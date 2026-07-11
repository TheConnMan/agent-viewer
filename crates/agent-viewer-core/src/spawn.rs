use crate::error::{Error, Result};

/// Current time as epoch milliseconds (0 if the system clock predates the epoch).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
/// The shared shape for every shell-out mutation (codex archive/unarchive,
/// opencode delete/rename).
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

/// Run `cmd`, returning captured stdout as a String if it exits 0 within `timeout`.
/// On timeout the child is killed; any failure (spawn error, non-zero exit, timeout,
/// non-utf8 stdout) returns None. Poll-based (no new deps): spawn with piped stdout and
/// null stderr, then loop `try_wait()` with a short sleep until the deadline. Used to
/// bound the best-effort model-discovery shell-outs (`codex debug models`,
/// `opencode models`) so a hung CLI cannot freeze the caller indefinitely.
pub(crate) fn run_with_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> Option<String> {
    use std::io::Read;
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut buf = String::new();
                child.stdout.take()?.read_to_string(&mut buf).ok()?;
                return Some(buf);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Shared detached-spawn helper (codex + opencode; claude self-detaches):
/// unsafe pre_exec calling libc::setsid() (new session, no ctty); stdin Stdio::null();
/// stdout+stderr appended to log_path (parent dir created if missing); do NOT wait.
/// Returns the child PID.
pub fn spawn_detached(mut cmd: std::process::Command, log_path: &std::path::Path) -> Result<u32> {
    use std::os::unix::process::CommandExt;

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let log_err = log.try_clone()?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    // SAFETY: setsid() is async-signal-safe and the only work done in the child between
    // fork and exec; it detaches the spawned process into its own session (no ctty).
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok(child.id())
}

/// SIGTERM with a pid-reuse guard: read /proc/<pid>/comm; it must start with
/// `expected_comm_prefix` ("codex" / "opencode"), else Err(Command("comm mismatch")).
/// If getpgid(pid) == pid (process leads its own group) send SIGTERM to the group
/// (-pid), else to the single pid. ESRCH (already gone) -> Ok(()). Never SIGKILL in v2.
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

#[cfg(test)]
mod tests {
    use super::run_with_timeout;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn run_with_timeout_captures_stdout_on_success() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf hi");
        assert_eq!(
            run_with_timeout(cmd, Duration::from_secs(3)),
            Some("hi".to_string())
        );
    }

    #[test]
    fn run_with_timeout_returns_none_on_nonzero_exit() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 1");
        assert_eq!(run_with_timeout(cmd, Duration::from_secs(3)), None);
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
