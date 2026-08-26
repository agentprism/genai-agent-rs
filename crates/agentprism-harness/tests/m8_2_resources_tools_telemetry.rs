use agentprism_ai::{
    AssistantFinishReason, CancellationToken, ModelRef, RunId, SendBoxFuture, SendBoxStream,
    Timestamp, ToolResultContent, Usage, UsageSource,
};
use agentprism_env::{
    AgentFileSystem, AgentPath, ArtifactError, ArtifactRef, CanonicalPath, Clock, ClockError,
    EditResult, FileReadResult, FileSystemError, FileWriteResult, ProcessCommand, ProcessError,
    ProcessEvent, ProcessExitStatus, ProcessOutcome, ProcessSpawner, ProcessTermination,
    ReadLimits, RunningProcess, TemporaryArtifactRequest, TemporaryArtifactStore,
    TerminationPolicy,
};
use agentprism_harness::*;
use agentprism_session::{AppendReceipt, CompactionReason, LaneName, Sequence, SessionId};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use bytes::Bytes;
use futures_channel::oneshot;
use futures_executor::block_on;
use futures_util::{
    StreamExt,
    future::{join, join3},
    stream,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::symlink;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn valid_skill(source: &str) -> LoadedSkill {
    parse_skill_document(
        "/workspace/skills/example/SKILL.md",
        source,
        true,
        Vec::new(),
    )
    .skill
    .expect("fixture skill is valid")
}

// Pi basis: packages/agent/src/harness/tools/file-mutation-queue.ts and
// packages/agent/test/harness/tools.test.ts (same canonical path queueing).
#[test]
fn mutation_queue_same_path_serializes() {
    block_on(async {
        let queue = FileMutationQueue::new();
        let path = CanonicalPath::new("/workspace/same.txt");
        let log = Arc::new(Mutex::new(Vec::new()));
        let (first_started_tx, first_started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (second_started_tx, mut second_started_rx) = oneshot::channel();

        let first_log = Arc::clone(&log);
        let first = queue.with_path_lock(path.clone(), async move {
            lock(&first_log).push("first-start");
            let _ = first_started_tx.send(());
            let _ = release_rx.await;
            lock(&first_log).push("first-end");
        });
        let second_log = Arc::clone(&log);
        let second = queue.with_path_lock(path, async move {
            lock(&second_log).push("second-start");
            let _ = second_started_tx.send(());
        });
        let coordinator = async move {
            first_started_rx.await.expect("first operation starts");
            assert!(matches!(second_started_rx.try_recv(), Ok(None)));
            let _ = release_tx.send(());
        };

        join3(first, second, coordinator).await;
        assert_eq!(&*lock(&log), &["first-start", "first-end", "second-start"]);
        assert_eq!(queue.active_paths(), 0);
    });
}

// Pi basis: packages/agent/src/harness/tools/file-mutation-queue.ts.
#[test]
fn mutation_queue_different_paths_concurrent() {
    block_on(async {
        let queue = FileMutationQueue::new();
        let (left_started_tx, left_started_rx) = oneshot::channel();
        let (right_started_tx, right_started_rx) = oneshot::channel();
        let (left_release_tx, left_release_rx) = oneshot::channel();
        let (right_release_tx, right_release_rx) = oneshot::channel();

        let left = queue.with_path_lock(CanonicalPath::new("/workspace/left"), async move {
            let _ = left_started_tx.send(());
            let _ = left_release_rx.await;
        });
        let right = queue.with_path_lock(CanonicalPath::new("/workspace/right"), async move {
            let _ = right_started_tx.send(());
            let _ = right_release_rx.await;
        });
        let coordinator = async move {
            let (left_started, right_started) = join(left_started_rx, right_started_rx).await;
            left_started.expect("left operation starts");
            right_started.expect("right operation starts");
            let _ = left_release_tx.send(());
            let _ = right_release_tx.send(());
        };

        join3(left, right, coordinator).await;
        assert_eq!(queue.active_paths(), 0);
    });
}

#[derive(Default)]
struct MemoryFileSystem {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    aliases: Mutex<BTreeMap<String, String>>,
}

impl MemoryFileSystem {
    fn with_file(path: &str, content: &str) -> Self {
        Self {
            files: Mutex::new(BTreeMap::from([(
                path.to_owned(),
                content.as_bytes().to_vec(),
            )])),
            aliases: Mutex::new(BTreeMap::new()),
        }
    }

    fn with_bytes(path: &str, content: impl Into<Vec<u8>>) -> Self {
        Self {
            files: Mutex::new(BTreeMap::from([(path.to_owned(), content.into())])),
            aliases: Mutex::new(BTreeMap::new()),
        }
    }

    fn alias(&self, alias: &str, target: &str) {
        lock(&self.aliases).insert(alias.to_owned(), target.to_owned());
    }
}

impl AgentFileSystem for MemoryFileSystem {
    fn canonicalize(
        &self,
        path: &AgentPath,
    ) -> SendBoxFuture<'_, Result<CanonicalPath, FileSystemError>> {
        let path = path.clone();
        Box::pin(async move {
            let source = path.to_string();
            let canonical = lock(&self.aliases)
                .get(&source)
                .cloned()
                .unwrap_or_else(|| source.clone());
            if lock(&self.files).contains_key(&canonical)
                || lock(&self.aliases).contains_key(&source)
            {
                Ok(CanonicalPath::new(canonical))
            } else {
                Err(FileSystemError::NotFound {
                    path,
                    message: "not found".into(),
                })
            }
        })
    }

    fn read(
        &self,
        path: &AgentPath,
        _limits: ReadLimits,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileReadResult, FileSystemError>> {
        let path = path.clone();
        Box::pin(async move {
            let bytes = lock(&self.files)
                .get(&path.to_string())
                .cloned()
                .ok_or_else(|| FileSystemError::NotFound {
                    path: path.clone(),
                    message: "not found".into(),
                })?;
            Ok(FileReadResult {
                path,
                total_bytes: bytes.len() as u64,
                data: Bytes::from(bytes),
                truncated: false,
            })
        })
    }

    fn write(
        &self,
        path: &AgentPath,
        data: Bytes,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileWriteResult, FileSystemError>> {
        let path = path.clone();
        Box::pin(async move {
            let bytes_written = data.len() as u64;
            lock(&self.files).insert(path.to_string(), data.to_vec());
            Ok(FileWriteResult {
                path,
                bytes_written,
            })
        })
    }

    fn replace_exact(
        &self,
        path: &AgentPath,
        expected: &str,
        replacement: &str,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<EditResult, FileSystemError>> {
        let path = path.clone();
        let expected = expected.to_owned();
        let replacement = replacement.to_owned();
        Box::pin(async move {
            let mut files = lock(&self.files);
            let content =
                files
                    .get(&path.to_string())
                    .cloned()
                    .ok_or_else(|| FileSystemError::NotFound {
                        path: path.clone(),
                        message: "not found".into(),
                    })?;
            let content = String::from_utf8(content).expect("test content is UTF-8");
            let matches = content.match_indices(&expected).collect::<Vec<_>>();
            if matches.is_empty() {
                return Err(FileSystemError::ExactMatchNotFound { path });
            }
            if matches.len() > 1 {
                return Err(FileSystemError::MultipleExactMatches {
                    path,
                    matches: matches.len() as u64,
                });
            }
            if expected == replacement {
                return Err(FileSystemError::NoOpReplacement { path });
            }
            let changed = content.replacen(&expected, &replacement, 1);
            let bytes_written = changed.len() as u64;
            files.insert(path.to_string(), changed.into_bytes());
            Ok(EditResult {
                path,
                replacements: 1,
                bytes_written,
            })
        })
    }
}

fn tool_text(output: &agentprism_core::ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|content| match content {
            ToolResultContent::Text { text, .. } => Some(text.as_str()),
            ToolResultContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Default)]
struct RecordingImageProcessor {
    received: Mutex<Vec<(Vec<u8>, String, bool)>>,
}

impl ReadImageProcessor for RecordingImageProcessor {
    fn process(
        &self,
        bytes: Bytes,
        mime_type: &str,
        auto_resize_images: bool,
    ) -> SendBoxFuture<'_, Result<ProcessedReadImage, String>> {
        let mime_type = mime_type.to_owned();
        Box::pin(async move {
            lock(&self.received).push((bytes.to_vec(), mime_type, auto_resize_images));
            Ok(ProcessedReadImage {
                data: "converted".into(),
                mime_type: "image/png".into(),
                hints: vec!["[Image converted from image/bmp to image/png.]".into()],
            })
        })
    }
}

// Pi basis: packages/agent/test/harness/tools.test.ts (`read` describe block)
// and packages/agent/src/harness/tools/read.ts.
#[test]
fn read_tool_offsets_limits_truncation_and_images_pi_exact() {
    let lines = (1..=2_500)
        .map(|line| format!("Line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let filesystem = MemoryFileSystem::with_file("/large.txt", &lines);
    let ranged = block_on(read_tool(
        &filesystem,
        &AgentPath::new("/large.txt"),
        ReadToolRequest {
            offset: Some(41),
            limit: Some(20),
        },
        ReadToolOptions::default(),
        None,
        CancellationToken::new(),
    ))
    .expect("ranged text read succeeds");
    let ranged_text = tool_text(&ranged);
    assert!(!ranged_text.contains("Line 40\n"));
    assert!(ranged_text.contains("Line 41"));
    assert!(ranged_text.contains("Line 60"));
    assert!(!ranged_text.contains("Line 61"));
    assert!(ranged_text.contains("[2440 more lines in file. Use offset=61 to continue.]"));

    let truncated = block_on(read_tool(
        &filesystem,
        &AgentPath::new("/large.txt"),
        ReadToolRequest::default(),
        ReadToolOptions::default(),
        None,
        CancellationToken::new(),
    ))
    .expect("large text read succeeds");
    assert!(
        tool_text(&truncated)
            .contains("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]")
    );
    let details: ReadToolDetails = serde_json::from_str(
        truncated
            .details
            .as_deref()
            .expect("truncated read carries details")
            .get(),
    )
    .expect("read details decode");
    assert_eq!(details.schema_version, READ_TOOL_DETAILS_SCHEMA_VERSION);
    assert_eq!(details.truncation.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(details.truncation.total_lines, 2_500);
    assert_eq!(details.truncation.output_lines, 2_000);

    let exact =
        MemoryFileSystem::with_file("/exact.txt", &format!("{}\n", vec!["x"; 2_000].join("\n")));
    let exact_result = block_on(read_tool(
        &exact,
        &AgentPath::new("/exact.txt"),
        ReadToolRequest::default(),
        ReadToolOptions::default(),
        None,
        CancellationToken::new(),
    ))
    .expect("trailing newline at the exact line limit is accepted");
    assert!(exact_result.details.is_none());
    assert!(!tool_text(&exact_result).contains("Use offset="));

    let offset_error = block_on(read_tool(
        &MemoryFileSystem::with_file("/short.txt", "one\ntwo\nthree"),
        &AgentPath::new("/short.txt"),
        ReadToolRequest {
            offset: Some(100),
            limit: None,
        },
        ReadToolOptions::default(),
        None,
        CancellationToken::new(),
    ))
    .expect_err("offset beyond the file is rejected");
    assert!(
        offset_error
            .to_string()
            .contains("Offset 100 is beyond end of file (3 lines total)")
    );

    let png = BASE64_STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==")
        .expect("PNG fixture decodes");
    let image_filesystem = MemoryFileSystem::with_bytes("/image.txt", png.clone());
    let image = block_on(read_tool(
        &image_filesystem,
        &AgentPath::new("/image.txt"),
        ReadToolRequest::default(),
        ReadToolOptions::default(),
        None,
        CancellationToken::new(),
    ))
    .expect("magic-detected PNG read succeeds");
    assert!(tool_text(&image).contains("Read image file [image/png]"));
    assert!(image.content.iter().any(|content| matches!(
        content,
        ToolResultContent::Image { data, mime_type, .. }
            if data == &BASE64_STANDARD.encode(&png) && mime_type == "image/png"
    )));

    let mut bmp = vec![0_u8; 58];
    bmp[0..2].copy_from_slice(b"BM");
    let processor = RecordingImageProcessor::default();
    let converted = block_on(read_tool(
        &MemoryFileSystem::with_bytes("/image.bmp", bmp.clone()),
        &AgentPath::new("/image.bmp"),
        ReadToolRequest::default(),
        ReadToolOptions {
            auto_resize_images: false,
            ..ReadToolOptions::default()
        },
        Some(&processor),
        CancellationToken::new(),
    ))
    .expect("injected BMP conversion succeeds");
    assert_eq!(
        &*lock(&processor.received),
        &[(bmp, "image/bmp".into(), false)]
    );
    assert!(tool_text(&converted).contains("[Image converted from image/bmp to image/png.]"));
}

struct BlockingWriteFileSystem {
    inner: MemoryFileSystem,
    first_started: Mutex<Option<oneshot::Sender<()>>>,
    first_release: Mutex<Option<oneshot::Receiver<()>>>,
    second_started: AtomicBool,
}

impl BlockingWriteFileSystem {
    fn new() -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (started_sender, started_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = oneshot::channel();
        (
            Self {
                inner: MemoryFileSystem::default(),
                first_started: Mutex::new(Some(started_sender)),
                first_release: Mutex::new(Some(release_receiver)),
                second_started: AtomicBool::new(false),
            },
            started_receiver,
            release_sender,
        )
    }
}

impl AgentFileSystem for BlockingWriteFileSystem {
    fn canonicalize(
        &self,
        path: &AgentPath,
    ) -> SendBoxFuture<'_, Result<CanonicalPath, FileSystemError>> {
        self.inner.canonicalize(path)
    }

    fn read(
        &self,
        path: &AgentPath,
        limits: ReadLimits,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileReadResult, FileSystemError>> {
        self.inner.read(path, limits, cancellation)
    }

    fn write(
        &self,
        path: &AgentPath,
        data: Bytes,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileWriteResult, FileSystemError>> {
        let path = path.clone();
        Box::pin(async move {
            if data == Bytes::from_static(b"first\n") {
                if let Some(sender) = lock(&self.first_started).take() {
                    let _ = sender.send(());
                }
                let receiver = lock(&self.first_release).take();
                if let Some(receiver) = receiver {
                    let _ = receiver.await;
                }
            } else if data == Bytes::from_static(b"second\n") {
                self.second_started.store(true, Ordering::Release);
            }
            self.inner
                .write(&path, data, CancellationToken::new())
                .await
        })
    }

    fn replace_exact(
        &self,
        path: &AgentPath,
        expected: &str,
        replacement: &str,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<EditResult, FileSystemError>> {
        self.inner
            .replace_exact(path, expected, replacement, cancellation)
    }
}

// Pi basis: packages/agent/test/harness/tools.test.ts (`write` describe block)
// and packages/agent/src/harness/tools/write.ts.
#[test]
fn write_tool_creates_content_and_holds_lock_until_settled_pi_exact() {
    block_on(async {
        let (filesystem, first_started, first_release) = BlockingWriteFileSystem::new();
        let mutations = FileMutationQueue::new();
        let cancellation = CancellationToken::new();
        let path = AgentPath::new("/nested/dir/file.txt");
        let first = write_tool(
            &filesystem,
            &mutations,
            &path,
            "first\n",
            cancellation.clone(),
        );
        let second = write_tool(
            &filesystem,
            &mutations,
            &path,
            "second\n",
            CancellationToken::new(),
        );
        let coordinator = async {
            first_started.await.expect("first write begins");
            cancellation.cancel();
            assert!(!filesystem.second_started.load(Ordering::Acquire));
            let _ = first_release.send(());
        };
        let (first_result, second_result, ()) = join3(first, second, coordinator).await;
        assert!(matches!(
            first_result,
            Err(FileSystemError::Cancelled { .. })
        ));
        let second_result = second_result.expect("second write succeeds after first settles");
        assert_eq!(
            tool_text(&second_result),
            "Successfully wrote 7 bytes to /nested/dir/file.txt"
        );
        assert_eq!(
            lock(&filesystem.inner.files)["/nested/dir/file.txt"],
            b"second\n"
        );
    });
}

struct BlockingEditFileSystem {
    inner: MemoryFileSystem,
    first_started: Mutex<Option<oneshot::Sender<()>>>,
    first_release: Mutex<Option<oneshot::Receiver<()>>>,
    second_started: AtomicBool,
}

impl BlockingEditFileSystem {
    fn new() -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let inner = MemoryFileSystem::with_file("/target.txt", "alpha\nbeta\n");
        inner.alias("/link.txt", "/target.txt");
        let (started_sender, started_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = oneshot::channel();
        (
            Self {
                inner,
                first_started: Mutex::new(Some(started_sender)),
                first_release: Mutex::new(Some(release_receiver)),
                second_started: AtomicBool::new(false),
            },
            started_receiver,
            release_sender,
        )
    }
}

impl AgentFileSystem for BlockingEditFileSystem {
    fn canonicalize(
        &self,
        path: &AgentPath,
    ) -> SendBoxFuture<'_, Result<CanonicalPath, FileSystemError>> {
        self.inner.canonicalize(path)
    }

    fn read(
        &self,
        path: &AgentPath,
        limits: ReadLimits,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileReadResult, FileSystemError>> {
        self.inner.read(path, limits, cancellation)
    }

    fn write(
        &self,
        path: &AgentPath,
        data: Bytes,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileWriteResult, FileSystemError>> {
        self.inner.write(path, data, cancellation)
    }

    fn replace_exact(
        &self,
        path: &AgentPath,
        expected: &str,
        replacement: &str,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<EditResult, FileSystemError>> {
        let path = path.clone();
        let expected = expected.to_owned();
        let replacement = replacement.to_owned();
        Box::pin(async move {
            if replacement.contains("ALPHA") {
                if let Some(sender) = lock(&self.first_started).take() {
                    let _ = sender.send(());
                }
                let receiver = lock(&self.first_release).take();
                if let Some(receiver) = receiver {
                    let _ = receiver.await;
                }
            } else if replacement.contains("BETA") {
                self.second_started.store(true, Ordering::Release);
            }
            self.inner
                .replace_exact(&path, &expected, &replacement, CancellationToken::new())
                .await
        })
    }
}

// Architecture v2 part 2 §7.11/§10.10; Pi basis:
// packages/agent/test/harness/tools.test.ts (`keeps the mutation queue locked
// until an aborted edit write settles` and canonical/symlink serialization).
#[test]
fn edit_tool_holds_canonical_lock_until_aborted_write_settles_pi_exact() {
    block_on(async {
        let (filesystem, first_started, first_release) = BlockingEditFileSystem::new();
        let mutations = FileMutationQueue::new();
        let cancellation = CancellationToken::new();
        let link_path = AgentPath::new("/link.txt");
        let target_path = AgentPath::new("/target.txt");
        let first = edit_file_exact(
            &filesystem,
            &mutations,
            &link_path,
            "alpha",
            "ALPHA",
            cancellation.clone(),
        );
        let second = edit_file_exact(
            &filesystem,
            &mutations,
            &target_path,
            "beta",
            "BETA",
            CancellationToken::new(),
        );
        let coordinator = async {
            first_started.await.expect("first edit write begins");
            cancellation.cancel();
            assert!(!filesystem.second_started.load(Ordering::Acquire));
            let _ = first_release.send(());
        };
        let (first_result, second_result, ()) = join3(first, second, coordinator).await;
        assert!(matches!(
            first_result,
            Err(FileSystemError::Cancelled { .. })
        ));
        second_result.expect("second edit starts only after the first write settles");
        assert_eq!(
            String::from_utf8(lock(&filesystem.inner.files)["/target.txt"].clone()).expect("UTF-8"),
            "ALPHA\nBETA\n"
        );
    });
}

// Pi basis: packages/agent/src/harness/tools/edit-diff.ts (`getNotFoundError`).
#[test]
fn edit_requires_exact_match() {
    let filesystem = MemoryFileSystem::with_file("/file", "alpha\nbeta\n");
    let error = block_on(edit_file_exact(
        &filesystem,
        &FileMutationQueue::new(),
        &AgentPath::new("/file"),
        "Alpha",
        "ALPHA",
        CancellationToken::new(),
    ))
    .expect_err("case-different text is not an exact match");
    assert!(matches!(error, FileSystemError::ExactMatchNotFound { .. }));

    let filesystem = MemoryFileSystem::with_file(
        "/fuzzy",
        "\u{feff}alpha  \r\n“smart” — gap\u{00a0}here\r\nuntouched  \r\nomega\r\n",
    );
    block_on(edit_file(
        &filesystem,
        &FileMutationQueue::new(),
        &AgentPath::new("/fuzzy"),
        &[
            EditReplacement::new("alpha\n", "ALPHA\n"),
            EditReplacement::new("\"smart\" - gap here", "plain"),
        ],
        CancellationToken::new(),
    ))
    .expect("Pi fuzzy normalization matches both original-file regions");
    assert_eq!(
        String::from_utf8(lock(&filesystem.files)["/fuzzy"].clone()).expect("UTF-8"),
        "\u{feff}ALPHA\r\nplain\r\nuntouched  \r\nomega\r\n"
    );
}

// Pi basis: packages/agent/src/harness/tools/edit-diff.ts (`getDuplicateError`).
#[test]
fn edit_rejects_multiple_matches() {
    let filesystem = MemoryFileSystem::with_file("/file", "same\nsame\n");
    let error = block_on(edit_file_exact(
        &filesystem,
        &FileMutationQueue::new(),
        &AgentPath::new("/file"),
        "same",
        "changed",
        CancellationToken::new(),
    ))
    .expect_err("ambiguous replacement is rejected");
    assert!(matches!(
        error,
        FileSystemError::MultipleExactMatches { matches: 2, .. }
    ));

    let filesystem = MemoryFileSystem::with_file("/disjoint", "one\ntwo\nthree\n");
    let details = block_on(edit_file(
        &filesystem,
        &FileMutationQueue::new(),
        &AgentPath::new("/disjoint"),
        &[
            EditReplacement::new("one", "ONE"),
            EditReplacement::new("three", "THREE"),
        ],
        CancellationToken::new(),
    ))
    .expect("disjoint edits matched against the original are applied together");
    assert_eq!(details.diff.hunks.len(), 2);
    assert_eq!(
        String::from_utf8(lock(&filesystem.files)["/disjoint"].clone()).expect("UTF-8"),
        "ONE\ntwo\nTHREE\n"
    );

    let filesystem = MemoryFileSystem::with_file("/overlap", "one\ntwo\nthree\n");
    let error = block_on(edit_file(
        &filesystem,
        &FileMutationQueue::new(),
        &AgentPath::new("/overlap"),
        &[
            EditReplacement::new("one\ntwo", "ONE\nTWO"),
            EditReplacement::new("two\nthree", "TWO\nTHREE"),
        ],
        CancellationToken::new(),
    ))
    .expect_err("overlapping original-file ranges are rejected");
    assert!(matches!(
        error,
        FileSystemError::Invalid { message, .. } if message.contains("overlap")
    ));
}

// Pi basis: packages/agent/src/harness/tools/edit-diff.ts (`getNoChangeError`).
#[test]
fn edit_rejects_noop() {
    let filesystem = MemoryFileSystem::with_file("/file", "same\n");
    let error = block_on(edit_file_exact(
        &filesystem,
        &FileMutationQueue::new(),
        &AgentPath::new("/file"),
        "same",
        "same",
        CancellationToken::new(),
    ))
    .expect_err("no-op replacement is rejected");
    assert!(matches!(error, FileSystemError::NoOpReplacement { .. }));
}

// Pi basis: packages/agent/test/harness/tools.test.ts (`edit` describe block)
// and packages/agent/src/harness/tools/edit.ts.
#[test]
fn edit_tool_disjoint_original_symlink_bom_crlf_and_overlap_pi_exact() {
    let filesystem =
        MemoryFileSystem::with_file("/target.txt", "\u{feff}alpha\r\nbeta\r\ngamma\r\ndelta\r\n");
    filesystem.alias("/link.txt", "/target.txt");
    let result = block_on(edit_file(
        &filesystem,
        &FileMutationQueue::new(),
        &AgentPath::new("/link.txt"),
        &[
            EditReplacement::new("alpha\n", "ALPHA\n"),
            EditReplacement::new("gamma\n", "GAMMA\n"),
        ],
        CancellationToken::new(),
    ))
    .expect("disjoint edits through a canonicalized alias succeed");
    assert_eq!(result.diff.path, "/target.txt");
    assert!(!result.diff.hunks.is_empty());
    assert_eq!(
        String::from_utf8(lock(&filesystem.files)["/target.txt"].clone()).expect("UTF-8"),
        "\u{feff}ALPHA\r\nbeta\r\nGAMMA\r\ndelta\r\n"
    );

    let overlap = block_on(edit_file(
        &filesystem,
        &FileMutationQueue::new(),
        &AgentPath::new("/target.txt"),
        &[
            EditReplacement::new("ALPHA\nbeta", "one"),
            EditReplacement::new("beta\nGAMMA", "two"),
        ],
        CancellationToken::new(),
    ))
    .expect_err("overlapping regions matched against the original are rejected");
    assert!(matches!(
        overlap,
        FileSystemError::Invalid { message, .. } if message.contains("overlap")
    ));
    assert_eq!(
        String::from_utf8(lock(&filesystem.files)["/target.txt"].clone()).expect("UTF-8"),
        "\u{feff}ALPHA\r\nbeta\r\nGAMMA\r\ndelta\r\n"
    );
}

// Pi basis: packages/agent/src/harness/utils/truncate.ts and truncate.test.ts.
#[test]
fn truncate_never_splits_utf8() {
    let result = truncate(
        "aé🙂b",
        TruncationLimits {
            max_bytes: 5,
            max_lines: 10,
            strategy: TruncationStrategy::Tail,
        },
    );
    assert_eq!(result.content, "🙂b");
    assert_eq!(result.output_bytes, 5);
}

// Pi basis: packages/agent/src/harness/utils/truncate.ts (`truncateHead`).
#[test]
fn truncate_respects_byte_limit() {
    let result = truncate(
        "éé\nabc",
        TruncationLimits {
            max_bytes: 4,
            max_lines: 10,
            strategy: TruncationStrategy::Head,
        },
    );
    assert_eq!(result.content, "éé");
    assert_eq!(result.output_bytes, 4);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
}

// Pi basis: packages/agent/src/harness/utils/truncate.ts (`truncateHead`).
#[test]
fn truncate_respects_line_limit() {
    let result = truncate(
        "one\ntwo\nthree\n",
        TruncationLimits {
            max_bytes: 1_024,
            max_lines: 2,
            strategy: TruncationStrategy::Head,
        },
    );
    assert_eq!(result.content, "one\ntwo");
    assert_eq!(result.output_lines, 2);
    assert_eq!(result.total_lines, 3);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
}

// Architecture v2 part 2 §7.10/§10.10; Pi basis:
// packages/agent/test/harness/truncate.test.ts.
#[test]
fn truncate_head_tail_metadata_edge_cases_pi_exact() {
    let limits = |max_bytes, strategy| TruncationLimits {
        max_bytes,
        max_lines: 10,
        strategy,
    };
    let complete = truncate("line\nline\nline\n", limits(100, TruncationStrategy::Head));
    assert_eq!(complete.total_lines, 3);
    assert_eq!(complete.output_lines, 3);
    assert!(!complete.truncated);

    let oversized_head = truncate("éé\nabc", limits(3, TruncationStrategy::Head));
    assert!(oversized_head.content.is_empty());
    assert!(oversized_head.first_line_exceeds_limit);

    let partial_tail = truncate("aé🙂b", limits(5, TruncationStrategy::Tail));
    assert_eq!(partial_tail.content, "🙂b");
    assert!(partial_tail.last_line_partial);

    let dropped_tail = truncate("abc🙂", limits(3, TruncationStrategy::Tail));
    assert!(dropped_tail.content.is_empty());
    assert!(dropped_tail.last_line_partial);

    let huge = format!("{}\n", "X".repeat(300_000));
    let huge_tail = truncate(
        &huge,
        TruncationLimits {
            max_bytes: 1_024,
            max_lines: 100,
            strategy: TruncationStrategy::Tail,
        },
    );
    assert_eq!(huge_tail.content, "X".repeat(1_024));
    assert_eq!(huge_tail.total_bytes, 300_001);
    assert_eq!(huge_tail.output_bytes, 1_024);
}

// Architecture v2 part 2 §7.10/§10.10; Pi basis:
// packages/agent/test/harness/truncate.test.ts deterministic fuzz corpus.
#[test]
fn truncate_tail_matches_utf8_suffix_across_deterministic_cases_pi_exact() {
    let alphabet = [
        'a', '\u{7f}', '\u{80}', 'é', '\u{7ff}', '\u{800}', '中', '🙂', '\u{e000}',
    ];
    let mut cases = vec![String::new()];
    for _ in 0..3 {
        let previous = cases.clone();
        cases.extend(previous.iter().flat_map(|prefix| {
            alphabet.iter().map(move |character| {
                let mut value = prefix.clone();
                value.push(*character);
                value
            })
        }));
    }

    for input in cases {
        for max_bytes in 0..=input.len().saturating_add(2) {
            let result = truncate(
                &input,
                TruncationLimits {
                    max_bytes,
                    max_lines: 10,
                    strategy: TruncationStrategy::Tail,
                },
            );
            let minimum = input.len().saturating_sub(max_bytes);
            let start = input
                .char_indices()
                .map(|(index, _)| index)
                .find(|index| *index >= minimum)
                .unwrap_or(input.len());
            assert_eq!(result.content, input[start..]);
            assert!(result.content.len() <= max_bytes);
        }
    }
}

#[derive(Default)]
struct MemoryArtifacts {
    created: Mutex<Vec<(TemporaryArtifactRequest, Bytes)>>,
}

#[derive(Clone)]
struct ProcessScript {
    events: Vec<Result<ProcessEvent, ProcessError>>,
    pending_after_events: bool,
    termination: ProcessOutcome,
}

struct ScriptedProcess {
    script: ProcessScript,
}

impl RunningProcess for ScriptedProcess {
    fn events(&mut self) -> SendBoxStream<'_, Result<ProcessEvent, ProcessError>> {
        let events = std::mem::take(&mut self.script.events);
        if self.script.pending_after_events {
            Box::pin(stream::iter(events).chain(stream::pending()))
        } else {
            Box::pin(stream::iter(events))
        }
    }

    fn terminate(
        &mut self,
        _policy: TerminationPolicy,
    ) -> SendBoxFuture<'_, Result<ProcessOutcome, ProcessError>> {
        let outcome = self.script.termination.clone();
        Box::pin(async move { Ok(outcome) })
    }
}

#[derive(Default)]
struct ScriptedProcessSpawner {
    commands: Mutex<Vec<ProcessCommand>>,
    scripts: Mutex<VecDeque<ProcessScript>>,
}

impl ScriptedProcessSpawner {
    fn new(scripts: impl IntoIterator<Item = ProcessScript>) -> Self {
        Self {
            commands: Mutex::new(Vec::new()),
            scripts: Mutex::new(scripts.into_iter().collect()),
        }
    }
}

impl ProcessSpawner for ScriptedProcessSpawner {
    fn spawn(
        &self,
        command: ProcessCommand,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RunningProcess>, ProcessError>> {
        Box::pin(async move {
            lock(&self.commands).push(command);
            let script = lock(&self.scripts)
                .pop_front()
                .expect("a process script is available");
            Ok(Box::new(ScriptedProcess { script }) as Box<dyn RunningProcess>)
        })
    }
}

struct YieldOnceClock;

impl Clock for YieldOnceClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(0)
    }

    fn sleep(
        &self,
        _duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), ClockError>> {
        let mut yielded = false;
        Box::pin(futures_util::future::poll_fn(move |context| {
            if cancellation.is_cancelled() {
                return std::task::Poll::Ready(Err(ClockError::Cancelled));
            }
            if yielded {
                std::task::Poll::Ready(Ok(()))
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }))
    }
}

fn process_outcome(code: i32, success: bool, termination: ProcessTermination) -> ProcessOutcome {
    ProcessOutcome {
        status: ProcessExitStatus {
            code: Some(code),
            signal: None,
            success,
        },
        termination,
    }
}

#[derive(Debug)]
struct BashTurnContext {
    workspace: AgentPath,
}

struct AsyncBashPrepare {
    expected_context: Arc<BashTurnContext>,
    saw_signal: AtomicBool,
}

impl BashPrepare<BashTurnContext> for AsyncBashPrepare {
    fn prepare<'a>(
        &'a self,
        execution: &'a mut BashExecutionRequest,
        context: &'a BashTurnContext,
        cancellation: &'a CancellationToken,
    ) -> SendBoxFuture<'a, Result<(), BashPrepareError>> {
        Box::pin(async move {
            let mut yielded = false;
            futures_util::future::poll_fn(move |task| {
                if yielded {
                    std::task::Poll::Ready(())
                } else {
                    yielded = true;
                    task.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;

            assert!(std::ptr::eq(context, self.expected_context.as_ref()));
            assert!(!cancellation.is_cancelled());
            self.saw_signal.store(true, Ordering::SeqCst);
            assert_eq!(
                execution.command, "prefix=ready\nprintf original",
                "command prefix is applied before prepare"
            );
            execution
                .command
                .push_str("\nprintf \"$prefix:$INHERITED:$EXPLICIT:$PWD\"");
            execution.current_dir = Some(context.workspace.clone());
            execution.environment = BTreeMap::from([("EXPLICIT".into(), "explicit".into())]);
            execution.inherit_environment = false;
            Ok(())
        })
    }
}

struct CancellingBashPrepare {
    signal: CancellationToken,
    saw_shared_signal: AtomicBool,
}

impl BashPrepare<BashTurnContext> for CancellingBashPrepare {
    fn prepare<'a>(
        &'a self,
        _execution: &'a mut BashExecutionRequest,
        _context: &'a BashTurnContext,
        cancellation: &'a CancellationToken,
    ) -> SendBoxFuture<'a, Result<(), BashPrepareError>> {
        Box::pin(async move {
            self.signal.cancel();
            self.saw_shared_signal
                .store(cancellation.is_cancelled(), Ordering::SeqCst);
            Ok(())
        })
    }
}

impl TemporaryArtifactStore for MemoryArtifacts {
    fn create(
        &self,
        request: TemporaryArtifactRequest,
        data: Bytes,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ArtifactRef, ArtifactError>> {
        Box::pin(async move {
            lock(&self.created).push((request, data));
            Ok(ArtifactRef {
                path: CanonicalPath::new("/artifacts/bash-1.log"),
            })
        })
    }

    fn remove(
        &self,
        _artifact: &ArtifactRef,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), ArtifactError>> {
        Box::pin(async { Ok(()) })
    }
}

// Pi basis: packages/agent/src/harness/tools/bash.ts, utils/shell-output.ts,
// and packages/agent/test/harness/tools.test.ts (`persists truncated full output`).
#[test]
fn bash_truncated_output_has_full_artifact() {
    let artifacts = MemoryArtifacts::default();
    let output = "one\ntwo\nthree\n";
    let result = block_on(prepare_bash_tool_result(
        &artifacts,
        output,
        &ProcessExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        },
        TruncationLimits {
            max_bytes: 1_024,
            max_lines: 2,
            strategy: TruncationStrategy::Head,
        },
        CancellationToken::new(),
    ))
    .expect("artifact creation succeeds");
    assert_eq!(result.output, "two\nthree");
    assert!(result.details.truncated);
    assert_eq!(
        result
            .details
            .full_output_artifact
            .as_ref()
            .expect("truncated output has an artifact")
            .path
            .to_string(),
        "/artifacts/bash-1.log"
    );
    assert_eq!(
        lock(&artifacts.created)[0].1,
        Bytes::from_static(output.as_bytes())
    );
}

/// Architecture v2 part 2 §7.11, §9.5, and §10.10; pinned Pi basis:
/// `packages/agent/test/harness/tools.test.ts` (`supports command prefixes and
/// prepare hook`) and `packages/agent/src/harness/tools/bash.ts`.
#[test]
fn bash_prepare_hook_receives_turn_context_signal_and_orders_mutations_pi_exact() {
    let success = process_outcome(0, true, ProcessTermination::Exited);
    let processes = ScriptedProcessSpawner::new([ProcessScript {
        events: vec![
            Ok(ProcessEvent::Stdout(Bytes::from_static(
                b"ready::explicit:/workspace",
            ))),
            Ok(ProcessEvent::Exited(success.clone())),
        ],
        pending_after_events: false,
        termination: success,
    }]);
    let context = Arc::new(BashTurnContext {
        workspace: AgentPath::new("/workspace"),
    });
    let prepare = AsyncBashPrepare {
        expected_context: Arc::clone(&context),
        saw_signal: AtomicBool::new(false),
    };
    let mut request = BashExecutionRequest::new("printf original");
    request.command_prefix = Some("prefix=ready".into());

    let result = block_on(execute_bash_tool_with_prepare(
        &processes,
        &YieldOnceClock,
        &MemoryArtifacts::default(),
        &request,
        context.as_ref(),
        &prepare,
        CancellationToken::new(),
    ))
    .expect("prepared command succeeds");

    assert_eq!(result.result.output, "ready::explicit:/workspace");
    assert!(prepare.saw_signal.load(Ordering::SeqCst));
    let command = &lock(&processes.commands)[0];
    assert_eq!(
        command.arguments,
        [
            "-lc",
            "prefix=ready\nprintf original\nprintf \"$prefix:$INHERITED:$EXPLICIT:$PWD\""
        ]
    );
    assert_eq!(command.current_dir, Some(AgentPath::new("/workspace")));
    assert_eq!(
        command.environment,
        BTreeMap::from([("EXPLICIT".into(), "explicit".into())])
    );
    assert!(!command.inherit_environment);
}

/// Architecture v2 part 2 §7.11, §9.5, and §10.10; pinned Pi basis:
/// `packages/agent/test/harness/tools.test.ts` and the AbortSignal passed by
/// `packages/agent/src/harness/tools/bash.ts`.
#[test]
fn bash_prepare_hook_observes_logical_cancellation_before_spawn_pi_exact() {
    let processes = ScriptedProcessSpawner::default();
    let context = BashTurnContext {
        workspace: AgentPath::new("/workspace"),
    };
    let cancellation = CancellationToken::new();
    let prepare = CancellingBashPrepare {
        signal: cancellation.clone(),
        saw_shared_signal: AtomicBool::new(false),
    };
    let error = block_on(execute_bash_tool_with_prepare(
        &processes,
        &YieldOnceClock,
        &MemoryArtifacts::default(),
        &BashExecutionRequest::new("must not spawn"),
        &context,
        &prepare,
        cancellation,
    ))
    .expect_err("prepare-triggered cancellation prevents spawn");

    assert!(prepare.saw_shared_signal.load(Ordering::SeqCst));
    assert!(matches!(
        error,
        BashExecutionError::Process(ProcessError::Cancelled)
    ));
    assert!(lock(&processes.commands).is_empty());
}

// Pi basis: packages/agent/test/harness/tools.test.ts (`bash` describe block)
// and packages/agent/src/harness/tools/bash.ts.
#[test]
fn bash_tool_combines_streams_and_reports_failures_pi_exact() {
    let success = process_outcome(0, true, ProcessTermination::Exited);
    let failure = process_outcome(7, false, ProcessTermination::Exited);
    let processes = ScriptedProcessSpawner::new([
        ProcessScript {
            events: vec![
                Ok(ProcessEvent::Stdout(Bytes::from_static(b"out"))),
                Ok(ProcessEvent::Stderr(Bytes::from_static(b"err"))),
                Ok(ProcessEvent::Exited(success.clone())),
            ],
            pending_after_events: false,
            termination: success,
        },
        ProcessScript {
            events: vec![
                Ok(ProcessEvent::Stdout(Bytes::from_static(b"failed"))),
                Ok(ProcessEvent::Exited(failure.clone())),
            ],
            pending_after_events: false,
            termination: failure,
        },
    ]);
    let artifacts = MemoryArtifacts::default();
    let mut request = BashExecutionRequest::new("printf command");
    request.command_prefix = Some("printf prefix".into());
    request.current_dir = Some(AgentPath::new("/workspace"));
    request.environment = BTreeMap::from([("EXPLICIT".into(), "yes".into())]);
    request.inherit_environment = false;

    let result = block_on(execute_bash_tool(
        &processes,
        &YieldOnceClock,
        &artifacts,
        &request,
        CancellationToken::new(),
    ))
    .expect("successful scripted command completes");
    assert_eq!(result.result.output, "outerr");
    assert_eq!(
        result.result.details.schema_version,
        BASH_TOOL_DETAILS_SCHEMA_VERSION
    );
    let recorded = lock(&processes.commands)[0].clone();
    assert_eq!(recorded.program, "bash");
    assert_eq!(recorded.arguments, ["-lc", "printf prefix\nprintf command"]);
    assert_eq!(recorded.current_dir, Some(AgentPath::new("/workspace")));
    assert_eq!(recorded.environment, request.environment);
    assert!(!recorded.inherit_environment);

    let error = block_on(execute_bash_tool(
        &processes,
        &YieldOnceClock,
        &artifacts,
        &BashExecutionRequest::new("exit 7"),
        CancellationToken::new(),
    ))
    .expect_err("nonzero status is a tool failure");
    assert_eq!(error.to_string(), "failed\n\nCommand exited with code 7");
}

// Pi basis: packages/agent/test/harness/tools.test.ts (`reports nonzero exits
// and timeouts`, `preserves truncated output when a command times out`, and
// `reports the total size of an oversized final line`).
#[test]
fn bash_tool_timeout_preserves_truncated_output_and_long_line_size_pi_exact() {
    let forced = process_outcome(143, false, ProcessTermination::Forced);
    let many_lines = format!(
        "{}\n",
        (1..=DEFAULT_TOOL_MAX_LINES + 1)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let processes = ScriptedProcessSpawner::new([
        ProcessScript {
            events: vec![Ok(ProcessEvent::Stdout(Bytes::from(many_lines.clone())))],
            pending_after_events: true,
            termination: forced.clone(),
        },
        ProcessScript {
            events: vec![Ok(ProcessEvent::Stdout(Bytes::from(vec![b'0'; 60_000])))],
            pending_after_events: true,
            termination: forced,
        },
    ]);
    let artifacts = MemoryArtifacts::default();
    let mut request = BashExecutionRequest::new("chatty");
    request.timeout = Some(Duration::from_millis(50));
    let error = block_on(execute_bash_tool(
        &processes,
        &YieldOnceClock,
        &artifacts,
        &request,
        CancellationToken::new(),
    ))
    .expect_err("deadline is reported after output recovery");
    let message = error.to_string();
    assert!(message.contains("line-2001"));
    assert!(message.contains("Full output: /artifacts/bash-1.log"));
    assert!(message.contains("Command timed out after 0.05 seconds"));
    assert_eq!(lock(&artifacts.created)[0].1, Bytes::from(many_lines));

    let mut long_line_request = BashExecutionRequest::new("long-line");
    long_line_request.timeout = Some(Duration::from_millis(50));
    let long_line_error = block_on(execute_bash_tool(
        &processes,
        &YieldOnceClock,
        &artifacts,
        &long_line_request,
        CancellationToken::new(),
    ))
    .expect_err("long line command times out after retaining its output");
    assert!(
        long_line_error
            .to_string()
            .contains("Showing last 50.0KB of line 1 (line is 58.6KB). Full output:")
    );
}

#[derive(Default)]
struct RecordingBashUpdates {
    updates: Mutex<Vec<BashToolResult>>,
}

impl BashUpdateSink for RecordingBashUpdates {
    fn update(&self, update: BashToolResult) {
        lock(&self.updates).push(update);
    }
}

// Architecture v2 part 2 §7.11/§10.10; Pi basis:
// packages/agent/test/harness/tools.test.ts (`ignores output callbacks after
// execution settles` and `coalesces updates and persists truncated full output`).
#[test]
fn bash_tool_updates_coalesce_and_stop_after_settlement_pi_exact() {
    let success = process_outcome(0, true, ProcessTermination::Exited);
    let mut events = (1..=3_000)
        .map(|line| Ok(ProcessEvent::Stdout(Bytes::from(format!("line-{line}\n")))))
        .collect::<Vec<_>>();
    events.push(Ok(ProcessEvent::Exited(success.clone())));
    events.push(Ok(ProcessEvent::Stdout(Bytes::from_static(b"late\n"))));
    let processes = ScriptedProcessSpawner::new([ProcessScript {
        events,
        pending_after_events: false,
        termination: success,
    }]);
    let artifacts = MemoryArtifacts::default();
    let updates = RecordingBashUpdates::default();
    let execution = block_on(execute_bash_tool_with_updates(
        &processes,
        &YieldOnceClock,
        &artifacts,
        &BashExecutionRequest::new("many-lines"),
        Some(&updates),
        CancellationToken::new(),
    ))
    .expect("scripted command succeeds");
    let updates = lock(&updates.updates);
    assert!(!updates.is_empty());
    assert!(updates.len() < 25);
    assert!(updates.iter().all(|update| !update.output.contains("late")));
    let final_update = updates.last().expect("final update");
    assert_eq!(final_update, &execution.result);
    assert_eq!(final_update.truncation.total_lines, 3_000);
    assert_eq!(final_update.truncation.output_lines, 2_000);
    assert_eq!(
        final_update.details.full_output_artifact,
        Some(ArtifactRef {
            path: CanonicalPath::new("/artifacts/bash-1.log")
        })
    );
    let complete = &lock(&artifacts.created)[0].1;
    assert!(complete.starts_with(b"line-1\nline-2\n"));
    assert!(complete.ends_with(b"line-2999\nline-3000\n"));
}

// Pi basis: packages/agent/src/harness/skills.ts and skills.test.ts.
#[test]
fn skill_catalog_discovers_valid_skills() {
    fn object_safe(_: &dyn SkillCatalog, _: &dyn LocalSkillCatalog) {}

    let temporary = tempfile::tempdir().expect("temporary skill fixture root");
    let root = temporary.path().join("skills");
    fs::create_dir_all(root.join("alpha")).expect("alpha directory");
    fs::create_dir_all(root.join("zeta")).expect("zeta directory");
    fs::create_dir_all(root.join("ignored")).expect("ignored directory");
    fs::create_dir_all(root.join("fdignored")).expect("fdignored directory");
    fs::create_dir_all(root.join("nested")).expect("nested docs directory");
    fs::create_dir_all(root.join("broken")).expect("broken skill directory");
    fs::write(
        root.join("alpha/SKILL.md"),
        "---\nname: alpha\ndescription: Alpha skill\ndisable-model-invocation: true\n---\nUse alpha.\n",
    )
    .expect("alpha skill");
    fs::write(
        root.join("zeta/SKILL.md"),
        "---\nname: zeta\ndescription: Zeta skill\n---\nUse zeta.\n",
    )
    .expect("zeta skill");
    fs::write(
        root.join("ignored/SKILL.md"),
        "---\nname: ignored\ndescription: Ignored\n---\nIgnored.",
    )
    .expect("ignored skill");
    fs::write(
        root.join("fdignored/SKILL.md"),
        "---\nname: fdignored\ndescription: Ignored\n---\nIgnored.",
    )
    .expect("fdignored skill");
    fs::write(
        root.join("broken/SKILL.md"),
        "---\nname: [unterminated\n---\nBroken.",
    )
    .expect("broken skill");
    fs::write(
        root.join("root.md"),
        "---\ndescription: Root skill\n---\nUse root.",
    )
    .expect("root markdown skill");
    fs::write(
        root.join("root-ignored.md"),
        "---\ndescription: Ignored root skill\n---\nIgnored.",
    )
    .expect("ignored root markdown");
    fs::write(
        root.join("nested/ignored.md"),
        "---\ndescription: Nested markdown is not a skill\n---\nIgnored.",
    )
    .expect("nested markdown");
    fs::write(root.join("README.md"), "# Documentation").expect("root docs");
    fs::write(root.join(".gitignore"), "ignored/\n").expect("gitignore");
    fs::write(root.join(".ignore"), "root-ignored.md\n").expect("ignore");
    fs::write(root.join(".fdignore"), "fdignored/\n").expect("fdignore");

    let linked_target = temporary.path().join("linked-target/linked");
    fs::create_dir_all(&linked_target).expect("linked target");
    fs::write(
        linked_target.join("SKILL.md"),
        "---\nname: linked\ndescription: Linked skill\n---\nUse linked.",
    )
    .expect("linked skill");
    #[cfg(unix)]
    symlink(&linked_target, root.join("linked")).expect("skill directory symlink");
    #[cfg(not(unix))]
    {
        fs::create_dir_all(root.join("linked")).expect("linked fixture directory");
        fs::copy(linked_target.join("SKILL.md"), root.join("linked/SKILL.md"))
            .expect("linked fixture skill");
    }

    let catalog = NativeSkillCatalog::discover([&root]).expect("skill discovery completes");
    object_safe(&catalog, &catalog);
    let descriptors = block_on(catalog.list()).expect("catalog lists");
    assert_eq!(
        descriptors
            .iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "linked", "skills", "zeta"]
    );
    assert_eq!(
        descriptors,
        block_on(catalog.list()).expect("stable second listing")
    );
    assert!(descriptors[0].disable_model_invocation);
    assert!(catalog.diagnostics().iter().any(|diagnostic| {
        diagnostic.code == SkillDiagnosticCode::ParseFailed
            && diagnostic.path.ends_with("broken/SKILL.md")
    }));

    let skill = block_on(catalog.load(&SkillId::new("alpha"))).expect("alpha loads");
    assert_eq!(
        format_skill_invocation(&skill, Some("Check errors.")),
        format!(
            "<skill name=\"alpha\" location=\"{}\">\nReferences are relative to {}.\n\nUse alpha.\n</skill>\n\nCheck errors.",
            root.join("alpha/SKILL.md").display(),
            root.join("alpha").display()
        )
    );
}

// Pi basis: packages/agent/src/harness/skills.ts (`loadSkills` and
// `loadSkillsFromDirInternal`) preserves caller root order while sorting each root.
#[test]
fn skill_catalog_preserves_multi_root_order() {
    let temporary = tempfile::tempdir().expect("temporary skill fixture root");
    let first_root = temporary.path().join("first-root");
    let second_root = temporary.path().join("second-root");
    for (root, skills) in [
        (&first_root, ["beta", "zeta"]),
        (&second_root, ["alpha", "gamma"]),
    ] {
        for name in skills {
            let directory = root.join(name);
            fs::create_dir_all(&directory).expect("skill directory");
            fs::write(
                directory.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\nUse {name}."),
            )
            .expect("skill source");
        }
    }

    let catalog = NativeSkillCatalog::discover([&first_root, &second_root])
        .expect("multi-root skill discovery completes");
    let names = block_on(catalog.list())
        .expect("catalog lists")
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["beta", "zeta", "alpha", "gamma"]);
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TestSkillSource {
    User,
}

// Pi basis: packages/agent/test/harness/skills.test.ts (`preserves source info
// for sourced skills` and `attaches source info to diagnostics`).
#[test]
fn skill_catalog_preserves_opaque_source_on_skills_and_diagnostics_pi_exact() {
    let temporary = tempfile::tempdir().expect("temporary sourced-skill fixture root");
    let root = temporary.path().join("user");
    fs::create_dir_all(root.join("example")).expect("valid skill directory");
    fs::create_dir_all(root.join("broken")).expect("broken skill directory");
    fs::write(
        root.join("example/SKILL.md"),
        "---\nname: example\ndescription: Example skill\n---\nUse this skill.",
    )
    .expect("valid sourced skill");
    fs::write(
        root.join("broken/SKILL.md"),
        "---\nname: broken\n---\nMissing description.",
    )
    .expect("invalid sourced skill");

    let discovered = discover_sourced_skills([SourcedSkillRoot {
        path: root,
        source: TestSkillSource::User,
    }])
    .expect("sourced discovery completes");
    assert_eq!(discovered.skills.len(), 1);
    assert_eq!(discovered.skills[0].skill.descriptor.name, "example");
    assert_eq!(discovered.skills[0].source, TestSkillSource::User);
    assert_eq!(discovered.diagnostics.len(), 1);
    assert_eq!(
        discovered.diagnostics[0].diagnostic.code,
        SkillDiagnosticCode::InvalidMetadata
    );
    assert_eq!(
        discovered.diagnostics[0].diagnostic.message,
        "description is required"
    );
    assert_eq!(discovered.diagnostics[0].source, TestSkillSource::User);
}

// Pi basis: packages/agent/src/harness/skills.ts (`validateName`, `validateDescription`).
#[test]
fn skill_invalid_metadata_is_reported() {
    let result = parse_skill_document(
        "/workspace/skills/example/SKILL.md",
        "---\nname: Wrong_Name\n---\nMissing description.",
        true,
        Vec::new(),
    );
    assert!(result.skill.is_none());
    assert!(result.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == SkillDiagnosticCode::InvalidMetadata
            && diagnostic.message == "description is required"
    }));
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("invalid characters"))
    );

    let weakly_typed = parse_skill_document(
        "/workspace/skills/example/SKILL.md",
        "---\nname: [not, a, string]\ndescription: Example\ndisable-model-invocation: \"true\"\n---\nBody",
        true,
        Vec::new(),
    );
    let skill = weakly_typed
        .skill
        .expect("non-string optional metadata is treated as absent");
    assert_eq!(skill.descriptor.name, "example");
    assert!(!skill.descriptor.disable_model_invocation);
    assert!(weakly_typed.diagnostics.is_empty());

    let wrong_description = parse_skill_document(
        "/workspace/skills/example/SKILL.md",
        "---\ndescription: 42\n---\nBody",
        true,
        Vec::new(),
    );
    assert!(wrong_description.skill.is_none());
    assert!(wrong_description.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == SkillDiagnosticCode::InvalidMetadata
            && diagnostic.message == "description is required"
    }));
    assert!(
        !wrong_description
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == SkillDiagnosticCode::ParseFailed)
    );
}

