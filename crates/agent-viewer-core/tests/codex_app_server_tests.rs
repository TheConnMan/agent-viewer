//! Pure `codex app-server` daemon helpers: parsing the version probe, building the
//! `--remote` endpoint, building the JSON-RPC requests, and parsing the `thread/start`
//! response. Nothing here opens a socket or spawns a process, so every case is a captured
//! fixture or a literal string.
//!
//! Wire shapes are evidence-backed, not guessed. Live captures live in
//! `tests/fixtures/codex_app_server/`; the param names and enum values come from
//! `codex app-server generate-json-schema` (codex-cli 0.145.x): `ThreadStartParams` carries
//! `cwd`, `sandbox` (SandboxMode: read-only | workspace-write | danger-full-access),
//! `approvalPolicy` (AskForApproval: untrusted | on-request | never) and `model`;
//! `TurnStartParams` requires `threadId` plus `input`, where a text input is
//! `{"type":"text","text":...}`; `TurnInterruptParams` requires BOTH `threadId` and `turnId`.

mod common;

use agent_viewer_core::codex::app_server::{
    CodexSkill, Daemon, SpawnAttempt, daemon_start_command, initialize_request,
    parse_daemon_version, parse_skills_list, parse_thread_id, remote_endpoint, skills_list_request,
    stable_daemon_cwd, thread_start_request, turn_interrupt_request, turn_start_request,
};
use agent_viewer_core::codex::spawn_failure_reason;
use agent_viewer_core::error::Error;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The socket path carried by the `daemon_version.json` live capture.
const SOCKET: &str = "/home/user/.codex/app-server-control/app-server-control.sock";

fn daemon() -> Daemon {
    Daemon {
        socket_path: PathBuf::from(SOCKET),
    }
}

/// Parse a builder's output back into JSON. Assertions go through this rather than string
/// matching, so key order and whitespace are never part of the contract.
fn json(line: &str) -> Value {
    serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("builder emitted invalid JSON ({e}): {line}"))
}

/// The JSON-RPC 2.0 envelope: protocol version, the caller's own id echoed back (the
/// correlation the reader loop depends on), and the method name.
fn assert_envelope(request: &Value, id: i64, method: &str) {
    assert_eq!(request.get("jsonrpc").and_then(Value::as_str), Some("2.0"));
    assert_eq!(request.get("id").and_then(Value::as_i64), Some(id));
    assert_eq!(request.get("method").and_then(Value::as_str), Some(method));
}

// --- daemon discovery ---

#[test]
fn parse_daemon_version_takes_the_socket_path_from_a_running_daemon() {
    // Live capture of `codex app-server daemon version`. The socket is DISCOVERED, never
    // hardcoded (same rule as the state_*.sqlite glob).
    let stdout = common::read_fixture("codex_app_server/daemon_version.json");
    assert_eq!(parse_daemon_version(&stdout), Some(daemon()));
}

#[test]
fn parse_daemon_version_needs_both_a_running_status_and_a_socket() {
    // The availability gate is `status == "running"` AND a usable `socketPath`. Anything
    // else must be None so the caller falls back to the exec path instead of dialing a
    // socket that is not there.
    let cases = [
        r#"{"status":"stopped","cliVersion":"0.144.4"}"#,
        r#"{"status":"starting","socketPath":"/tmp/x.sock"}"#,
        r#"{"cliVersion":"0.144.4","socketPath":"/tmp/x.sock"}"#,
        r#"{"status":"running","cliVersion":"0.144.4"}"#,
        r#"{"status":"running","socketPath":"","cliVersion":"0.144.4"}"#,
        r#"{"status":"running","socketPath":null}"#,
    ];
    for stdout in cases {
        assert_eq!(parse_daemon_version(stdout), None, "must reject: {stdout}");
    }
}

#[test]
fn parse_daemon_version_survives_the_no_daemon_output() {
    // With no daemon listening the command exits non-zero and prints prose, not JSON. The
    // parser sees whatever it is handed and must answer None rather than panic.
    let no_daemon = "failed to connect to \
        /home/user/.codex/app-server-control/app-server-control.sock: \
        No such file or directory";
    for stdout in [
        "",
        "   \n",
        no_daemon,
        "{not json",
        "[]",
        "null",
        "\"running\"",
    ] {
        assert_eq!(
            parse_daemon_version(stdout),
            None,
            "must reject: {stdout:?}"
        );
    }
}

#[test]
fn remote_endpoint_is_the_unix_scheme_socket_path() {
    // This exact string is what `codex resume --remote <endpoint> <id>` takes.
    assert_eq!(remote_endpoint(&daemon()), format!("unix://{SOCKET}"));
    // An absolute socket path therefore yields the triple-slash form.
    assert!(remote_endpoint(&daemon()).starts_with("unix:///"));
}

