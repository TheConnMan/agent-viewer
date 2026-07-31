use crate::error::{Error, Result};

/// Current time as epoch milliseconds (0 if the system clock predates the epoch).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The spawn "name"/"title" derived from a task prompt: the first 40 chars (char-, not
/// byte-bounded). Used by the claude `--name` spawn flag.
pub fn truncated_title(task: &str) -> String {
    task.chars().take(40).collect()
}

/// Build a viewer-owned log path
/// ($HOME/.local/state/agent-viewer/logs/{prefix}-{now_ms}.log). Creates nothing;
/// the spawn helper makes the parent dir when it actually opens the file.
pub fn viewer_log_path(prefix: &str) -> std::path::PathBuf {
    crate::home_dir()
        .join(".local/state/agent-viewer/logs")
        .join(format!("{prefix}-{}.log", now_ms()))
}

/// Run a command to completion; non-zero exit -> Err(Error::Command(stderr)).
/// The shared shape for every shell-out mutation (codex archive/unarchive).
pub(crate) fn run_checked(cmd: &mut std::process::Command) -> Result<()> {
    let output = cmd.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Command(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ))
    }
}

/// How long a model-discovery shell-out (`codex debug models`) may take.
/// Generous because these run on a worker thread, never the render loop: a cold probe can
/// take seconds on this box, and a deadline it can lose silently empties the composer's
/// picker down to the built-in default.
pub const MODEL_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Run `cmd`, returning captured stdout as a String if it exits 0 within `timeout`.
/// On timeout the child is killed; any failure (spawn error, non-zero exit, timeout,
/// non-utf8 stdout) returns None. Used to bound the best-effort model-discovery shell-outs
/// (`codex debug models`) so a hung CLI cannot freeze the caller
/// indefinitely: there, every failure means the same thing (no catalog today), so the reason
/// is genuinely not worth carrying.
pub(crate) fn run_with_timeout(
    cmd: std::process::Command,
    timeout: std::time::Duration,
) -> Option<String> {
    run_reporting_failure(cmd, timeout).ok()
}

/// How long a failed command's stderr may take to arrive once the child is gone. The pipe is
/// at EOF by then, so this is slack for the reader thread to finish, not a real wait; a
/// descendant still holding the pipe open must cost the caller this and no more.
const STDERR_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Ceiling on captured stderr. The output becomes one footer notice, so more than this is
/// never shown and only serves to let a chatty descendant grow the buffer unboundedly.
const STDERR_CAP: usize = 8 * 1024;

/// Ceiling on captured stdout. Model catalogs can exceed the OS pipe buffer, so the reader
/// continues draining after this limit while retaining at most 384 KiB in memory.
const STDOUT_CAP: usize = 384 * 1024;

