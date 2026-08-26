use agentprism_ai::CancellationToken;
use agentprism_env::{
    AgentEnvironment, AgentFileSystem, AgentPath, FileSystemError, ProcessCommand, ProcessError,
    ProcessEvent, ProcessSpawner, ProcessTermination, ReadLimits, TerminationPolicy,
    UnavailableProcessSpawner,
};
use agentprism_runtime_tokio::{TokioAgentEnvironment, TokioAgentFileSystem};
use bytes::Bytes;
use futures_util::StreamExt;
use std::{
    io::{Read, Write},
    time::Duration,
};
use tempfile::TempDir;

#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};

const BIDIRECTIONAL_HELPER_ENV: &str = "PI_AGENT_ENV_BIDIRECTIONAL_HELPER";
const BIDIRECTIONAL_BYTES: usize = 2 * 1024 * 1024;

fn filesystem() -> (TempDir, TokioAgentFileSystem) {
    let root = tempfile::tempdir().unwrap();
    let filesystem = TokioAgentFileSystem::new(root.path()).unwrap();
    (root, filesystem)
}

#[cfg(unix)]
fn shell(script: &str) -> ProcessCommand {
    ProcessCommand::new("/bin/sh").with_arguments(["-c".into(), script.into()])
}

#[cfg(windows)]
fn shell(script: &str) -> ProcessCommand {
    ProcessCommand::new("cmd.exe").with_arguments(["/C".into(), script.into()])
}

#[cfg(unix)]
fn long_running_command() -> ProcessCommand {
    shell("sleep 60")
}

#[cfg(windows)]
fn long_running_command() -> ProcessCommand {
    shell("ping -n 61 127.0.0.1 >NUL")
}

fn bidirectional_helper_command(input: Bytes) -> ProcessCommand {
    let executable = std::env::current_exe().unwrap();
    let mut command = ProcessCommand::new(executable.to_string_lossy()).with_arguments([
        "env_process_large_bidirectional_stdio".into(),
        "--exact".into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
    ]);
    command
        .environment
        .insert(BIDIRECTIONAL_HELPER_ENV.into(), "1".into());
    command.stdin = Some(input);
    command
}

#[cfg(windows)]
fn windows_helper_command(mode: &str) -> ProcessCommand {
    let executable = std::env::current_exe().unwrap();
    let mut command = ProcessCommand::new(executable.to_string_lossy()).with_arguments([
        "windows_process_helper".into(),
        "--exact".into(),
        "--ignored".into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
    ]);
    command
        .environment
        .insert("PI_AGENT_ENV_WINDOWS_HELPER".into(), mode.into());
    command
}

#[cfg(windows)]
fn native_windows_helper(mode: &str) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "windows_process_helper",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PI_AGENT_ENV_WINDOWS_HELPER", mode);
    command
}

#[cfg(windows)]
static WINDOWS_BREAK_RECEIVED: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
static WINDOWS_BREAK_IGNORED: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
unsafe extern "system" fn windows_control_handler(control: u32) -> i32 {
    if control != windows_sys::Win32::System::Console::CTRL_BREAK_EVENT {
        return 0;
    }
    if !WINDOWS_BREAK_IGNORED.load(Ordering::SeqCst) {
        WINDOWS_BREAK_RECEIVED.store(true, Ordering::SeqCst);
    }
    1
}

