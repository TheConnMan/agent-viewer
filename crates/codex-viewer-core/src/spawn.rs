use crate::error::Result;

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
