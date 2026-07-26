use agent_viewer_core::backend::{Backend, BackendKind, SessionOrigin, Status};
use agent_viewer_core::claude::ClaudeBackend;
use agent_viewer_core::codex::cli::name_set_request;
use agent_viewer_core::error::Error;
use serde_json::Value;

fn claude_session(short_id: Option<&str>) -> agent_viewer_core::Session {
    agent_viewer_core::Session {
        backend: BackendKind::Claude,
        id: "3f9c1a2e-0000-4000-8000-000000000001".to_string(),
        short_id: short_id.map(str::to_string),
        origin: SessionOrigin::Background,
        title: "probe".to_string(),
        cwd: std::path::PathBuf::from("/tmp"),
        git_branch: None,
        status: Status::Working,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
    }
}

#[test]
fn codex_name_set_request_shape() {
    // A name containing a quote and a newline must survive serialization intact
    // (serde-built, never string-interpolated).
    let tricky = "name with \" and \n newline";
    let line = name_set_request(7, "019f-thread", tricky);

    // One valid JSON object per line.
    let v: Value = serde_json::from_str(line.trim()).expect("valid json-rpc line");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["method"], "thread/name/set");
    assert_eq!(v["params"]["threadId"], "019f-thread");
    assert_eq!(v["params"]["name"], tricky);
    assert_eq!(v["id"], 7);
}

// Claude has no external rename channel. The daemon rendezvous socket authenticates its first
// frame as `attacher-caps` and rejects a `rename_session` frame, so the old best-effort attempt
// could never succeed - while every attempt evicted the daemon's supervisor connection for that
// live session. Rename must therefore be advertised unsupported and must never open the socket.
#[test]
fn claude_advertises_rename_unsupported() {
    assert!(!ClaudeBackend::new().capabilities().rename);
}

#[test]
fn claude_rename_is_refused_without_contacting_the_daemon() {
    let backend = ClaudeBackend::new();
    // A row carrying a live short id is exactly the case that previously reached the socket.
    let err = backend
        .rename(&claude_session(Some("ab12")), "new name")
        .expect_err("claude rename must be refused, never attempted");
    match err {
        Error::Unsupported(name) => assert_eq!(name, "claude"),
        other => panic!("expected Unsupported(\"claude\"), got {other:?}"),
    }
}
