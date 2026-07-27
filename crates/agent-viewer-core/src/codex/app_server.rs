//! Client for the `codex app-server` daemon: discovering it, the pure JSON-RPC request
//! builders, and the blocking WebSocket exchange that starts and interrupts the threads the
//! viewer spawns.
//!
//! Why this exists. `codex exec` runs its app-server IN PROCESS, so an exec-spawned thread can
//! never be joined from another process, and `codex resume <id>` on a thread that is currently
//! running does not join it either: the resuming process gets its own ThreadManager, replays
//! the rollout, finds it ends mid-turn and appends a synthesized `TurnAborted{Interrupted}`,
//! which the TUI renders as "Conversation interrupted". Verified live on codex-cli 0.145.0 - a
//! foreign resume of a live thread reported `status: idle` with turn status "interrupted"
//! while the real session kept running. A true join exists only INSIDE the one app-server
//! process that hosts the thread, reached with `codex resume --remote unix://<socket> <id>`,
//! so viewer-spawned sessions are started on the shared daemon. See SPEC.md
//! "Codex attach/resume".
//!
//! Wire shapes come from `codex app-server generate-json-schema` (codex-cli 0.145.0):
//! `ThreadStartParams` carries `cwd`, `sandbox` (SandboxMode), `approvalPolicy`
//! (AskForApproval) and `model`; `TurnStartParams` requires `threadId` plus `input`, where a
//! text input is `{"type":"text","text":...}`; `TurnInterruptParams` requires BOTH `threadId`
//! and `turnId`; `thread/read` with `includeTurns` returns `thread.turns[]` as `{id, status}`.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A reachable app-server daemon, identified by the control socket it listens on. The path is
/// DISCOVERED from `codex app-server daemon version`, never hardcoded (the same rule as the
/// `state_*.sqlite` glob).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Daemon {
    pub socket_path: PathBuf,
}

/// The WebSocket handshake target. The control socket is a Unix socket that upgrades to
/// RFC6455 at `/rpc`; the host is meaningless over a Unix socket but the HTTP request needs
/// one. No compression is offered: the daemon rejects a client advertising
/// `permessage-deflate` with "Missing, duplicated or incorrect header
/// sec-websocket-extensions" and drops the connection, and tungstenite offers none.
const HANDSHAKE_URL: &str = "ws://localhost/rpc";

/// How long one whole exchange (connect, initialize, and the request) may take. Generous
/// against a busy daemon but finite: this runs on the TUI's background worker and on the
/// attach key path, so it can never be allowed to block forever. `thread/start` answered in
/// 0.16s on this box.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the polite close on drop may take. Short on purpose: the connection is finished
/// with, and a wedged peer must not hold the caller in `drop`.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(250);

/// Read slice while waiting for a response. tungstenite hands a timed-out read back as
/// `WouldBlock` for the caller to retry, so the deadline is enforced by the retry loop rather
/// than by one long blocking read.
const READ_SLICE: Duration = Duration::from_millis(250);

/// `codex app-server daemon version` is a fast local probe (34ms measured on this box); the
/// deadline only exists so a wedged CLI cannot hang an attach keypress.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Budget for `codex app-server daemon start` plus the re-probes that follow it. The start
/// itself returned in 0.5s on this box, and it prints `"status":"started"` rather than
/// `"running"`, so availability is confirmed by re-probing, not by that output. Deliberately
/// tighter than `MODEL_PROBE_TIMEOUT`: a wedged start must surface as a failed spawn the user
/// can act on rather than freeze the composer.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// PURE: the daemon described by `codex app-server daemon version` stdout, or None when no
/// daemon is usable. The availability gate is `status == "running"` AND a non-empty
/// `socketPath`; with no daemon listening the command exits non-zero and prints prose, so
/// anything unparseable is also None. Never panics.
pub fn parse_daemon_version(stdout: &str) -> Option<Daemon> {
    let value: serde_json::Value = serde_json::from_str(stdout).ok()?;
    if crate::json_str(&value, "status") != Some("running") {
        return None;
    }
    let socket_path = crate::json_str(&value, "socketPath")?;
    if socket_path.is_empty() {
        return None;
    }
    Some(Daemon {
        socket_path: PathBuf::from(socket_path),
    })
}