#[cfg(windows)]
#[test]
#[ignore = "spawned by the Windows process conformance tests"]
fn windows_process_helper() {
    let mode = std::env::var("PI_AGENT_ENV_WINDOWS_HELPER").unwrap_or_default();
    match mode.as_str() {
        "graceful" | "ignore" | "stdio_terminate" => {
            WINDOWS_BREAK_RECEIVED.store(false, Ordering::SeqCst);
            WINDOWS_BREAK_IGNORED.store(mode == "ignore", Ordering::SeqCst);
            // SAFETY: the handler has static lifetime and the process exits
            // before this ignored helper test returns to its parent harness.
            assert_ne!(
                unsafe {
                    windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                        Some(windows_control_handler),
                        1,
                    )
                },
                0
            );
            println!("PI_READY");
            std::io::stdout().flush().unwrap();
            if mode == "ignore" {
                let mut stdout = std::io::stdout().lock();
                loop {
                    if stdout.write_all(b"flood").is_err() || stdout.flush().is_err() {
                        return;
                    }
                }
            }
            while !WINDOWS_BREAK_RECEIVED.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            if mode == "stdio_terminate" {
                let child = native_windows_helper("stdio_child")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .spawn()
                    .unwrap();
                std::mem::forget(child);
                print!("PI_TERMINATED");
                std::io::stdout().flush().unwrap();
            }
        }
        "tree_parent" => {
            let mut child = native_windows_helper("sleep_child")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            println!("PI_DESCENDANT:{}", child.id());
            std::io::stdout().flush().unwrap();
            let _ = child.wait();
        }
        "stdio_parent" => {
            let child = native_windows_helper("stdio_child")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .unwrap();
            let child_id = child.id();
            // This helper must exit while its descendant retains inherited
            // stdio, exactly like pinned nodejs-env.test.ts. The containing
            // Job Object owns eventual tree cleanup.
            std::mem::forget(child);
            println!("PI_DESCENDANT:{child_id}");
            print!("PI_PARENT");
            std::io::stdout().flush().unwrap();
        }
        "stdio_child" => {
            for index in 1..=3 {
                std::thread::sleep(Duration::from_millis(100));
                print!("PI_CHUNK_{index}");
                std::io::stdout().flush().unwrap();
            }
            std::thread::sleep(Duration::from_secs(60));
        }
        "sleep_child" => std::thread::sleep(Duration::from_secs(60)),
        _ => panic!("unknown Windows helper mode: {mode}"),
    }
}

async fn collect_process(
    process: &mut dyn agentprism_env::RunningProcess,
) -> (Vec<u8>, Vec<u8>, agentprism_env::ProcessOutcome) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut outcome = None;
    let mut events = process.events();
    while let Some(event) = events.next().await {
        match event.unwrap() {
            ProcessEvent::Stdout(bytes) => stdout.extend_from_slice(&bytes),
            ProcessEvent::Stderr(bytes) => stderr.extend_from_slice(&bytes),
            ProcessEvent::Exited(value) => outcome = Some(value),
            _ => {}
        }
    }
    (
        stdout,
        stderr,
        outcome.expect("process stream must terminate"),
    )
}

