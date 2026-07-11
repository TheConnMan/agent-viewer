use crate::error::Result;

/// Shared detached-spawn helper (codex + opencode; claude self-detaches):
/// unsafe pre_exec calling libc::setsid() (new session, no ctty); stdin Stdio::null();
/// stdout+stderr appended to log_path (parent dir created if missing); do NOT wait.
/// Returns the child PID.
pub fn spawn_detached(cmd: std::process::Command, log_path: &std::path::Path) -> Result<u32> {
    let _ = (cmd, log_path);
    todo!()
}
