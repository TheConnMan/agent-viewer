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

#[test]
fn claude_rename_preserves_the_state_file_permissions() {
    // Claude writes state.json 0600 and the jobs dir is group/other traversable, so replacing
    // the file with a umask-default 0644/0664 temp would expose intent, output, respawn
    // configuration, and transcript paths to every local user. The atomic write must carry the
    // original mode across the rename.
    use std::os::unix::fs::PermissionsExt;
    let root = jobs_root_with("ab12", r#"{"name":"old","intent":"secret"}"#);
    let path = root.path().join("ab12").join("state.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");

    let backend = ClaudeBackend::with_binary_and_jobs_root("claude", root.path().to_path_buf());
    backend
        .rename(&claude_session(Some("ab12")), "new")
        .expect("rename succeeds");

    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "rename must not widen the state file");
}

#[test]
fn claude_jobs_root_follows_the_claude_config_dir() {
    // `claude agents` lists jobs out of $CLAUDE_CONFIG_DIR when it is set, so a viewer that
    // inherited that env must write the same tree - otherwise rename either fails on a missing
    // file or, worse, writes a same-short-id job in the default tree.
    use agent_viewer_core::claude::jobs_root_from;
    let home = std::path::Path::new("/home/somebody");
    assert_eq!(
        jobs_root_from(Some("/srv/claude-config"), home),
        std::path::Path::new("/srv/claude-config/jobs")
    );
    assert_eq!(
        jobs_root_from(None, home),
        std::path::Path::new("/home/somebody/.claude/jobs")
    );
    assert_eq!(
        jobs_root_from(Some("   "), home),
        std::path::Path::new("/home/somebody/.claude/jobs"),
        "a blank env value is not a config dir"
    );
}

#[test]
fn claude_iso8601_matches_the_javascript_toisostring_shape() {
    // The value goes into a field claude's own writer stamps with `new Date().toISOString()`,
    // so the shape has to match exactly: UTC, millisecond precision, trailing Z.
    use agent_viewer_core::claude::iso8601_utc_millis;
    use std::time::{Duration, UNIX_EPOCH};
    assert_eq!(iso8601_utc_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        iso8601_utc_millis(UNIX_EPOCH + Duration::from_millis(1_785_095_531_007)),
        "2026-07-26T19:52:11.007Z"
    );
    // Leap day, and the last second of a leap year.
    assert_eq!(
        iso8601_utc_millis(UNIX_EPOCH + Duration::from_secs(1_709_209_845)),
        "2024-02-29T12:30:45.000Z"
    );
    assert_eq!(
        iso8601_utc_millis(UNIX_EPOCH + Duration::from_secs(1_735_689_599)),
        "2024-12-31T23:59:59.000Z"
    );
}

#[test]
fn claude_rename_stamps_updated_at_like_claude_does() {
    // Claude's own rename writer sets `updatedAt` alongside the name. Leaving it stale makes
    // this rename invisible to anything that sorts or invalidates on that field.
    let root = jobs_root_with(
        "ab12",
        r#"{"name":"old","updatedAt":"2020-01-01T00:00:00.000Z"}"#,
    );
    let backend = ClaudeBackend::with_binary_and_jobs_root("claude", root.path().to_path_buf());

    backend
        .rename(&claude_session(Some("ab12")), "new")
        .expect("rename succeeds");

    let after = state_json(&root, "ab12");
    let stamped = after["updatedAt"].as_str().expect("updatedAt is a string");
    assert_ne!(
        stamped, "2020-01-01T00:00:00.000Z",
        "updatedAt must advance"
    );
    assert!(
        stamped.ends_with('Z') && stamped.len() == 24,
        "updatedAt must keep the toISOString shape, got {stamped:?}"
    );
}

#[test]
fn claude_rename_never_exposes_the_temp_file_to_other_users() {
    // The temp must be CREATED restricted, not widened after the fact: another local user can
    // read a 0644 temp in the window between write and chmod.
    use std::os::unix::fs::PermissionsExt;
    let root = jobs_root_with("ab12", r#"{"name":"old","intent":"secret"}"#);
    let dir = root.path().join("ab12");
    std::fs::set_permissions(
        dir.join("state.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("chmod 600");

    // A writer that observes every file the rename creates, at the moment it is created.
    let watch_dir = dir.clone();
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = std::sync::Arc::clone(&observed);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = std::sync::Arc::clone(&stop);
    let watcher = std::thread::spawn(move || {
        while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok(entries) = std::fs::read_dir(&watch_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().contains(".tmp")
                        && let Ok(meta) = entry.metadata()
                    {
                        sink.lock()
                            .expect("lock")
                            .push(meta.permissions().mode() & 0o777);
                    }
                }
            }
        }
    });

    let backend = ClaudeBackend::with_binary_and_jobs_root("claude", root.path().to_path_buf());
    for i in 0..40 {
        backend
            .rename(&claude_session(Some("ab12")), &format!("new {i}"))
            .expect("rename succeeds");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    watcher.join().expect("watcher joins");

    let seen = observed.lock().expect("lock").clone();
    assert!(
        seen.iter().all(|mode| mode & 0o077 == 0),
        "temp file was group/other readable at some point: {seen:?}"
    );
}