#[tokio::test]
async fn env_read_file() {
    // §10.10 Environment. Pi basis: nodejs-env.test.ts reads text/binary
    // content and stops bounded reads at the requested limit.
    let (root, filesystem) = filesystem();
    std::fs::write(root.path().join("input.txt"), b"hello").unwrap();

    let complete = filesystem
        .read(
            &AgentPath::new("input.txt"),
            ReadLimits::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(complete.data, Bytes::from_static(b"hello"));
    assert_eq!(complete.total_bytes, 5);
    assert!(!complete.truncated);

    let bounded = filesystem
        .read(
            &AgentPath::new("input.txt"),
            ReadLimits { max_bytes: Some(3) },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(bounded.data, Bytes::from_static(b"hel"));
    assert_eq!(bounded.total_bytes, 5);
    assert!(bounded.truncated);

    #[cfg(target_os = "linux")]
    {
        // Procfs commonly reports zero metadata length. The extra observed
        // byte, not metadata, must still prove that a zero-byte result was
        // truncated.
        let procfs = TokioAgentFileSystem::new("/").unwrap();
        let observed_extra = procfs
            .read(
                &AgentPath::new("/proc/self/cmdline"),
                ReadLimits { max_bytes: Some(0) },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(observed_extra.data.is_empty());
        assert!(observed_extra.total_bytes >= 1);
        assert!(observed_extra.truncated);
    }
}

#[tokio::test]
async fn env_write_file() {
    // §10.10 Environment. Pi basis: nodejs-env.test.ts creates missing parent
    // directories and writes binary-safe content.
    let (root, filesystem) = filesystem();
    let result = filesystem
        .write(
            &AgentPath::new("nested/output.bin"),
            Bytes::from_static(&[0, 1, 2, 255]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.bytes_written, 4);
    assert_eq!(
        std::fs::read(root.path().join("nested/output.bin")).unwrap(),
        [0, 1, 2, 255]
    );
}

#[tokio::test]
async fn env_atomic_replace() {
    // §10.10 Environment. Pi basis: nodejs-env.test.ts requires atomic rename
    // replacement; harness edit semantics require one exact, non-noop match.
    let (root, filesystem) = filesystem();
    std::fs::write(root.path().join("edit.txt"), "before middle after").unwrap();
    let result = filesystem
        .replace_exact(
            &AgentPath::new("edit.txt"),
            "middle",
            "replacement",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.replacements, 1);
    assert_eq!(
        std::fs::read_to_string(root.path().join("edit.txt")).unwrap(),
        "before replacement after"
    );
    assert!(matches!(
        filesystem
            .replace_exact(
                &AgentPath::new("edit.txt"),
                "before",
                "before",
                CancellationToken::new(),
            )
            .await,
        Err(FileSystemError::NoOpReplacement { .. })
    ));
}

#[tokio::test]
async fn env_process_stdout_stream() {
    // §10.10 Environment. Pi basis: nodejs-env.test.ts streams stdout chunks.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut process = environment
        .processes()
        .spawn(shell("printf stdout"), CancellationToken::new())
        .await
        .unwrap();
    let (stdout, _, _) = collect_process(process.as_mut()).await;
    assert_eq!(stdout, b"stdout");
}

#[tokio::test]
async fn env_process_stderr_stream() {
    // §10.10 Environment. Pi basis: nodejs-env.test.ts streams stderr chunks.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut process = environment
        .processes()
        .spawn(shell("printf stderr >&2"), CancellationToken::new())
        .await
        .unwrap();
    let (_, stderr, _) = collect_process(process.as_mut()).await;
    assert_eq!(stderr, b"stderr");
}

#[tokio::test]
async fn env_process_exit_status() {
    // §10.10 Environment. Pi basis: nodejs-env.test.ts treats nonzero process
    // exit as an ordinary status rather than a spawn failure.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut process = environment
        .processes()
        .spawn(shell("exit 7"), CancellationToken::new())
        .await
        .unwrap();
    let (_, _, outcome) = collect_process(process.as_mut()).await;
    assert_eq!(outcome.status.code, Some(7));
    assert!(!outcome.status.success);
    assert_eq!(outcome.termination, ProcessTermination::Exited);
}

#[tokio::test]
async fn env_process_large_bidirectional_stdio() {
    // Reviewer regression for §7.10/§10.10 environment streaming: the child
    // fills stdout before it reads a stdin payload larger than an OS pipe.
    // Spawn and the owned process driver must therefore pump all three pipes
    // concurrently instead of synchronously writing stdin first.
    if std::env::var_os(BIDIRECTIONAL_HELPER_ENV).is_some() {
        let output = vec![0xa5; BIDIRECTIONAL_BYTES];
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&output).unwrap();
        stdout.flush().unwrap();
        drop(stdout);

        let mut input = Vec::new();
        std::io::stdin().lock().read_to_end(&mut input).unwrap();
        assert_eq!(input.len(), BIDIRECTIONAL_BYTES);
        assert!(input.iter().all(|byte| *byte == 0x5a));
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let input = Bytes::from(vec![0x5a; BIDIRECTIONAL_BYTES]);
    let mut process = tokio::time::timeout(
        Duration::from_secs(2),
        environment.processes().spawn(
            bidirectional_helper_command(input),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("spawn must not block while writing process stdin")
    .unwrap();
    let (stdout, stderr, outcome) =
        tokio::time::timeout(Duration::from_secs(10), collect_process(process.as_mut()))
            .await
            .expect("bidirectional stdio must make progress in both directions");
    assert!(
        outcome.status.success,
        "helper stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(
        stdout.iter().filter(|byte| **byte == 0xa5).count(),
        BIDIRECTIONAL_BYTES
    );
}

#[cfg(unix)]
#[tokio::test]
async fn env_process_graceful_termination() {
    // §10.10 Environment. Pi basis: nodejs.ts terminates the active process
    // group and waits for process settlement.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut process = environment
        .processes()
        .spawn(
            shell("trap 'exit 0' TERM; while :; do sleep 1; done"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let outcome = process
        .terminate(TerminationPolicy::default())
        .await
        .unwrap();
    assert_eq!(outcome.termination, ProcessTermination::Graceful);
}

#[cfg(windows)]
#[tokio::test]
async fn env_process_graceful_termination() {
    // §10.10 Environment. Windows children start suspended, enter the Job
    // Object, and only then run. CTRL_BREAK is the graceful process-group
    // signal available on Windows.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut process = environment
        .processes()
        .spawn(windows_helper_command("graceful"), CancellationToken::new())
        .await
        .unwrap();
    wait_for_stdout_marker(process.as_mut(), b"PI_READY").await;
    let outcome = process
        .terminate(TerminationPolicy::default())
        .await
        .unwrap();
    assert_eq!(outcome.termination, ProcessTermination::Graceful);
}

#[cfg(unix)]
#[tokio::test]
async fn env_process_forced_termination() {
    // §10.10 Environment. Pi basis: nodejs.ts escalates process-tree
    // termination when graceful shutdown does not settle.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut process = environment
        .processes()
        .spawn(
            shell("trap '' TERM; printf ready; while :; do printf flood; done"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    {
        let mut events = process.events();
        loop {
            if let Some(ProcessEvent::Stdout(bytes)) = events.next().await.transpose().unwrap()
                && bytes
                    .windows(b"ready".len())
                    .any(|window| window == b"ready")
            {
                break;
            }
        }
    }
    let policy = TerminationPolicy {
        graceful_timeout: Duration::from_millis(30),
        forced_timeout: Duration::from_secs(1),
        ..TerminationPolicy::default()
    };
    let outcome = tokio::time::timeout(Duration::from_secs(2), process.terminate(policy))
        .await
        .expect("ready stdout must not starve termination deadlines")
        .unwrap();
    assert_eq!(outcome.termination, ProcessTermination::Forced);
}

#[cfg(windows)]
#[tokio::test]
async fn env_process_forced_termination() {
    // §10.10 Environment. A child that consumes CTRL_BREAK without exiting is
    // escalated through TerminateJobObject after the graceful deadline.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut process = environment
        .processes()
        .spawn(windows_helper_command("ignore"), CancellationToken::new())
        .await
        .unwrap();
    wait_for_stdout_marker(process.as_mut(), b"PI_READY").await;
    let policy = TerminationPolicy {
        graceful_timeout: Duration::from_millis(30),
        forced_timeout: Duration::from_secs(2),
        ..TerminationPolicy::default()
    };
    let outcome = tokio::time::timeout(Duration::from_secs(3), process.terminate(policy))
        .await
        .expect("ready stdout must not starve termination deadlines")
        .unwrap();
    assert_eq!(outcome.termination, ProcessTermination::Forced);
}

#[cfg(unix)]
#[tokio::test]
async fn env_process_tree_termination() {
    // §10.10 Environment. Pi basis: nodejs.ts killProcessTree targets the
    // detached Unix process group, not only the immediate shell.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut process = environment
        .processes()
        .spawn(
            shell("sleep 60 & child=$!; printf '%s\\n' \"$child\"; wait"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let descendant = {
        let mut events = process.events();
        loop {
            if let Some(ProcessEvent::Stdout(bytes)) = events.next().await.transpose().unwrap() {
                let text = std::str::from_utf8(&bytes).unwrap().trim();
                if !text.is_empty() {
                    break text.parse::<i32>().unwrap();
                }
            }
        }
    };
    process
        .terminate(TerminationPolicy::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!process_exists(descendant));
}

#[cfg(windows)]
#[tokio::test]
async fn env_process_tree_termination() {
    // §10.10 Environment. Descendants inherit the Job Object, so forced tree
    // termination settles both the harness child and its spawned descendant.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut process = environment
        .processes()
        .spawn(
            windows_helper_command("tree_parent"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let output = wait_for_stdout_marker(process.as_mut(), b"PI_DESCENDANT:").await;
    let descendant = marker_pid(&output, b"PI_DESCENDANT:");
    let policy = TerminationPolicy {
        graceful_timeout: Duration::from_millis(30),
        forced_timeout: Duration::from_secs(2),
        ..TerminationPolicy::default()
    };
    process.terminate(policy).await.unwrap();
    wait_for_windows_process_exit(descendant).await;
}

#[tokio::test]
async fn env_process_non_tree_termination() {
    // §7.10 TerminationPolicy regression. A non-tree request must preserve a
    // descendant. On Windows the only immediate-process primitive is forced
    // termination, so the outcome must not be mislabeled graceful and Job
    // Object cleanup must be disarmed before the handle closes.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();

    #[cfg(unix)]
    {
        let mut process = environment
            .processes()
            .spawn(
                shell("trap 'exit 0' TERM; sleep 60 & child=$!; printf '%s\\n' \"$child\"; wait"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let descendant = {
            let mut events = process.events();
            loop {
                if let Some(ProcessEvent::Stdout(bytes)) = events.next().await.transpose().unwrap()
                {
                    let text = std::str::from_utf8(&bytes).unwrap().trim();
                    if !text.is_empty() {
                        break text.parse::<i32>().unwrap();
                    }
                }
            }
        };
        let policy = TerminationPolicy {
            terminate_process_tree: false,
            ..TerminationPolicy::default()
        };
        let outcome = process.terminate(policy).await.unwrap();
        let descendant_survived = process_exists(descendant);
        // SAFETY: the PID came from the child shell and SIGKILL is a valid
        // cleanup signal for this hermetic test-owned process.
        unsafe {
            libc::kill(descendant, libc::SIGKILL);
        }
        assert_eq!(outcome.termination, ProcessTermination::Graceful);
        assert!(descendant_survived);

        let mut resistant = environment
            .processes()
            .spawn(
                shell(
                    "trap '' TERM; sleep 60 & child=$!; printf '%s\\n' \"$child\"; wait \"$child\"",
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let resistant_descendant = {
            let mut events = resistant.events();
            loop {
                if let Some(ProcessEvent::Stdout(bytes)) = events.next().await.transpose().unwrap()
                {
                    let text = std::str::from_utf8(&bytes).unwrap().trim();
                    if !text.is_empty() {
                        break text.parse::<i32>().unwrap();
                    }
                }
            }
        };
        let forced_policy = TerminationPolicy {
            graceful_timeout: Duration::from_millis(30),
            forced_timeout: Duration::from_secs(1),
            terminate_process_tree: false,
            ..TerminationPolicy::default()
        };
        let forced_outcome =
            tokio::time::timeout(Duration::from_secs(2), resistant.terminate(forced_policy))
                .await
                .expect("non-tree forced escalation must settle the immediate parent")
                .unwrap();
        let resistant_descendant_survived = process_exists(resistant_descendant);
        // SAFETY: the PID came from the child shell and SIGKILL is a valid
        // cleanup signal for this hermetic test-owned process.
        unsafe {
            libc::kill(resistant_descendant, libc::SIGKILL);
        }
        assert_eq!(forced_outcome.termination, ProcessTermination::Forced);
        assert!(
            resistant_descendant_survived,
            "forced non-tree escalation terminated the descendant"
        );
    }

    #[cfg(windows)]
    {
        let mut process = environment
            .processes()
            .spawn(
                windows_helper_command("tree_parent"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = wait_for_stdout_marker(process.as_mut(), b"PI_DESCENDANT:").await;
        let descendant = marker_pid(&output, b"PI_DESCENDANT:");
        let policy = TerminationPolicy {
            graceful_timeout: Duration::from_secs(1),
            forced_timeout: Duration::from_secs(2),
            terminate_process_tree: false,
            ..TerminationPolicy::default()
        };
        let outcome = process.terminate(policy).await.unwrap();
        drop(process);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let descendant_survived = windows_process_exists(descendant);
        terminate_windows_process(descendant);
        wait_for_windows_process_exit(descendant).await;
        assert_eq!(outcome.termination, ProcessTermination::Forced);
        assert!(
            descendant_survived,
            "non-tree Job Object cleanup terminated the descendant"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn env_stdio_grace_period() {
    // §10.10 Environment. Pi basis: nodejs.ts waits briefly for trailing
    // stdio but settles when a descendant retains inherited descriptors.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut command = shell(
        "(sleep 0.08; printf chunk-1; sleep 0.08; printf chunk-2; sleep 0.08; printf chunk-3; sleep 60) & printf '%s\\n' \"$!\"; printf parent",
    );
    command.cancellation_policy.stdio_grace_period = Duration::from_millis(150);
    let mut process = environment
        .processes()
        .spawn(command, CancellationToken::new())
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), collect_process(process.as_mut()))
        .await
        .expect("stdio grace must bound inherited descriptors");
    let stdout = String::from_utf8(result.0).unwrap();
    let mut lines = stdout.lines();
    let descendant = lines.next().unwrap().parse::<i32>().unwrap();
    assert!(stdout.contains("parent"));
    assert!(
        stdout.contains("chunk-3"),
        "post-exit chunks must rearm the idle deadline"
    );
    // SAFETY: the PID came from the child shell and SIGKILL is a valid signal.
    unsafe {
        libc::kill(descendant, libc::SIGKILL);
    }

    let mut explicit_command = shell(
        "trap '(sleep 0.04; printf explicit-tail) & printf explicit-stop; exit 0' TERM; printf explicit-ready; while :; do sleep 1; done",
    );
    explicit_command.cancellation_policy.stdio_grace_period = Duration::from_millis(150);
    let mut explicit = environment
        .processes()
        .spawn(explicit_command, CancellationToken::new())
        .await
        .unwrap();
    wait_for_stdout_marker(explicit.as_mut(), b"explicit-ready").await;
    let explicit_policy = TerminationPolicy {
        stdio_grace_period: Duration::from_millis(150),
        ..TerminationPolicy::default()
    };
    explicit.terminate(explicit_policy).await.unwrap();
    let (explicit_stdout, _, explicit_outcome) = collect_process(explicit.as_mut()).await;
    assert_eq!(explicit_outcome.termination, ProcessTermination::Graceful);
    assert!(
        explicit_stdout
            .windows(b"explicit-stop".len())
            .any(|part| part == b"explicit-stop"),
        "stdio read while explicit termination waits must be emitted"
    );
    assert!(
        explicit_stdout
            .windows(b"explicit-tail".len())
            .any(|part| part == b"explicit-tail"),
        "explicit termination must retain trailing stdio through the grace window"
    );

    let cancellation = CancellationToken::new();
    let mut cancelled_command = shell(
        "trap '(sleep 0.04; printf cancel-tail) & printf cancel-stop; exit 0' TERM; printf cancel-ready; while :; do sleep 1; done",
    );
    cancelled_command.cancellation_policy.stdio_grace_period = Duration::from_millis(150);
    let mut cancelled = environment
        .processes()
        .spawn(cancelled_command, cancellation.clone())
        .await
        .unwrap();
    wait_for_stdout_marker(cancelled.as_mut(), b"cancel-ready").await;
    cancellation.cancel();
    let (cancelled_stdout, _, cancelled_outcome) = collect_process(cancelled.as_mut()).await;
    assert_eq!(cancelled_outcome.termination, ProcessTermination::Cancelled);
    assert!(
        cancelled_stdout
            .windows(b"cancel-stop".len())
            .any(|part| part == b"cancel-stop"),
        "stdio read while cancellation waits must be emitted"
    );
    assert!(
        cancelled_stdout
            .windows(b"cancel-tail".len())
            .any(|part| part == b"cancel-tail"),
        "cancellation must retain trailing stdio through the grace window"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn env_stdio_grace_period() {
    // §10.10 Environment. This is the pinned nodejs-env Windows case plus the
    // nodejs.ts idle-deadline rule: every inherited-stdio chunk rearms the
    // grace period, then an idle descendant cannot hold completion forever.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();
    let mut command = windows_helper_command("stdio_parent");
    command.cancellation_policy.stdio_grace_period = Duration::from_millis(200);
    let mut process = environment
        .processes()
        .spawn(command, CancellationToken::new())
        .await
        .unwrap();
    let (stdout, _, _) =
        tokio::time::timeout(Duration::from_secs(3), collect_process(process.as_mut()))
            .await
            .expect("stdio idle grace must bound inherited descriptors");
    assert!(
        stdout
            .windows(b"PI_PARENT".len())
            .any(|part| part == b"PI_PARENT")
    );
    assert!(
        stdout
            .windows(b"PI_CHUNK_3".len())
            .any(|part| part == b"PI_CHUNK_3"),
        "post-exit chunks must rearm the idle deadline"
    );
    let descendant = marker_pid(&stdout, b"PI_DESCENDANT:");
    drop(process);
    wait_for_windows_process_exit(descendant).await;

    let mut explicit_command = windows_helper_command("stdio_terminate");
    explicit_command.cancellation_policy.stdio_grace_period = Duration::from_millis(200);
    let mut explicit = environment
        .processes()
        .spawn(explicit_command, CancellationToken::new())
        .await
        .unwrap();
    wait_for_stdout_marker(explicit.as_mut(), b"PI_READY").await;
    let explicit_policy = TerminationPolicy {
        stdio_grace_period: Duration::from_millis(200),
        ..TerminationPolicy::default()
    };
    explicit.terminate(explicit_policy).await.unwrap();
    let (explicit_stdout, _, explicit_outcome) = collect_process(explicit.as_mut()).await;
    assert_eq!(explicit_outcome.termination, ProcessTermination::Graceful);
    assert!(
        explicit_stdout
            .windows(b"PI_TERMINATED".len())
            .any(|part| part == b"PI_TERMINATED")
    );
    assert!(
        explicit_stdout
            .windows(b"PI_CHUNK_3".len())
            .any(|part| part == b"PI_CHUNK_3")
    );
    drop(explicit);

    let cancellation = CancellationToken::new();
    let mut cancelled_command = windows_helper_command("stdio_terminate");
    cancelled_command.cancellation_policy.stdio_grace_period = Duration::from_millis(200);
    let mut cancelled = environment
        .processes()
        .spawn(cancelled_command, cancellation.clone())
        .await
        .unwrap();
    wait_for_stdout_marker(cancelled.as_mut(), b"PI_READY").await;
    cancellation.cancel();
    let (cancelled_stdout, _, cancelled_outcome) = collect_process(cancelled.as_mut()).await;
    assert_eq!(cancelled_outcome.termination, ProcessTermination::Cancelled);
    assert!(
        cancelled_stdout
            .windows(b"PI_TERMINATED".len())
            .any(|part| part == b"PI_TERMINATED")
    );
    assert!(
        cancelled_stdout
            .windows(b"PI_CHUNK_3".len())
            .any(|part| part == b"PI_CHUNK_3")
    );
}

#[tokio::test]
async fn env_cancellation() {
    // §10.10 Environment. Pi basis: nodejs-env.test.ts checks every
    // cancellable filesystem operation with a pre-aborted signal, aborts
    // process creation before spawn, and terminates active process trees.
    let root = tempfile::tempdir().unwrap();
    let environment = TokioAgentEnvironment::new(root.path()).unwrap();

    std::fs::write(root.path().join("input.txt"), b"before").unwrap();
    let pre_cancelled = CancellationToken::new();
    pre_cancelled.cancel();
    assert!(matches!(
        environment
            .filesystem()
            .read(
                &AgentPath::new("input.txt"),
                ReadLimits::default(),
                pre_cancelled.clone(),
            )
            .await,
        Err(FileSystemError::Cancelled { .. })
    ));
    assert!(matches!(
        environment
            .filesystem()
            .write(
                &AgentPath::new("cancelled-write.txt"),
                Bytes::from_static(b"after"),
                pre_cancelled.clone(),
            )
            .await,
        Err(FileSystemError::Cancelled { .. })
    ));
    assert!(matches!(
        environment
            .filesystem()
            .replace_exact(
                &AgentPath::new("input.txt"),
                "before",
                "after",
                pre_cancelled.clone(),
            )
            .await,
        Err(FileSystemError::Cancelled { .. })
    ));
    assert!(!root.path().join("cancelled-write.txt").exists());
    assert_eq!(
        std::fs::read(root.path().join("input.txt")).unwrap(),
        b"before"
    );
    assert!(matches!(
        environment
            .processes()
            .spawn(long_running_command(), pre_cancelled)
            .await,
        Err(ProcessError::Cancelled)
    ));

    let cancellation = CancellationToken::new();
    let mut command = long_running_command();
    command.cancellation_policy.graceful_timeout = Duration::from_millis(20);
    let mut process = environment
        .processes()
        .spawn(command, cancellation.clone())
        .await
        .unwrap();
    cancellation.cancel();
    let (_, _, outcome) =
        tokio::time::timeout(Duration::from_secs(2), collect_process(process.as_mut()))
            .await
            .expect("cancelled process must settle");
    assert_eq!(outcome.termination, ProcessTermination::Cancelled);
}

#[tokio::test]
async fn capability_unavailable_process_spawner_is_explicit() {
    // Architecture v2 part 2 §7.10: mobile/WASM process spawning reports the
    // capability path explicitly instead of impersonating a process runtime.
    let result = UnavailableProcessSpawner
        .spawn(ProcessCommand::new("ignored"), CancellationToken::new())
        .await;
    assert!(matches!(
        result,
        Err(ProcessError::CapabilityUnavailable(ref capability))
            if capability.capability == "process_spawn"
    ));
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    // SAFETY: signal zero performs existence/permission probing only.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

async fn wait_for_stdout_marker(
    process: &mut dyn agentprism_env::RunningProcess,
    marker: &[u8],
) -> Vec<u8> {
    let mut stdout = Vec::new();
    let mut events = process.events();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(event) = events.next().await {
            match event.unwrap() {
                ProcessEvent::Stdout(bytes) => {
                    stdout.extend_from_slice(&bytes);
                    if stdout
                        .windows(marker.len())
                        .any(|candidate| candidate == marker)
                    {
                        return;
                    }
                }
                ProcessEvent::Exited(outcome) => {
                    panic!("Windows helper exited before marker: {outcome:?}")
                }
                ProcessEvent::Stderr(_) => {}
                _ => {}
            }
        }
        panic!("Windows helper event stream ended before marker");
    })
    .await
    .expect("Windows helper must produce its readiness marker");
    stdout
}

#[cfg(windows)]
fn marker_pid(output: &[u8], marker: &[u8]) -> u32 {
    let start = output
        .windows(marker.len())
        .position(|candidate| candidate == marker)
        .expect("process output must contain the PID marker")
        + marker.len();
    let digits = output[start..]
        .iter()
        .copied()
        .take_while(u8::is_ascii_digit)
        .collect::<Vec<_>>();
    std::str::from_utf8(&digits)
        .unwrap()
        .parse()
        .expect("PID marker must contain a decimal process identifier")
}

#[cfg(windows)]
async fn wait_for_windows_process_exit(pid: u32) {
    for _ in 0..100 {
        if !windows_process_exists(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("Windows descendant {pid} survived process-tree termination");
}

#[cfg(windows)]
fn windows_process_exists(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };

    // SAFETY: the access mask is read-only synchronization access and `pid`
    // came from the spawned child. The returned handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: `process` is live and a zero timeout performs a status probe.
    let status = unsafe { WaitForSingleObject(process, 0) };
    // SAFETY: `process` is an owned handle no longer used after this call.
    unsafe {
        CloseHandle(process);
    }
    status == WAIT_TIMEOUT
}

#[cfg(windows)]
fn terminate_windows_process(pid: u32) {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
    };

    // SAFETY: the access is limited to termination of this test-owned helper,
    // and the returned handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        return;
    }
    // SAFETY: `process` is a live handle opened with termination access.
    unsafe {
        TerminateProcess(process, 1);
        CloseHandle(process);
    }
}