// --- request builders ---

#[test]
fn initialize_request_is_a_jsonrpc_handshake_naming_this_client() {
    let request = json(&initialize_request(1));
    assert_envelope(&request, 1, "initialize");
    assert_eq!(
        request
            .pointer("/params/clientInfo/name")
            .and_then(Value::as_str),
        Some("agent-viewer"),
        "the daemon logs clientInfo, so the viewer must identify itself"
    );
}

#[test]
fn thread_start_request_runs_the_thread_unsandboxed() {
    // Regression with the same reasoning as `codex_spawn_command_runs_unsandboxed`: under a
    // sandbox codex mounts `.git` READ-ONLY, so every git-shaped task this viewer spawns
    // (fetch, branch, worktree, commit) becomes a silent no-op that still burns a turn and
    // still reports "done". Verified live on codex-cli 0.145.0: a workspace write exits 0
    // while a `.git` write exits 1. The daemon spawn path must ask for the same posture the
    // exec path already uses. See SPEC.md "Spawn sandbox posture".
    let request = json(&thread_start_request(
        3,
        Path::new("/home/user/git/proj"),
        None,
    ));
    assert_envelope(&request, 3, "thread/start");
    assert_eq!(
        request.pointer("/params/cwd").and_then(Value::as_str),
        Some("/home/user/git/proj"),
        "the thread must start in the project dir, not the viewer's cwd"
    );
    assert_eq!(
        request.pointer("/params/sandbox").and_then(Value::as_str),
        Some("danger-full-access"),
        "any other SandboxMode re-imposes the read-only .git mount"
    );
    assert_eq!(
        request
            .pointer("/params/approvalPolicy")
            .and_then(Value::as_str),
        Some("never"),
        "an approval prompt has no one to answer it on a viewer-spawned thread"
    );
}

#[test]
fn thread_start_request_carries_a_model_only_when_one_was_picked() {
    let picked = json(&thread_start_request(
        4,
        Path::new("/tmp"),
        Some("gpt-5.3-codex"),
    ));
    assert_eq!(
        picked.pointer("/params/model").and_then(Value::as_str),
        Some("gpt-5.3-codex")
    );

    // None means "let codex resolve its own default", so the key must be ABSENT. Sending
    // the composer's placeholder label ("default", BackendKind::default_model) or a null
    // would be sending a bogus model id.
    let defaulted = json(&thread_start_request(5, Path::new("/tmp"), None));
    assert_eq!(defaulted.pointer("/params/model"), None);
}

#[test]
fn skills_list_request_uses_the_plural_cwds_array() {
    let request = json(&skills_list_request(6, Path::new("/home/user/git/proj")));
    assert_envelope(&request, 6, "skills/list");
    assert_eq!(
        request.pointer("/params/cwds"),
        Some(&serde_json::json!(["/home/user/git/proj"])),
        "skills/list requires the plural cwds array"
    );
    assert_eq!(
        request
            .pointer("/params/forceReload")
            .and_then(Value::as_bool),
        Some(false)
    );
    let params = request
        .pointer("/params")
        .and_then(Value::as_object)
        .expect("params is an object");
    assert_eq!(params.len(), 2, "only the installed schema fields are sent");
    assert!(
        !params.contains_key("cwd"),
        "the singular cwd field is not part of SkillsListParams"
    );
}

#[test]
fn parse_skills_list_keeps_enabled_names_and_paths() {
    let line = common::read_fixture("codex_app_server/skills_list.json");
    assert_eq!(
        parse_skills_list(&line, 12, Path::new("/home/user/git/proj")),
        vec![
            CodexSkill {
                name: "review".to_string(),
                path: PathBuf::from("/home/user/.codex/skills/review/SKILL.md"),
            },
            CodexSkill {
                name: "project_check".to_string(),
                path: PathBuf::from("/home/user/git/proj/.agents/skills/project_check/SKILL.md",),
            },
        ],
        "disabled skills are omitted while enabled skills retain their exact paths"
    );
}

#[test]
fn parse_skills_list_rejects_malformed_or_unrelated_responses() {
    let cwd = Path::new("/home/user/git/proj");
    for line in [
        "",
        "{not json",
        r#"{"jsonrpc":"2.0","id":12,"result":{"data":{}}}"#,
        r#"{"jsonrpc":"2.0","id":12,"result":{"data":[{"cwd":"/home/user/git/proj","errors":[],"skills":"invalid"}]}}"#,
        r#"{"jsonrpc":"2.0","id":13,"result":{"data":[]}}"#,
        r#"{"jsonrpc":"2.0","id":12,"error":{"code":-32602,"message":"bad params"}}"#,
    ] {
        assert!(
            parse_skills_list(line, 12, cwd).is_empty(),
            "must reject: {line:?}"
        );
    }
}