/// `run_with_timeout` for callers that must TELL THE USER why it failed, returning the
/// failure as a one-line diagnostic naming the command.
///
/// The Option-returning wrapper above is right for best-effort discovery, where every failure
/// is equally "no catalog today". It is wrong wherever the failure is the answer: a spawn that
/// the user must act on has to say whether codex was missing, exited non-zero (with its
/// stderr), or hung, not just that something did not work.
pub(crate) fn run_reporting_failure(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> std::result::Result<String, String> {
    use std::io::Read;
    let described = describe(&cmd);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run `{described}`: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("`{described}` gave no stdout pipe"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("`{described}` gave no stderr pipe"))?;
    // A channel, not a join handle: a grandchild inheriting the pipe can hold it open past
    // the child's exit, and joining on that would block the caller with no deadline at all.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8 * 1024];
        let mut truncated = false;
        let read = loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break true,
                Ok(n) => {
                    let retained = n.min(STDOUT_CAP.saturating_sub(buf.len()));
                    buf.extend_from_slice(&chunk[..retained]);
                    truncated |= retained < n;
                }
                Err(_) => break false,
            }
        };
        let _ = tx.send(read.then_some((buf, truncated)));
    });
    // stderr gets its OWN reader thread for both of the reasons stdout does: a command that
    // writes more than the pipe buffer (64KB) blocks on write until someone reads, and a
    // descendant that inherited the pipe can hold it open past the parent's exit. Draining it
    // inline after the wait would turn either case into a hang past the deadline.
    //
    // It accumulates into SHARED memory rather than sending one message at EOF, because the
    // descendant case is exactly when the diagnostic matters and exactly when EOF never
    // comes: the parent's own error message is already in the buffer, and a channel that only
    // fires on EOF would throw it away and report a bare exit code.
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let writer = std::sync::Arc::clone(&collected);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if let Ok(mut buf) = writer.lock() {
                // Cap it: this is a one-line diagnostic for a footer, not a log sink, and a
                // chatty descendant must not grow the buffer without bound.
                if buf.len() < STDERR_CAP {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    });
    let stderr_so_far = move || {
        collected
            .lock()
            .map(|buf| String::from_utf8_lossy(&buf).trim().to_string())
            .unwrap_or_default()
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    // The child is gone, so its own output is written; the grace is only slack
                    // for the reader thread to move the last chunk across. Whatever arrived is
                    // used either way, so a descendant holding the pipe open costs the wait
                    // but never the message.
                    std::thread::sleep(STDERR_DRAIN_GRACE);
                    let why = stderr_so_far();
                    let why = if why.is_empty() {
                        String::new()
                    } else {
                        format!(": {why}")
                    };
                    return Err(format!("`{described}` exited {status}{why}"));
                }
                // The child is gone, so the pipe is at EOF and the reader is about to finish;
                // the floor keeps a child that exits right on the deadline from losing its
                // already-buffered output to a zero-length wait.
                let remaining = deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .max(std::time::Duration::from_secs(1));
                let (buf, truncated) = rx
                    .recv_timeout(remaining)
                    .ok()
                    .flatten()
                    .ok_or_else(|| format!("`{described}` stdout was unreadable"))?;
                if truncated {
                    return Err(format!(
                        "`{described}` stdout was truncated after {STDOUT_CAP} bytes"
                    ));
                }
                return String::from_utf8(buf)
                    .map_err(|_| format!("`{described}` printed non-utf8 stdout"));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("`{described}` timed out after {timeout:?}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(format!("could not wait for `{described}`: {e}")),
        }
    }
}

/// PURE: the program plus its args, for naming a command in an error the user reads.
fn describe(cmd: &std::process::Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().to_string()];
    parts.extend(cmd.get_args().map(|a| a.to_string_lossy().to_string()));
    parts.join(" ")
}

/// Shared detached spawn helper for every backend.
/// Unix starts a new session. Windows uses detached process creation flags.
/// Standard input is null and output is appended to the log path. The child is not waited.
/// Returns the child PID.
pub fn spawn_detached(mut cmd: std::process::Command, log_path: &std::path::Path) -> Result<u32> {
    let log = open_spawn_log(log_path)?;
    let log_err = log.try_clone()?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: setsid() is async-signal-safe and the only work done in the child between
        // fork and exec; it detaches the spawned process into its own session (no ctty).
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    Ok(reap_detached(cmd.spawn()?))
}

/// Create (or reopen) a spawn log, readable only by its owner on unix.
///
/// These logs are the backend's raw stdout/stderr: the task prompt, file contents it printed,
/// occasionally a token it echoed. Created under the default umask they landed 0644 inside a
/// 0755 directory, i.e. readable by every account on the box, so the mode is set explicitly at
/// creation time (an existing file/dir keeps whatever mode it already has).
#[cfg(unix)]
fn open_spawn_log(log_path: &std::path::Path) -> Result<std::fs::File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    if let Some(parent) = log_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)?)
}

#[cfg(not(unix))]
fn open_spawn_log(log_path: &std::path::Path) -> Result<std::fs::File> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?)
}

