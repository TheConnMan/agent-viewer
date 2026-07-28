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
    crate::spawn::run_checked(std::process::Command::new("codex").args(args))
}

/// Build (do not run) `codex --no-alt-screen resume <id>` with inherited stdio.
pub fn resume_command(id: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("--no-alt-screen").arg("resume").arg(id);
    cmd
}

/// Build (do not run) `codex --no-alt-screen resume --remote <endpoint> <id>`: the TUI joins
/// the app-server
/// that HOSTS the thread instead of starting its own ThreadManager, which is what makes the
/// join live rather than a replay that fabricates an interrupt.
pub fn resume_remote_command(endpoint: &str, id: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("--no-alt-screen")
        .arg("resume")
        .arg("--remote")
        .arg(endpoint)
        .arg(id);
    cmd
}

/// PURE builder (unit-tested): one JSON-RPC 2.0 request line for
/// `thread/name/set` {threadId, name}, serialized via serde_json (no hand-built
/// strings — names with quotes/newlines must survive).
pub fn name_set_request(request_id: i64, thread_id: &str, name: &str) -> String {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "thread/name/set",
        "params": { "threadId": thread_id, "name": name },
    });
    request.to_string()
}

/// Spawn `codex app-server` (stdio mode), write the initialize handshake +
/// `name_set_request`, read until the matching response id or 5s timeout, then drop
/// the child. LIVE VERIFICATION REQUIRED during implementation (framing + initialize
/// shape against `codex app-server generate-json-schema`).
pub fn rename(thread_id: &str, name: &str) -> Result<()> {
    // Framing verified live against 0.144.1: line-delimited JSON over stdio, an
    // `initialize` request carrying clientInfo, then `thread/name/set`. No Content-Length,
    // no separate `initialized` notification required. The server preserves stdin order,
    // so the two requests can be written back to back.
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let mut child = Command::new("codex")
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Command("app-server stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Command("app-server stdout unavailable".into()))?;

    // A reader thread streams response lines over a channel so the wait can be bounded.
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "agent-viewer", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true },
        },
    })
    .to_string();
    let name_set = name_set_request(2, thread_id, name);

    let outcome = (|| -> Result<()> {
        writeln!(stdin, "{initialize}")?;
        writeln!(stdin, "{name_set}")?;
        stdin.flush()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| Error::Command("app-server rename timed out".into()))?;
            let line = rx
                .recv_timeout(remaining)
                .map_err(|_| Error::Command("app-server rename timed out".into()))?;
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(|v| v.as_i64()) == Some(2) {
                if value.get("error").is_some() {
                    return Err(Error::Command(format!("app-server rename error: {line}")));
                }
                return Ok(());
            }
        }
    })();

    // Closing stdin signals EOF; kill+reap the child and join the reader regardless.
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    outcome
}