// Pi basis: deterministic sorted skill traversal in packages/agent/src/harness/skills.ts;
// Architecture v2 part 2 §7.9 adds the content digest.
#[test]
fn skill_content_digest_is_stable() {
    let left = parse_skill_document(
        "/workspace/skills/example/SKILL.md",
        "---\r\nname: example\r\ndescription: Example\r\n---\r\nBody\r\n",
        true,
        vec![
            SkillResource {
                path: "z.txt".into(),
                data: b"z".to_vec(),
            },
            SkillResource {
                path: "a.txt".into(),
                data: b"a".to_vec(),
            },
        ],
    )
    .skill
    .expect("left skill is valid");
    let right = parse_skill_document(
        "/workspace/skills/example/SKILL.md",
        "---\nname: example\ndescription: Example\n---\nBody\n",
        true,
        vec![
            SkillResource {
                path: "a.txt".into(),
                data: b"a".to_vec(),
            },
            SkillResource {
                path: "z.txt".into(),
                data: b"z".to_vec(),
            },
        ],
    )
    .skill
    .expect("right skill is valid");
    assert_eq!(left.digest, right.digest);
}

// Pi basis: Architecture v2 part 2 §7.9 resume invariant over Pi skill content.
#[test]
fn skill_resume_uses_recorded_digest() {
    let admitted = valid_skill("---\nname: example\ndescription: Example\n---\nVersion one");
    let recorded = RecordedSkill::from_loaded(&admitted);
    let changed = valid_skill("---\nname: example\ndescription: Example\n---\nVersion two");
    let catalog = StaticSkillCatalog::new([changed.clone()]).expect("catalog is valid");

    let error = block_on(load_skill_for_resume(
        &catalog,
        &recorded,
        SkillResumePolicy::RequireRecordedDigest,
    ))
    .expect_err("changed skill is not silently resumed");
    assert!(matches!(error, SkillError::DigestMismatch { .. }));
    let explicitly_changed = block_on(load_skill_for_resume(
        &catalog,
        &recorded,
        SkillResumePolicy::AllowChangedContent,
    ))
    .expect("explicit policy accepts changed content");
    assert_eq!(explicitly_changed.digest, changed.digest);
}