/// PURE: the endpoint `codex resume --remote <endpoint> <id>` takes for this daemon.
pub fn remote_endpoint(daemon: &Daemon) -> String {
    format!("unix://{}", daemon.socket_path.display())
}

/// PURE builder: the JSON-RPC `initialize` handshake every connection opens with. The daemon
/// logs `clientInfo`, so the viewer identifies itself by name.
pub fn initialize_request(id: i64) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "agent-viewer", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true },
        },
    })
    .to_string()
}

/// PURE builder: `thread/start` for a viewer-spawned session.
///
/// `sandbox` is pinned to `danger-full-access` and `approvalPolicy` to `never`, for the same
/// reason `SANDBOX_ARGS` bypasses the sandbox on the exec path: under any sandbox codex mounts
/// `.git` READ-ONLY, so every git-shaped task this viewer spawns (fetch, branch, worktree,
/// commit) becomes a silent no-op that still burns a turn and still reports "done". Verified
/// live on codex-cli 0.145.0 - a workspace write exits 0 while a `.git` write exits 1. An
/// approval prompt likewise has nobody to answer it on a viewer-spawned thread. See SPEC.md
/// "Spawn sandbox posture".
///
/// `model` None omits the key entirely so codex resolves its own default; sending the
/// composer's placeholder label ("default") would be sending a bogus model id.
pub fn thread_start_request(id: i64, cwd: &Path, model: Option<&str>) -> String {
    let mut params = serde_json::json!({
        "cwd": cwd.to_string_lossy(),
        "sandbox": "danger-full-access",
        "approvalPolicy": "never",
    });
    if let (Some(model), Some(params)) = (model, params.as_object_mut()) {
        params.insert("model".to_string(), serde_json::json!(model));
    }
    request(id, "thread/start", params)
}

/// PURE builder: `turn/start` carrying the task as one text UserInput.
pub fn turn_start_request(id: i64, thread_id: &str, task: &str) -> String {
    request(
        id,
        "turn/start",
        serde_json::json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": task }],
        }),
    )
}

/// PURE builder: `turn/interrupt`, which needs BOTH the thread and the turn to interrupt.
pub fn turn_interrupt_request(id: i64, thread_id: &str, turn_id: &str) -> String {
    request(
        id,
        "turn/interrupt",
        serde_json::json!({ "threadId": thread_id, "turnId": turn_id }),
    )
}

/// PURE builder: `thread/read` including the turn list, which is how the in-progress turn id
/// an interrupt needs is discovered.
fn thread_read_request(id: i64, thread_id: &str) -> String {
    request(
        id,
        "thread/read",
        serde_json::json!({ "threadId": thread_id, "includeTurns": true }),
    )
}

/// PURE: one JSON-RPC 2.0 request line, serialized through serde_json rather than formatted
/// (a task or name carrying quotes or newlines must survive).
fn request(id: i64, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

/// PURE: the new thread's id from a `thread/start` response line (`result.thread.id`), or None
/// for a line that is not this request's successful response. The daemon multiplexes
/// notifications and other requests' responses onto the same stream, so the id correlation is
/// load-bearing. Never panics.
pub fn parse_thread_id(line: &str, expect_id: i64) -> Option<String> {
    let value = response_for(line, expect_id)?;
    value
        .pointer("/result/thread/id")
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

/// PURE: the id of the one turn still in progress in a `thread/read` response, or None when
/// the thread has no live turn (nothing to interrupt). Never panics.
fn parse_in_progress_turn(line: &str, expect_id: i64) -> Option<String> {
    let value = response_for(line, expect_id)?;
    let turns = value.pointer("/result/thread/turns")?.as_array()?;
    turns
        .iter()
        .rev()
        .find(|turn| crate::json_str(turn, "status") == Some("inProgress"))
        .and_then(|turn| crate::json_str(turn, "id"))
        .map(str::to_string)
}

/// PURE: `line` parsed as this request's successful response, or None when it is a
/// notification, another request's response, an error payload, or unparseable.
fn response_for(line: &str, expect_id: i64) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("id").and_then(|id| id.as_i64()) != Some(expect_id) {
        return None;
    }
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return None;
    }
    Some(value)
}