/// Hand a detached child to a thread that blocks in `wait()`, returning its pid.
///
/// `setsid` detaches the SESSION, not the parent-child relationship: this process stays the
/// child's parent, so dropping the `Child` never collects its exit status and the kernel keeps
/// the entry as a zombie for the whole life of the viewer (two accumulated in 22 minutes of
/// ordinary use). The thread costs one stack and lives exactly as long as the child.
///
/// Reaping does free the pid for reuse, which is why `terminate` re-checks `/proc/<pid>/comm`
/// before signalling anything.
fn reap_detached(mut child: std::process::Child) -> u32 {
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    pid
}

/// SIGTERM with a pid-reuse guard: read /proc/<pid>/comm; it must start with
/// `expected_comm_prefix` ("codex"), else Err(Command("comm mismatch")).
/// If getpgid(pid) == pid (process leads its own group) send SIGTERM to the group
/// (-pid), else to the single pid. ESRCH (already gone) -> Ok(()). Never SIGKILL in v2.
#[cfg(target_os = "linux")]
pub fn terminate(pid: u32, expected_comm_prefix: &str) -> Result<()> {
    // pid-reuse guard: the live comm must still be the tool we spawned.
    let comm = match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) => comm,
        // The process is already gone; nothing to signal.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if !comm.trim_start().starts_with(expected_comm_prefix) {
        return Err(Error::Command("comm mismatch".into()));
    }
    let pid = pid as libc::pid_t;
    // getpgid(pid) == pid means the process leads its own group (our setsid spawns and
    // shell foreground jobs) — signal the whole group; otherwise only the single pid.
    let pgid = unsafe { libc::getpgid(pid) };
    let target = if pgid == pid { -pid } else { pid };
    let ret = unsafe { libc::kill(target, libc::SIGTERM) };
    if ret == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(Error::Command(format!("kill failed: {err}")))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn terminate(_pid: u32, _expected_comm_prefix: &str) -> Result<()> {
    Err(Error::Unsupported("process termination"))
}

#[cfg(test)]
mod tests {
    use super::{run_reporting_failure, run_with_timeout};
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// A detached spawn must not leave a zombie. `setsid` detaches the session, not the parent
    /// relationship, so a dropped `Child` is never waited on and the process table keeps the
    /// entry forever (measured live: two zombies in 22 minutes). Polling `/proc/<pid>` is the
    /// direct evidence — a zombie still HAS a `/proc` entry, so the assertion is that the entry
    /// disappears, not merely that the child exited.
    #[cfg(target_os = "linux")]
    #[test]
    fn spawn_detached_reaps_its_child_instead_of_leaving_a_zombie() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let log = dir.path().join("logs/child.log");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 0");
        let pid = super::spawn_detached(cmd, &log).expect("spawn");