// Architecture v2 part 2 §7.9/§10.10; Pi basis:
// packages/agent/test/harness/system-prompt.test.ts.
#[test]
fn system_prompt_formats_visible_skills_in_order_pi_exact() {
    let skills = vec![
        SkillDescriptor {
            id: SkillId::new("visible"),
            name: "visible".into(),
            description: "Use <this> & that".into(),
            location: "/skills/visible/SKILL.md".into(),
            disable_model_invocation: false,
        },
        SkillDescriptor {
            id: SkillId::new("hidden"),
            name: "hidden".into(),
            description: "Hidden".into(),
            location: "/skills/hidden/SKILL.md".into(),
            disable_model_invocation: true,
        },
        SkillDescriptor {
            id: SkillId::new("second"),
            name: "second".into(),
            description: "Second skill".into(),
            location: "/skills/second/SKILL.md".into(),
            disable_model_invocation: false,
        },
    ];

    assert_eq!(
        format_skills_for_system_prompt(&skills),
        concat!(
            "The following skills provide specialized instructions for specific tasks.\n",
            "Read the full skill file when the task matches its description.\n",
            "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n",
            "<available_skills>\n",
            "  <skill>\n",
            "    <name>visible</name>\n",
            "    <description>Use &lt;this&gt; &amp; that</description>\n",
            "    <location>/skills/visible/SKILL.md</location>\n",
            "  </skill>\n",
            "  <skill>\n",
            "    <name>second</name>\n",
            "    <description>Second skill</description>\n",
            "    <location>/skills/second/SKILL.md</location>\n",
            "  </skill>\n",
            "</available_skills>"
        )
    );
}