#[test]
fn parse_skills_list_accepts_empty_results() {
    let cwd = Path::new("/home/user/git/proj");
    for line in [
        r#"{"jsonrpc":"2.0","id":12,"result":{"data":[]}}"#,
        r#"{"jsonrpc":"2.0","id":12,"result":{"data":[{"cwd":"/home/user/git/proj","errors":[],"skills":[]}]}}"#,
        r#"{"jsonrpc":"2.0","id":12,"result":{"data":[{"cwd":"/home/user/git/proj","errors":[],"skills":[{"enabled":false,"name":"disabled","path":"/tmp/disabled/SKILL.md"}]}]}}"#,
    ] {
        assert!(
            parse_skills_list(line, 12, cwd).is_empty(),
            "empty discovery must stay empty: {line}"
        );
    }
}

#[test]
fn turn_start_request_wraps_an_ordinary_task_as_one_text_input() {
    let task = "$review inspect this";
    let request = json(&turn_start_request(6, "thread-abc", task, None, None));
    assert_envelope(&request, 6, "turn/start");
    assert_eq!(
        request.pointer("/params/threadId").and_then(Value::as_str),
        Some("thread-abc")
    );
    assert_eq!(
        request.pointer("/params/input"),
        Some(&serde_json::json!([{"type": "text", "text": task}])),
        "without a selected skill even dollar prefixed text stays ordinary text"
    );
}

#[test]
fn turn_start_request_uses_structured_skill_input_and_retains_its_path() {
    let skill = CodexSkill {
        name: "review".to_string(),
        path: PathBuf::from("/home/user/.codex/skills/review/SKILL.md"),
    };
    let request = json(&turn_start_request(
        7,
        "thread-abc",
        "$review inspect this",
        None,
        Some(&skill),
    ));
    assert_envelope(&request, 7, "turn/start");
    assert_eq!(
        request.pointer("/params/input"),
        Some(&serde_json::json!([
            {
                "type": "skill",
                "name": "review",
                "path": "/home/user/.codex/skills/review/SKILL.md"
            },
            {"type": "text", "text": "inspect this"}
        ])),
        "a selected skill is structured and only its following task remains text"
    );

    let skill_only = json(&turn_start_request(
        8,
        "thread-abc",
        "$review ",
        None,
        Some(&skill),
    ));
    assert_eq!(
        skill_only.pointer("/params/input"),
        Some(&serde_json::json!([{
            "type": "skill",
            "name": "review",
            "path": "/home/user/.codex/skills/review/SKILL.md"
        }])),
        "an empty task must not add an empty text input"
    );
}

#[test]
fn turn_start_request_carries_effort_only_when_requested() {
    // Some(effort) puts the reasoning effort on the turn params verbatim.
    let requested = json(&turn_start_request(
        9,
        "thread-abc",
        "do a thing",
        Some("xhigh"),
        None,
    ));
    assert_eq!(
        requested.pointer("/params/effort").and_then(Value::as_str),
        Some("xhigh")
    );

    // None means "let codex resolve its own default", so the key must be ABSENT entirely
    // rather than present as a null, which the daemon would read as an explicit value.
    let defaulted = json(&turn_start_request(
        10,
        "thread-abc",
        "do a thing",
        None,
        None,
    ));
    assert_eq!(defaulted.pointer("/params/effort"), None);
    assert!(
        !defaulted
            .pointer("/params")
            .and_then(Value::as_object)
            .expect("params is an object")
            .contains_key("effort"),
        "an absent effort must not serialize the key at all"
    );
}

#[test]
fn turn_start_request_round_trips_quotes_and_newlines() {
    // Proof the builders serialize through serde instead of formatting a string: a task
    // carrying a quote, a newline and a backslash must still parse as JSON at all, and the
    // text must come back out unchanged.
    let task = "say \"hello\"\nthen stop \\ here";
    let request = json(&turn_start_request(7, "thread-abc", task, None, None));
    assert_eq!(
        request
            .pointer("/params/input/0/text")
            .and_then(Value::as_str),
        Some(task)
    );
}

#[test]
fn turn_interrupt_request_targets_one_turn_of_one_thread() {
    // TurnInterruptParams requires BOTH threadId and turnId, so an interrupt cannot be
    // addressed to a thread alone (generate-json-schema, codex-cli 0.145.x).
    let request = json(&turn_interrupt_request(8, "thread-abc", "turn-xyz"));
    assert_envelope(&request, 8, "turn/interrupt");
    assert_eq!(
        request.pointer("/params/threadId").and_then(Value::as_str),
        Some("thread-abc")
    );
    assert_eq!(
        request.pointer("/params/turnId").and_then(Value::as_str),
        Some("turn-xyz")
    );
}

// --- thread/start response ---