        let deadline = Instant::now() + Duration::from_secs(5);
        while let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            assert!(
                Instant::now() < deadline,
                "pid {pid} still in the process table after 5s: {stat}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Spawn logs carry the backend's raw output (the task prompt, whatever it printed), so they
    /// are owner-only. Under the default umask they were created 0644 in a 0755 directory.
    #[cfg(unix)]
    #[test]
    fn spawn_detached_logs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().expect("tempdir");
        let log = dir.path().join("logs/child.log");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 0");
        super::spawn_detached(cmd, &log).expect("spawn");

        let file_mode = std::fs::metadata(&log)
            .expect("log exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "log file mode {file_mode:o}");
        let dir_mode = std::fs::metadata(log.parent().expect("log dir"))
            .expect("log dir exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "log dir mode {dir_mode:o}");
    }

    #[test]
    fn run_with_timeout_captures_stdout_on_success() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf hi");
        assert_eq!(
            run_with_timeout(cmd, Duration::from_secs(3)),
            Some("hi".to_string())
        );
    }

    /// The Option wrapper throws away WHY, which is correct for best-effort discovery and
    /// wrong for a spawn refusal the user has to act on. These pin the three failures that a
    /// generic "no daemon" notice used to hide.
    #[test]
    fn run_reporting_failure_names_the_command_and_the_reason() {
        let mut missing = Command::new("definitely-not-a-real-binary-xyzzy");
        missing.arg("start");
        let err = run_reporting_failure(missing, Duration::from_secs(3)).unwrap_err();
        assert!(
            err.contains("definitely-not-a-real-binary-xyzzy start"),
            "a missing binary must name itself, got {err:?}"
        );

        let mut failing = Command::new("sh");
        failing.arg("-c").arg("echo 'config is broken' >&2; exit 3");
        let err = run_reporting_failure(failing, Duration::from_secs(3)).unwrap_err();
        assert!(
            err.contains("config is broken"),
            "a non-zero exit must carry its stderr, got {err:?}"
        );

        let mut hanging = Command::new("sh");
        hanging.arg("-c").arg("sleep 5");
        let err = run_reporting_failure(hanging, Duration::from_millis(300)).unwrap_err();
        assert!(
            err.contains("timed out"),
            "a hung command must say so, got {err:?}"
        );
    }

    /// Both pipes need a concurrent reader. A command that writes more than the pipe buffer
    /// to stderr blocks on write until someone reads it, so draining stderr only after the
    /// child exits deadlocks until the deadline and reports a timeout instead of the error.
    #[test]
    fn run_reporting_failure_survives_stderr_larger_than_the_pipe_buffer() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("yes stderr-noise | head -n 20000 >&2; exit 4");
        let start = Instant::now();
        let err = run_reporting_failure(cmd, Duration::from_secs(10)).unwrap_err();
        assert!(
            err.contains("exited") && err.contains("stderr-noise"),
            "must report the exit and its stderr, got {} chars: {:?}",
            err.len(),
            &err[..err.len().min(120)]
        );
        assert!(
            !err.contains("timed out"),
            "a big stderr must not be mistaken for a hang"
        );
        assert!(start.elapsed() < Duration::from_secs(5), "took too long");
    }

    /// A descendant that inherited stderr holds the pipe open after the parent exits, so EOF
    /// never comes. The parent's OWN diagnostic is already in the buffer by then, and that is
    /// the whole point of the capture: waiting for EOF would discard it and report a bare
    /// exit code, and waiting at all must stay bounded.
    #[test]
    fn run_reporting_failure_keeps_stderr_a_grandchild_never_closes() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo 'the real reason' >&2; sh -c 'sleep 30' & exit 5");
        let start = Instant::now();
        let err = run_reporting_failure(cmd, Duration::from_secs(30)).unwrap_err();
        assert!(
            err.contains("the real reason"),
            "the parent's own diagnostic must survive a pipe that never reaches EOF, got {err:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "a held-open stderr pipe must not stall the caller, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn run_with_timeout_returns_none_on_nonzero_exit() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 1");
        assert_eq!(run_with_timeout(cmd, Duration::from_secs(3)), None);
    }

    #[test]
    fn run_with_timeout_captures_output_larger_than_the_pipe_buffer() {
        // A model catalog can exceed the OS pipe buffer (64KB on Linux). Draining stdout
        // only after the child exits deadlocks: the child blocks writing, never exits, and
        // the deadline kills it, so a big catalog would silently discover nothing.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("yes provider/model | head -n 20000");
        let start = Instant::now();
        let out = run_with_timeout(cmd, Duration::from_secs(10)).expect("captured stdout");
        assert_eq!(out.lines().count(), 20_000);
        assert!(out.len() > 256 * 1024, "expected >256KB, got {}", out.len());
        // Success must come from the child exiting, not from riding the deadline out.
        assert!(start.elapsed() < Duration::from_secs(5), "took too long");
    }

    #[test]
    fn run_with_timeout_kills_and_returns_none_past_deadline() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5");
        let start = Instant::now();
        let out = run_with_timeout(cmd, Duration::from_millis(300));
        let elapsed = start.elapsed();
        assert_eq!(out, None);
        // The child must be killed well before its own 5s sleep would end.
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }
}