// Architecture v2 part 2 §7.9/§10.10; Pi basis:
// packages/agent/test/harness/system-prompt.test.ts.
#[test]
fn system_prompt_empty_when_no_skills_are_model_visible_pi_exact() {
    let hidden = SkillDescriptor {
        id: SkillId::new("hidden"),
        name: "hidden".into(),
        description: "Hidden".into(),
        location: "/skills/hidden/SKILL.md".into(),
        disable_model_invocation: true,
    };
    assert!(format_skills_for_system_prompt(&[hidden]).is_empty());
}

// Architecture v2 part 2 §7.9/§10.10; Pi basis:
// packages/agent/test/harness/system-prompt.test.ts.
#[test]
fn system_prompt_escapes_every_model_visible_field_pi_exact() {
    let skill = SkillDescriptor {
        id: SkillId::new("escaped"),
        name: "a&b".into(),
        description: "Quote \"double\" and 'single'".into(),
        location: "/skills/<bad>&\"quote\"/SKILL.md".into(),
        disable_model_invocation: false,
    };
    assert!(format_skills_for_system_prompt(&[skill]).contains(
        "<name>a&amp;b</name>\n    <description>Quote &quot;double&quot; and &apos;single&apos;</description>\n    <location>/skills/&lt;bad&gt;&amp;&quot;quote&quot;/SKILL.md</location>"
    ));
}

