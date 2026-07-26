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

// Claude renames a daemon-backed bg job by writing `name`/`nameSource` into that job's
// `~/.claude/jobs/<short>/state.json` - verified against the 2.1.220 bundle (fleet view's
// Ctrl+R calls that state writer, whose failure notice is "its state file is unwritable") and
// live on this box, where writing the field made `claude agents --json` report the new name.
// The rendezvous socket is NOT the channel: it authenticates its first frame as
// `attacher-caps`, and `rename_session` belongs to the unrelated SDK/bridge control protocol.
// Interactive rows carry no short id and so no job dir, which is why rename is gated per row.

/// A jobs root holding one job dir whose state.json contains `body`.
fn jobs_root_with(short_id: &str, body: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().join(short_id);
    std::fs::create_dir_all(&dir).expect("job dir");
    std::fs::write(dir.join("state.json"), body).expect("state.json");
    root
}

fn state_json(root: &tempfile::TempDir, short_id: &str) -> Value {
    let text = std::fs::read_to_string(root.path().join(short_id).join("state.json"))
        .expect("state.json readable");
    serde_json::from_str(&text).expect("state.json is valid json")
}

#[test]
fn claude_advertises_rename_only_for_rows_with_a_job_dir() {
    let backend = ClaudeBackend::new();
    assert!(backend.capabilities().rename);
    assert!(
        backend
            .capabilities_for(&claude_session(Some("ab12")))
            .rename,
        "a bg row carries the short id that names its job dir"
    );
    assert!(
        !backend.capabilities_for(&claude_session(None)).rename,
        "an interactive row has no job dir, so rename must be advertised unsupported"
    );
    assert!(
        !backend.capabilities_for(&claude_session(Some(""))).rename,
        "an empty short id names no job dir either"
    );
}

#[test]
fn claude_rename_writes_name_and_source_into_the_job_state() {
    let root = jobs_root_with(
        "ab12",
        r#"{"state":"done","name":"old name","nameSource":"auto","tokens":2043,
            "respawnFlags":["--model","opus[1m]"],"intent":"why does rename fail?"}"#,
    );
    let backend = ClaudeBackend::with_binary_and_jobs_root("claude", root.path().to_path_buf());

    backend
        .rename(&claude_session(Some("ab12")), "renamed by the viewer")
        .expect("rename writes the job state");

    let after = state_json(&root, "ab12");
    assert_eq!(after["name"], "renamed by the viewer");
    assert_eq!(
        after["nameSource"], "user",
        "a viewer rename is a user rename, exactly as fleet view records it"
    );
    // Every other field survives: this is a read-modify-write of claude's own state file, and
    // clobbering `respawnFlags` or `intent` would break the job's respawn contract.
    assert_eq!(after["state"], "done");
    assert_eq!(after["tokens"], 2043);
    assert_eq!(after["respawnFlags"][1], "opus[1m]");
    assert_eq!(after["intent"], "why does rename fail?");
}

#[test]
fn claude_rename_leaves_no_temp_file_beside_the_state() {
    // The write is atomic (temp in the same dir, then rename over the target) so a reader
    // never sees a half-written state.json; the temp must not survive the rename.
    let root = jobs_root_with("ab12", r#"{"name":"old"}"#);
    let backend = ClaudeBackend::with_binary_and_jobs_root("claude", root.path().to_path_buf());

    backend
        .rename(&claude_session(Some("ab12")), "new")
        .expect("rename succeeds");

    let entries: Vec<String> = std::fs::read_dir(root.path().join("ab12"))
        .expect("job dir readable")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["state.json".to_string()]);
}

#[test]
fn claude_rename_is_unsupported_without_a_short_id() {
    let root = jobs_root_with("ab12", r#"{"name":"old"}"#);
    let backend = ClaudeBackend::with_binary_and_jobs_root("claude", root.path().to_path_buf());
    let err = backend
        .rename(&claude_session(None), "new name")
        .expect_err("an interactive row has no job dir to write");
    match err {
        Error::Unsupported(name) => assert_eq!(name, "claude"),
        other => panic!("expected Unsupported(\"claude\"), got {other:?}"),
    }
}

#[test]
fn claude_rename_fails_loudly_when_the_job_state_is_gone() {
    // The job was removed under us. Reporting Ok here would leave the viewer showing a name
    // that no `claude agents` listing will ever echo back.
    let root = tempfile::tempdir().expect("tempdir");
    let backend = ClaudeBackend::with_binary_and_jobs_root("claude", root.path().to_path_buf());
    let err = backend
        .rename(&claude_session(Some("ab12")), "new name")
        .expect_err("a missing state file must surface, never be silently created");
    assert!(
        matches!(err, Error::Io(_)),
        "expected an io error, got {err:?}"
    );
    assert!(
        !root.path().join("ab12").exists(),
        "rename must not fabricate a job dir"
    );
}

#[test]
fn claude_rename_refuses_a_state_file_that_is_not_an_object() {
    // Defensive: a truncated or replaced state.json must not be overwritten with a synthesized
    // object that drops every field the job needs to respawn.
    let root = jobs_root_with("ab12", "[1, 2, 3]");
    let backend = ClaudeBackend::with_binary_and_jobs_root("claude", root.path().to_path_buf());
    let err = backend
        .rename(&claude_session(Some("ab12")), "new name")
        .expect_err("a non-object state file must be refused");
    assert!(
        matches!(err, Error::Command(_)),
        "expected a command error, got {err:?}"
    );
    assert_eq!(state_json(&root, "ab12"), serde_json::json!([1, 2, 3]));
}