/// IMPURE: the daemon currently listening, or None. Probe ONLY - this never starts anything,
/// which is what `stop` and `attach` need: if no daemon answers, a thread it hosted is not
/// hosted any more, so there is nothing to join and nothing to interrupt.
pub fn probe_daemon() -> Option<Daemon> {
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("app-server").arg("daemon").arg("version");
    let stdout = crate::spawn::run_with_timeout(cmd, PROBE_TIMEOUT)?;
    parse_daemon_version(&stdout)
}

/// PURE: the working directory a viewer-started daemon must run in, given the home directory
/// to prefer.
///
/// The daemon is long-lived, shared box-wide, and inherits the cwd of whoever started it.
/// agent-viewer routinely runs from a git worktree under `.worktrees/` that is DELETED as soon
/// as its branch merges, so a daemon that inherited the viewer's cwd outlives its own working
/// directory. Measured live on 2026-07-27: the daemon's `/proc/<pid>/cwd` pointed at a deleted
/// worktree, `daemon version` still answered `"status":"running"`, and every single
/// `thread/start` failed with `failed to load configuration: No such file or directory`. That
/// poisoned daemon then broke spawning for hours and for every other codex client on the box.
///
/// Home is stable for the life of the box; `/` is the fallback when HOME is unset or gone.
/// Never the viewer's own cwd.
pub fn stable_daemon_cwd(home: PathBuf) -> PathBuf {
    if home.is_dir() {
        home
    } else {
        PathBuf::from("/")
    }
}

/// PURE builder: `codex app-server daemon start`, pinned to a cwd it cannot outlive.
pub fn daemon_start_command(cwd: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("app-server").arg("daemon").arg("start");
    cmd.current_dir(cwd);
    cmd
}