// Architecture v2 part 2 §7.9/§10.10; Pi basis:
// packages/agent/test/harness/prompt-templates.test.ts.
#[test]
fn prompt_template_discovers_markdown_nonrecursively_pi_exact() {
    let temporary = tempfile::tempdir().expect("temporary prompt root");
    let first = temporary.path().join("a");
    let second = temporary.path().join("b");
    fs::create_dir_all(first.join("nested")).expect("nested prompt directory");
    fs::create_dir_all(&second).expect("second prompt directory");
    fs::write(
        first.join("one.md"),
        "---\ndescription: One template\n---\nHello $1",
    )
    .expect("first prompt");
    fs::write(first.join("nested/ignored.md"), "Ignored").expect("nested prompt");
    fs::write(first.join("ignored.MD"), "Ignored uppercase extension")
        .expect("uppercase prompt extension");
    fs::write(first.join("ignored.txt"), "Ignored non-markdown file").expect("non-markdown prompt");
    fs::write(second.join("two.md"), "First line description\nBody").expect("second prompt");
    let missing = temporary.path().join("missing.md");

    let registry = NativePromptTemplateRegistry::discover([&first, &second, &missing])
        .expect("discover templates");
    assert!(registry.diagnostics().is_empty());
    let templates = registry.templates();
    assert_eq!(
        templates
            .iter()
            .map(|template| template.name.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(templates[0].description.as_deref(), Some("One template"));
    assert_eq!(templates[0].content, "Hello $1");
    assert_eq!(
        templates[1].description.as_deref(),
        Some("First line description")
    );
    assert_eq!(templates[1].content, "First line description\nBody");
}

// Architecture v2 part 2 §7.9/§10.10; Pi basis:
// packages/agent/test/harness/prompt-templates.test.ts.
#[test]
fn prompt_template_preserves_source_and_diagnostic_provenance_pi_exact() {
    let temporary = tempfile::tempdir().expect("temporary prompt root");
    let valid = temporary.path().join("example.md");
    let broken = temporary.path().join("broken.md");
    fs::write(&valid, "---\ndescription: Example\n---\nExample body").expect("valid prompt");
    fs::write(&broken, "---\ndescription: [unterminated\n---\nBody").expect("broken prompt");

    let registry = NativePromptTemplateRegistry::discover_sourced([
        PromptTemplateSource::new(&valid).with_source(json!({ "type": "project", "ordinal": 1 })),
        PromptTemplateSource::new(&broken)
            .with_source(json!({ "type": "user", "nested": { "trusted": false } })),
    ])
    .expect("discover sourced templates");
    assert_eq!(registry.sourced_templates().len(), 1);
    assert_eq!(
        registry.sourced_templates()[0].prompt_template.name,
        "example"
    );
    assert_eq!(
        registry.sourced_templates()[0].source,
        Some(json!({ "type": "project", "ordinal": 1 }))
    );
    assert_eq!(registry.diagnostics().len(), 1);
    assert_eq!(registry.diagnostics()[0].path, broken);
    assert_eq!(
        registry.diagnostics()[0].source,
        Some(json!({ "type": "user", "nested": { "trusted": false } }))
    );
    assert_eq!(
        registry.diagnostics()[0].severity,
        PromptTemplateDiagnosticSeverity::Warning
    );
    assert_eq!(
        registry.diagnostics()[0].code,
        PromptTemplateDiagnosticCode::ParseFailed
    );
    assert_eq!(
        serde_json::to_value(&registry.diagnostics()[0]).expect("diagnostic serializes")["type"],
        "warning"
    );
}

// Architecture v2 part 2 §7.9/§10.10; Pi basis:
// packages/agent/test/harness/prompt-templates.test.ts.
#[test]
fn prompt_template_loads_explicit_and_symlinked_files_pi_exact() {
    let temporary = tempfile::tempdir().expect("temporary prompt root");
    let target = temporary.path().join("target.md");
    let link = temporary.path().join("link.md");
    fs::write(&target, "---\ndescription: Target\n---\nTarget body").expect("target prompt");
    #[cfg(unix)]
    symlink(&target, &link).expect("prompt symlink");
    #[cfg(not(unix))]
    fs::copy(&target, &link).expect("prompt symlink fixture copy");

    let registry = NativePromptTemplateRegistry::discover([&target, &link])
        .expect("discover explicit templates");
    assert_eq!(
        registry
            .templates()
            .iter()
            .map(|template| (template.name.as_str(), template.content.as_str()))
            .collect::<Vec<_>>(),
        [("target", "Target body"), ("link", "Target body")]
    );
}

fn template_registry(content: &str) -> StaticPromptTemplateRegistry {
    StaticPromptTemplateRegistry::new([PromptTemplate::new(
        "review",
        Some("Review".into()),
        None,
        content,
    )
    .expect("template is valid")])
    .expect("registry is valid")
}

// Pi basis: packages/agent/src/harness/prompt-templates.ts (`substituteArgs`).
#[test]
fn prompt_template_argument_substitution() {
    let rendered =
        template_registry("Review $1 with ${@:2} ($ARGUMENTS / $@) | ${@:0} | ${@:0:2} | ${@:0:0}")
            .resolve(
                "review",
                &TemplateArguments::new(["a.ts", "carefully", "today"]),
            )
            .expect("all placeholders resolve");
    assert_eq!(
        rendered.content,
        "Review a.ts with carefully today (a.ts carefully today / a.ts carefully today) | a.ts carefully today | a.ts carefully | "
    );
}

// Pi basis: Architecture v2 part 2 §10.10 strict registry conformance. Pinned
// Pi's empty-substitution behavior remains available through PiCompatibleEmpty.
#[test]
fn prompt_template_missing_argument_rejected() {
    let strict_registry = StaticPromptTemplateRegistry::with_policy(
        [PromptTemplate::new("review", None, None, "Review $2").expect("template is valid")],
        MissingTemplateArgumentPolicy::Reject,
    )
    .expect("registry is valid");
    let error = strict_registry
        .resolve("review", &TemplateArguments::new(["a.ts"]))
        .expect_err("strict registry rejects a missing positional argument");
    assert!(matches!(
        error,
        TemplateError::MissingArgument { index: 2, .. }
    ));

    let pi_registry = template_registry("Review $2");
    assert_eq!(
        pi_registry
            .resolve("review", &TemplateArguments::new(["a.ts"]))
            .expect("Pi-compatible mode substitutes empty")
            .content,
        "Review "
    );
}

// Pi basis: packages/agent/src/harness/prompt-templates.ts uses ordered pure substitutions.
#[test]
fn prompt_template_output_is_deterministic() {
    let registry = template_registry("$1 ${@:2:2} $ARGUMENTS");
    let arguments = TemplateArguments::parse("'hello world' two three four");
    let first = registry
        .resolve("review", &arguments)
        .expect("render succeeds");
    let second = registry
        .resolve("review", &arguments)
        .expect("render succeeds");
    assert_eq!(first, second);
    assert_eq!(
        first.content,
        "hello world two three hello world two three four"
    );
    assert_eq!(
        template_registry("$1")
            .resolve("review", &TemplateArguments::new(["$@", "tail"]))
            .expect("later Pi substitution passes observe inserted text")
            .content,
        "$@ tail"
    );

    let weakly_typed = PromptTemplate::from_markdown(
        "metadata.md",
        "---\ndescription: [not, a, string]\nargument-hint: 42\n---\nFallback description\nBody",
    )
    .expect("valid YAML with non-string metadata still parses");
    assert_eq!(
        weakly_typed.description.as_deref(),
        Some("Fallback description")
    );
    assert_eq!(weakly_typed.argument_hint, None);
}

// Pi basis: packages/agent/src/harness/prompt-templates.ts (`firstLine.slice(0, 60)`
// and JavaScript string `.length`, both measured in UTF-16 code units).
#[test]
fn prompt_template_fallback_description_uses_utf16_units() {
    let cjk_line = format!("{}tail", "界".repeat(60));
    let cjk = PromptTemplate::from_markdown("cjk.md", &cjk_line).expect("template parses");
    assert_eq!(
        cjk.description.as_deref(),
        Some(format!("{}...", "界".repeat(60)).as_str())
    );

    let emoji_line = format!("{}tail", "🙂".repeat(30));
    let emoji = PromptTemplate::from_markdown("emoji.md", &emoji_line).expect("template parses");
    assert_eq!(
        emoji.description.as_deref(),
        Some(format!("{}...", "🙂".repeat(30)).as_str())
    );

    let empty_frontmatter = PromptTemplate::from_markdown(
        "empty.md",
        "---\ndescription: \"\"\n---\nFallback description",
    )
    .expect("template parses");
    assert_eq!(
        empty_frontmatter.description.as_deref(),
        Some("Fallback description")
    );
}

fn usage() -> Usage {
    Usage::zero(UsageSource::ProviderReported)
}

fn envelope(event_number: u32, event: TelemetryEvent) -> TelemetryEnvelope {
    TelemetryEnvelope::new(
        TelemetryEventId::new(format!("event-{event_number}")),
        Timestamp::from_unix_millis(i64::from(event_number)),
        event,
    )
}

fn all_telemetry_events() -> Vec<TelemetryEnvelope> {
    vec![
        envelope(
            1,
            TelemetryEvent::RunStarted {
                model: ModelRef::new("provider", "model"),
            },
        ),
        envelope(2, TelemetryEvent::ModelRequestStarted { attempt: 0 }),
        envelope(
            3,
            TelemetryEvent::ModelRequestFinished {
                finish: AssistantFinishReason::Stop,
                usage: usage(),
                duration: Duration::from_millis(10),
            },
        ),
        envelope(
            4,
            TelemetryEvent::ToolStarted {
                tool_name: "read".into(),
            },
        ),
        envelope(
            5,
            TelemetryEvent::ToolFinished {
                tool_name: "read".into(),
                success: true,
                duration: Duration::from_millis(2),
            },
        ),
        envelope(
            6,
            TelemetryEvent::CompactionStarted {
                reason: CompactionReason::Threshold,
            },
        ),
        envelope(7, TelemetryEvent::CompactionFinished { usage: usage() }),
        envelope(
            8,
            TelemetryEvent::SessionMutationCommitted {
                mutation_kind: "entry".into(),
            },
        ),
        envelope(
            9,
            TelemetryEvent::HandoffPerformed {
                report: HandoffTelemetrySummary {
                    source_model_count: 2,
                    change_count: 3,
                    lossy: true,
                },
            },
        ),
    ]
}

// Pi basis: packages/agent/src/harness/telemetry.ts and docs/telemetry-schema.md;
// Architecture v2 part 2 §7.12 defines the native envelope schema artifact.
#[test]
fn telemetry_schema_validates_every_event() {
    fn object_safe(_: &dyn TelemetrySink, _: &dyn LocalTelemetrySink) {}
    object_safe(&NoopTelemetrySink, &NoopTelemetrySink);

    let schema = telemetry_json_schema();
    let validator = jsonschema::validator_for(&schema).expect("generated schema compiles");
    for envelope in all_telemetry_events() {
        let value = serde_json::to_value(envelope).expect("event serializes");
        assert!(
            validator.is_valid(&value),
            "invalid telemetry event: {value}"
        );
    }
    assert_eq!(
        include_str!("../schema/telemetry-envelope.schema.json"),
        telemetry_json_schema_pretty()
    );
}

// Pi basis: Architecture v2 part 2 §7.12 (`schema_version` is required).
#[test]
fn telemetry_schema_version_is_required() {
    let schema = telemetry_json_schema();
    let validator = jsonschema::validator_for(&schema).expect("generated schema compiles");
    let mut value = serde_json::to_value(&all_telemetry_events()[0]).expect("event serializes");
    value
        .as_object_mut()
        .expect("envelope is an object")
        .remove("schema_version");
    assert!(!validator.is_valid(&value));
}

// Pi basis: Architecture v2 part 2 §7.12 content-exclusion default.
#[test]
fn telemetry_default_excludes_content() {
    let events = serde_json::to_string(&all_telemetry_events()).expect("events serialize");
    assert!(!events.contains("\"prompt\":"));
    assert!(!events.contains("\"response_content\":"));
    assert!(!events.contains("\"tool_arguments\":"));
    assert!(!events.contains("\"tool_output\":"));
}

// Pi basis: Architecture v2 part 2 §7.12 auth/header exclusion default.
#[test]
fn telemetry_default_excludes_auth() {
    let events = serde_json::to_string(&all_telemetry_events()).expect("events serialize");
    assert!(!events.contains("\"api_key\":"));
    assert!(!events.contains("\"authorization\":"));
    assert!(!events.contains("\"headers\":"));
    assert!(!events.contains("\"auth_data\":"));
}

// Pi basis: Architecture v2 part 2 §7.12 replay-payload exclusion default.
#[test]
fn telemetry_default_excludes_replay_payload() {
    let events = serde_json::to_string(&all_telemetry_events()).expect("events serialize");
    assert!(!events.contains("\"replay_payload\":"));
    assert!(!events.contains("\"opaque_payload\":"));
    assert!(!events.contains("\"encrypted_content\":"));
}

// Pi basis: packages/agent/src/harness/telemetry.ts correlation attributes;
// Architecture v2 part 2 §7.12 native correlation envelope.
#[test]
fn telemetry_correlates_session_run_operation() {
    let envelope = envelope(10, TelemetryEvent::ModelRequestStarted { attempt: 1 })
        .with_correlation(
            Some(SessionId::new("session")),
            Some(LaneName::new("main")),
            Some(RunId::new("run")),
            Some(OperationId::new("operation")),
        );
    let value = serde_json::to_value(envelope).expect("envelope serializes");
    assert_eq!(value["session_id"], "session");
    assert_eq!(value["lane"], "main");
    assert_eq!(value["run_id"], "run");
    assert_eq!(value["operation_id"], "operation");
}

struct RecordingSink {
    log: Arc<Mutex<Vec<String>>>,
}

impl TelemetrySink for RecordingSink {
    fn emit(&self, event: TelemetryEnvelope) -> SendBoxFuture<'_, Result<(), TelemetryError>> {
        Box::pin(async move {
            lock(&self.log).push(format!("telemetry:{:?}", event.sequence));
            Ok(())
        })
    }
}

// Pi basis: packages/agent/src/harness/telemetry.ts `pi.session.write` is a
// committed write; Architecture v2 part 2 §7.12 requires post-acceptance emission.
#[test]
fn telemetry_durable_event_follows_commit() {
    let log = Arc::new(Mutex::new(vec!["commit".into()]));
    let emitter = TelemetryEmitter::new(Arc::new(RecordingSink {
        log: Arc::clone(&log),
    }));
    let receipt = AppendReceipt {
        schema_version: 1,
        previous_sequence: Sequence::ZERO,
        last_sequence: Sequence::FIRST,
        mutation_count: 1,
    };
    block_on(emitter.emit_committed_mutation(
        &receipt,
        envelope(11, TelemetryEvent::ModelRequestStarted { attempt: 0 }),
        "entry",
    ))
    .expect("best-effort telemetry settles");
    assert_eq!(&*lock(&log), &["commit", "telemetry:Some(Sequence(1))"]);
}

struct FailingSink;

impl TelemetrySink for FailingSink {
    fn emit(&self, _event: TelemetryEnvelope) -> SendBoxFuture<'_, Result<(), TelemetryError>> {
        Box::pin(async { Err(TelemetryError::sink("sink unavailable")) })
    }
}

// Pi basis: Architecture v2 part 2 §7.12 best-effort default.
#[test]
fn telemetry_sink_failure_is_best_effort_by_default() {
    let best_effort = TelemetryEmitter::new(Arc::new(FailingSink));
    assert!(
        block_on(best_effort.emit(envelope(
            12,
            TelemetryEvent::ModelRequestStarted { attempt: 0 }
        )))
        .is_ok()
    );

    let required = TelemetryEmitter::with_failure_policy(
        Arc::new(FailingSink),
        TelemetryFailurePolicy::Required,
    );
    assert!(
        block_on(required.emit(envelope(
            13,
            TelemetryEvent::ModelRequestStarted { attempt: 0 }
        )))
        .is_err()
    );
}