/// A `thread/start` response line built around a REAL captured thread row, so the parse is
/// exercised against the daemon's actual thread shape (extra keys, nulls, nested objects)
/// rather than a hand-trimmed stub. Returns the line and the id it carries.
fn thread_start_response(id: i64) -> (String, String) {
    let page = common::fixture_json("codex_app_server/thread_list_page_with_cursor.json");
    let thread = page
        .pointer("/result/data/0")
        .expect("fixture has a thread row")
        .clone();
    let thread_id = thread
        .get("id")
        .and_then(Value::as_str)
        .expect("captured thread row has an id")
        .to_string();
    let line = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "thread": thread },
    })
    .to_string();
    (line, thread_id)
}

#[test]
fn parse_thread_id_reads_the_new_thread_id_from_its_own_response() {
    let (line, expected) = thread_start_response(9);
    assert_eq!(parse_thread_id(&line, 9), Some(expected));
}

#[test]
fn parse_thread_id_ignores_a_response_meant_for_another_request() {
    // The daemon multiplexes: notifications and other requests' responses arrive on the
    // same stream, so an id that does not match ours is not our thread.
    let (line, _) = thread_start_response(9);
    assert_eq!(parse_thread_id(&line, 10), None);
    assert_eq!(
        parse_thread_id(r#"{"jsonrpc":"2.0","method":"thread/started"}"#, 9),
        None,
        "a notification has no id and answers nothing"
    );
}

#[test]
fn parse_thread_id_rejects_errors_and_unparseable_lines() {
    let error = r#"{"jsonrpc":"2.0","id":9,"error":{"code":-32602,"message":"bad params"}}"#;
    assert_eq!(
        parse_thread_id(error, 9),
        None,
        "an error payload must never look like a spawned thread"
    );

    let (line, _) = thread_start_response(9);
    let truncated: String = line.chars().take(line.chars().count() / 2).collect();
    assert_eq!(parse_thread_id(&truncated, 9), None);
    assert_eq!(parse_thread_id("", 9), None);
    assert_eq!(
        parse_thread_id(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#, 9),
        None,
        "a result with no thread carries no id to return"
    );
}

// --- starting a daemon that outlives its own cwd ---

/// The live defect this pins, measured 2026-07-27. `ensure_daemon` started the daemon as a
/// plain child, so it inherited agent-viewer's cwd, which was a git worktree under
/// `.worktrees/`. The worktree was deleted when its branch merged; the daemon kept running and
/// kept answering `daemon version` with `"status":"running"`, but EVERY `thread/start` failed
/// with `failed to load configuration: No such file or directory (os error 2)`. Spawns then
/// silently degraded to `codex exec` and attach correctly refused them, which is the bug Brian
/// reported. The daemon is shared box-wide, so this broke every other codex client too.
#[test]
fn a_started_daemon_is_pinned_to_a_directory_it_cannot_outlive() {
    let cmd = daemon_start_command(Path::new("/home/user"));
    assert_eq!(
        cmd.get_current_dir(),
        Some(Path::new("/home/user")),
        "the daemon must never inherit the viewer's cwd"
    );
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy()).collect();
    assert_eq!(args, vec!["app-server", "daemon", "start"]);
}

#[test]
fn the_daemon_cwd_falls_back_to_root_when_home_is_gone() {
    let home = tempfile::TempDir::new().unwrap();
    assert_eq!(
        stable_daemon_cwd(home.path().to_path_buf()),
        home.path(),
        "a real home is the stable choice"
    );
    assert_eq!(
        stable_daemon_cwd(PathBuf::from("/nonexistent/home/of/nobody")),
        PathBuf::from("/"),
        "an unset or deleted HOME must not become a cwd the daemon can outlive"
    );
}

/// A spawn whose `turn/start` response was lost must not be reported as a flat failure. The
/// turn is accepted before the answer is written, so the work may well be running; a user told
/// "the turn failed" retries the spawn and runs the task twice.
#[test]
fn a_lost_turn_response_reads_as_indeterminate_not_as_a_failure() {
    let reason = spawn_failure_reason(&SpawnAttempt::TurnFailed {
        thread_id: "thread-abc".to_string(),
        error: Error::Command("app-server request timed out".to_string()),
    });
    assert!(
        reason.contains("thread-abc"),
        "the reason must name the thread that exists: {reason}"
    );
    assert!(
        reason.contains("may have started"),
        "the reason must say the turn may be running: {reason}"
    );
    assert!(
        reason.contains("app-server request timed out"),
        "the underlying error must survive: {reason}"
    );

    // A thread that really was never created keeps carrying its own error verbatim.
    let reason = spawn_failure_reason(&SpawnAttempt::NotCreated(Error::Command(
        "no daemon is listening".to_string(),
    )));
    assert_eq!(reason, "command failed: no daemon is listening");
}
