use crate::error::{Error, Result};

// NOTE: `codex app-server` (thread/subscribe, thread/list, command/exec/terminate) is the
// experimental JSON-RPC daemon and the v2 upgrade path for these mutations. v1 shells out
// to the stable `codex archive|unarchive|resume` subcommands, which touch the same state.

/// Run `codex archive <id>`; capture output; non-zero exit -> Err(Error::Command(stderr)).
/// Never touch the DB directly.
pub fn archive(id: &str) -> Result<()> {
    run_codex(&["archive", id])
}

/// Run `codex unarchive <id>`; capture output; non-zero exit -> Err(Error::Command(stderr)).
pub fn unarchive(id: &str) -> Result<()> {
    run_codex(&["unarchive", id])
}

fn run_codex(args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("codex").args(args).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Command(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ))
    }
}

/// Build (do not run) `codex resume <id>` with inherited stdio.
pub fn resume_command(id: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("resume").arg(id);
    cmd
}

/// PURE builder (unit-tested): one JSON-RPC 2.0 request line for
/// `thread/name/set` {threadId, name}, serialized via serde_json (no hand-built
/// strings — names with quotes/newlines must survive).
pub fn name_set_request(request_id: i64, thread_id: &str, name: &str) -> String {
    let _ = (request_id, thread_id, name);
    todo!("Stream A: serde_json JSON-RPC 2.0 request line for thread/name/set")
}

/// Spawn `codex app-server` (stdio mode), write the initialize handshake +
/// `name_set_request`, read until the matching response id or 5s timeout, then drop
/// the child. LIVE VERIFICATION REQUIRED during implementation (framing + initialize
/// shape against `codex app-server generate-json-schema`).
pub fn rename(thread_id: &str, name: &str) -> Result<()> {
    let _ = (thread_id, name);
    todo!("Stream A: codex app-server JSON-RPC rename")
}