/// IMPURE: the daemon to spawn on, starting one if none is listening. `codex app-server daemon
/// start` is idempotent, and this NEVER stops or restarts a daemon - other clients (and every
/// other thread it hosts) live in that process. Its output says `"started"`, not `"running"`,
/// so availability is confirmed by re-probing until the socket answers or the deadline passes.
pub fn ensure_daemon() -> Option<Daemon> {
    if let Some(daemon) = probe_daemon() {
        return Some(daemon);
    }
    let cmd = daemon_start_command(&stable_daemon_cwd(crate::home_dir()));
    let deadline = Instant::now() + START_TIMEOUT;
    crate::spawn::run_with_timeout(cmd, START_TIMEOUT);
    loop {
        if let Some(daemon) = probe_daemon() {
            return Some(daemon);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// How far a daemon spawn got. Every non-`Started` outcome is now a hard, visible failure (see
/// `spawn_route`), so the distinction survives to tell the user WHICH half broke: a
/// `TurnFailed` left a real thread in the listing that they can attach to and retry, while
/// `NotCreated` left nothing behind at all.
#[derive(Debug)]
pub enum SpawnAttempt {
    /// Thread created and its first turn accepted.
    Started(String),
    /// The thread exists but its first turn did not start. There is a row either way, so this
    /// must be surfaced against that thread, never retried as a second spawn.
    TurnFailed { thread_id: String, error: Error },
    /// Nothing was created (no daemon, no connection, no thread).
    NotCreated(Error),
}

/// IMPURE: start a thread on `daemon` and kick off its first turn. The turn keeps running after
/// this client disconnects (verified live: the rollout grew 3778 bytes post-disconnect and the
/// turn ran to completion), so a spawn is exactly connect, start, turn, disconnect.
pub fn try_spawn_thread(
    daemon: &Daemon,
    cwd: &Path,
    task: &str,
    model: Option<&str>,
) -> SpawnAttempt {
    let deadline = Instant::now() + RPC_TIMEOUT;
    let mut client = match Client::connect(daemon, deadline) {
        Ok(client) => client,
        Err(error) => return SpawnAttempt::NotCreated(error),
    };
    let thread_id = match client.request(2, &thread_start_request(2, cwd, model), deadline) {
        Err(error) => return SpawnAttempt::NotCreated(error),
        Ok(line) => match parse_thread_id(&line, 2) {
            Some(thread_id) => thread_id,
            // A response with no thread id means no thread: nothing to double up on.
            None => {
                return SpawnAttempt::NotCreated(Error::Command(format!(
                    "app-server thread/start returned no thread id: {line}"
                )));
            }
        },
    };
    // `turn/start` answers immediately with the new turn (`status: inProgress`), so waiting
    // for the response confirms the turn was accepted without waiting for it to finish.
    match client.request(3, &turn_start_request(3, &thread_id, task), deadline) {
        Ok(_) => SpawnAttempt::Started(thread_id),
        Err(error) => SpawnAttempt::TurnFailed { thread_id, error },
    }
}

/// IMPURE: `try_spawn_thread` for callers that only need the id or an error (the live test).
pub fn spawn_thread(
    daemon: &Daemon,
    cwd: &Path,
    task: &str,
    model: Option<&str>,
) -> Result<String> {
    match try_spawn_thread(daemon, cwd, task, model) {
        SpawnAttempt::Started(thread_id) => Ok(thread_id),
        SpawnAttempt::TurnFailed { thread_id, error } => Err(Error::Command(format!(
            "app-server started thread {thread_id} but its first turn failed: {error}"
        ))),
        SpawnAttempt::NotCreated(error) => Err(error),
    }
}

/// IMPURE: interrupt whatever turn is in progress on `thread_id`. `turn/interrupt` needs the
/// turn id, so the thread is read over the same connection first. A thread with no live turn
/// is a no-op success: there is nothing to stop, which is not an error.
pub fn interrupt_thread(daemon: &Daemon, thread_id: &str) -> Result<()> {
    let deadline = Instant::now() + RPC_TIMEOUT;
    let mut client = Client::connect(daemon, deadline)?;
    let line = client.request(2, &thread_read_request(2, thread_id), deadline)?;
    let Some(turn_id) = parse_in_progress_turn(&line, 2) else {
        return Ok(());
    };
    match client.request(3, &turn_interrupt_request(3, thread_id, &turn_id), deadline) {
        Ok(_) => Ok(()),
        // The turn can finish between the read and the interrupt, and the daemon answers that
        // with `{"code":-32600,"message":"no active turn to interrupt"}` (captured live). That
        // is the same nothing-to-stop case as the None above, so it must read as success:
        // otherwise the footer says "stop failed" for a session that did stop.
        Err(error) if is_no_active_turn(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// PURE: is this the daemon's "the turn is already over" answer rather than a real failure?
fn is_no_active_turn(error: &Error) -> bool {
    error.to_string().contains("no active turn to interrupt")
}

/// One blocking JSON-RPC connection to the daemon: a tungstenite WebSocket over the control
/// socket, already through `initialize`. Every exchange is bounded by a caller-supplied
/// deadline, and dropping the client closes the connection.
struct Client {
    socket: tungstenite::WebSocket<std::os::unix::net::UnixStream>,
}

impl Client {
    /// Connect, upgrade, and complete the `initialize` handshake. The house style here is
    /// `cli::rename`'s bounded exchange: initialize carrying clientInfo, then the real
    /// request, reading until the matching id or the deadline.
    fn connect(daemon: &Daemon, deadline: Instant) -> Result<Client> {
        let stream = std::os::unix::net::UnixStream::connect(&daemon.socket_path)?;
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let (socket, _response) = tungstenite::client::client(HANDSHAKE_URL, stream)
            .map_err(|e| Error::Command(format!("app-server handshake failed: {e}")))?;
        // Past the handshake, reads poll in slices so the deadline (not the socket) decides
        // when to give up.
        socket.get_ref().set_read_timeout(Some(READ_SLICE))?;
        let mut client = Client { socket };
        client.request(1, &initialize_request(1), deadline)?;
        Ok(client)
    }

    /// Send one request and return the response line carrying the matching id. An error
    /// payload for that id is an Err; notifications and other ids are skipped.
    fn request(&mut self, id: i64, line: &str, deadline: Instant) -> Result<String> {
        self.socket
            .send(tungstenite::Message::text(line.to_string()))
            .map_err(|e| Error::Command(format!("app-server write failed: {e}")))?;
        loop {
            let line = self.read_line(deadline)?;
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(|v| v.as_i64()) != Some(id) {
                continue;
            }
            // An explicit `"error": null` alongside a result is success, not failure.
            if value.get("error").is_some_and(|error| !error.is_null()) {
                return Err(Error::Command(format!("app-server error: {line}")));
            }
            return Ok(line);
        }
    }

    /// One text frame, retrying the timed-out reads tungstenite reports as `WouldBlock` until
    /// the deadline. Ping/pong and binary frames are skipped; a close frame ends the exchange.
    fn read_line(&mut self, deadline: Instant) -> Result<String> {
        loop {
            remaining(deadline)?;
            match self.socket.read() {
                Ok(tungstenite::Message::Text(text)) => return Ok(text.to_string()),
                Ok(tungstenite::Message::Close(_)) => {
                    return Err(Error::Command("app-server closed the connection".into()));
                }
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(Error::Command(format!("app-server read failed: {e}"))),
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Politely close, then let the stream drop. The daemon keeps every thread this
        // connection started running regardless. The write timeout is re-armed short first: the
        // socket still carries whatever deadline `connect` set, and a wedged peer would
        // otherwise stall the drop for all of it.
        let _ = self.socket.get_ref().set_write_timeout(Some(CLOSE_TIMEOUT));
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }
}

/// Time left before `deadline`, or Err once it has passed.
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Error::Command("app-server request timed out".into()));
    }
    Ok(left)
}

#[cfg(test)]
mod tests {
    use super::{
        Daemon, interrupt_thread, parse_in_progress_turn, spawn_thread, thread_read_request,
    };
    use std::path::{Path, PathBuf};

    /// A daemon whose socket is not there: the failure the exec fallback in `CodexBackend::spawn`
    /// depends on. It must be a clean Err rather than a hang or a panic, and no daemon is needed
    /// to prove it (connect fails with ENOENT).
    #[test]
    fn a_socket_that_is_not_there_is_an_error_not_a_hang() {
        let daemon = Daemon {
            socket_path: PathBuf::from("/nonexistent/agent-viewer/app-server.sock"),
        };
        let started = std::time::Instant::now();
        assert!(spawn_thread(&daemon, Path::new("/tmp"), "t", None).is_err());
        assert!(interrupt_thread(&daemon, "thread-abc").is_err());
        // Both must fail on connect, not ride out the 10s RPC deadline.
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn thread_read_request_asks_for_the_turns() {
        // Without `includeTurns` the response carries no turn ids, so an interrupt would have
        // nothing to address.
        let value: serde_json::Value =
            serde_json::from_str(&thread_read_request(2, "t-1")).unwrap();
        assert_eq!(value.pointer("/method").unwrap(), "thread/read");
        assert_eq!(value.pointer("/params/threadId").unwrap(), "t-1");
        assert_eq!(value.pointer("/params/includeTurns").unwrap(), true);
    }

    #[test]
    fn parse_in_progress_turn_finds_the_live_turn_and_ignores_finished_ones() {
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "thread": { "turns": [
                { "id": "turn-1", "status": "completed" },
                { "id": "turn-2", "status": "interrupted" },
                { "id": "turn-3", "status": "inProgress" },
            ] } },
        })
        .to_string();
        assert_eq!(parse_in_progress_turn(&line, 2), Some("turn-3".to_string()));
        // Another request's response is not ours to read a turn out of.
        assert_eq!(parse_in_progress_turn(&line, 3), None);
    }

    #[test]
    fn parse_in_progress_turn_is_none_when_nothing_is_running() {
        // A thread whose turns all finished has nothing to interrupt, which `interrupt_thread`
        // treats as a no-op success rather than an error.
        let finished = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "thread": { "turns": [{ "id": "turn-1", "status": "completed" }] } },
        })
        .to_string();
        assert_eq!(parse_in_progress_turn(&finished, 2), None);
        for line in [
            r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"turns":[]}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"bad params"}}"#,
            "{not json",
            "",
        ] {
            assert_eq!(parse_in_progress_turn(line, 2), None, "must reject: {line}");
        }
    }
}
