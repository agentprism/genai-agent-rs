use bytes::Bytes;
use futures_channel::oneshot;
use futures_executor::block_on;
use futures_util::future::{join, join3};
use pi_agent_env::{
    AgentFileSystem, AgentPath, ArtifactError, ArtifactRef, CanonicalPath, EditResult,
    FileReadResult, FileSystemError, FileWriteResult, ProcessExitStatus, ReadLimits,
    TemporaryArtifactRequest, TemporaryArtifactStore,
};
use pi_agent_harness::*;
use pi_agent_session::{AppendReceipt, CompactionReason, LaneName, Sequence, SessionId};
use pi_ai::{
    AssistantFinishReason, CancellationToken, ModelRef, RunId, SendBoxFuture, Timestamp, Usage,
    UsageSource,
};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex, MutexGuard},
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
}

impl MemoryFileSystem {
    fn with_file(path: &str, content: &str) -> Self {
        Self {
            files: Mutex::new(BTreeMap::from([(
                path.to_owned(),
                content.as_bytes().to_vec(),
            )])),
        }
    }
}

impl AgentFileSystem for MemoryFileSystem {
    fn canonicalize(
        &self,
        path: &AgentPath,
    ) -> SendBoxFuture<'_, Result<CanonicalPath, FileSystemError>> {
        let path = path.to_string();
        Box::pin(async move { Ok(CanonicalPath::new(path)) })
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

#[derive(Default)]
struct MemoryArtifacts {
    created: Mutex<Vec<(TemporaryArtifactRequest, Bytes)>>,
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
