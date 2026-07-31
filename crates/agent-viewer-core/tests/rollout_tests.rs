mod common;

use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::codex::rollout::{
    PendingApproval, TailState, pending_approval, read_transcript_tail, tail_state,
};
use agent_viewer_core::{Backend, BackendKind, Session, SessionOrigin, Status, TailEvent};
use common::rfc3339_at;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn codex_session(rollout_path: Option<PathBuf>) -> Session {
    Session {
        backend: BackendKind::Codex,
        id: "activity-session".to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: "Activity session".to_string(),
        cwd: PathBuf::from("/home/user/project"),
        git_branch: None,
        status: Status::Done,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        subagent: false,
        summary: String::new(),
        pid: None,
        rollout_path,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

fn codex_activity_session(id: &str, rollout_path: PathBuf) -> Session {
    let mut session = codex_session(Some(rollout_path));
    session.id = id.to_string();
    session
}

fn write_codex_activity(path: &std::path::Path, timestamp: i64) {
    let mut file = std::fs::File::create(path).expect("create rollout");
    writeln!(
        file,
        r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"activity"}}]}}}}"#,
        rfc3339_at(timestamp)
    )
    .expect("write rollout");
}

fn insert_codex_activity_thread(
    conn: &rusqlite::Connection,
    id: &str,
    rollout_path: &std::path::Path,
    source: &str,
    updated_at_ms: i64,
) {
    let rollout_path = rollout_path.to_string_lossy().into_owned();
    conn.execute(
        "INSERT INTO threads \
         (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, \
          sandbox_policy, approval_mode, archived, model, git_branch, first_user_message, \
          preview, created_at_ms, updated_at_ms) \
         VALUES (?1, ?2, 1, 1, ?3, 'openai', '/project', ?1, 'workspace-write', \
                 'on-request', 0, NULL, NULL, 'message', 'preview', 1000, ?4)",
        rusqlite::params![id, rollout_path, source, updated_at_ms],
    )
    .expect("insert activity thread");
}

// --- Preserved v1 tests (unchanged behavior) ---

#[test]
fn transcript_extracts_text_and_tool_calls_in_order() {
    let path = common::fixture_path("rollout_complete.jsonl");
    let items = read_transcript_tail(&path).expect("read transcript");
    // The user message, the assistant message, and the function_call the tail pane needs,
    // in transcript order. `arguments` is an embedded JSON string, so the detail comes out
    // of its `cmd` key.
    assert_eq!(
        items,
        vec![
            TailEvent::User("Reply with exactly the word DONE.".to_string()),
            TailEvent::Agent("DONE".to_string()),
            TailEvent::Tool {
                name: "exec_command".to_string(),
                detail: "ls -la".to_string(),
            },
        ]
    );
}

#[test]
fn transcript_drops_developer_and_system_scaffolding() {
    // Measured on this box 2026-07-31: a real 6-item rollout was half `developer` prompt
    // blobs (the memory preamble, the multi-agent prompt), which sit at the START of a
    // session and would therefore BE the tail of a freshly spawned one. They are harness
    // scaffolding, not conversation, and must never reach the pane.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("scaffolding.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"developer","content":[{{"type":"input_text","text":"Memory preamble: you have access to a memory folder"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"system","content":[{{"type":"input_text","text":"you are a coding agent"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"the real task"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"on it"}}]}}}}"#
    )
    .unwrap();
    drop(f);

    assert_eq!(
        read_transcript_tail(&path).expect("read transcript"),
        vec![
            TailEvent::User("the real task".to_string()),
            TailEvent::Agent("on it".to_string()),
        ]
    );
}

#[test]
fn transcript_tool_detail_squashes_multiline_and_survives_unknown_arguments() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("tools.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    // A multi-line shell script must not eat the pane: it squashes to one line.
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":"{{\"cmd\":\"pwd\\n  wc -l state.md\"}}"}}}}"#
    )
    .unwrap();
    // An argv array joins with spaces.
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"function_call","name":"shell","arguments":"{{\"command\":[\"bash\",\"-lc\",\"ls\"]}}"}}}}"#
    )
    .unwrap();
    // Arguments with no recognized key still produce a named tool line, not a dropped event.
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"function_call","name":"update_plan","arguments":"{{\"plan\":[]}}"}}}}"#
    )
    .unwrap();
    // A custom tool call carries its input raw rather than as embedded JSON.
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** End Patch"}}}}"#
    )
    .unwrap();
    // A nameless call is not an event.
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"function_call","name":"","arguments":"{{}}"}}}}"#
    )
    .unwrap();
    drop(f);

    assert_eq!(
        read_transcript_tail(&path).expect("read transcript"),
        vec![
            TailEvent::Tool {
                name: "exec_command".to_string(),
                detail: "pwd wc -l state.md".to_string(),
            },
            TailEvent::Tool {
                name: "shell".to_string(),
                detail: "bash -lc ls".to_string(),
            },
            TailEvent::Tool {
                name: "update_plan".to_string(),
                detail: String::new(),
            },
            TailEvent::Tool {
                name: "apply_patch".to_string(),
                detail: "*** Begin Patch *** End Patch".to_string(),
            },
        ]
    );
}

#[test]
fn transcript_excludes_empty_text_items() {
    // A response_item whose content has ONLY a non-text chunk extracts to "" and must be
    // dropped, so peek never shows a blank role-only line.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("empty_item.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"role":"user","content":[{{"type":"input_text","text":"hello"}}]}}}}"#
    )
    .unwrap();
    // Tool-only assistant turn: no input_text/output_text chunk -> text == "".
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"role":"assistant","content":[{{"type":"function_call"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"role":"assistant","content":[{{"type":"output_text","text":"world"}}]}}}}"#
    )
    .unwrap();
    drop(f);

    let items = read_transcript_tail(&path).expect("read transcript");
    assert_eq!(
        items,
        vec![
            TailEvent::User("hello".to_string()),
            TailEvent::Agent("world".to_string()),
        ]
    );
}

#[test]
fn codex_turn_activity_normalizes_filters_and_tolerates_bad_timestamps() {
    let (_dir, path) = common::copy_fixture_to_temp("rollout_complete.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"not-a-timestamp","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"bad time"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"missing time"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"2026-07-10T21:07:11.000Z","type":"response_item","payload":{{"type":"reasoning","role":"assistant","content":[{{"type":"output_text","text":"protocol noise"}}]}}}}"#
    )
    .unwrap();
    drop(file);

    let backend = CodexBackend::new(PathBuf::from("/unused"));
    let session = codex_session(Some(path));
    assert_eq!(
        backend
            .turn_activity(&session, Duration::MAX)
            .expect("activity"),
        vec![1_783_717_630_000, 1_783_717_630_000, 1_783_717_632_000,]
    );
    assert!(
        backend
            .turn_activity(&session, Duration::ZERO)
            .expect("old turns excluded")
            .is_empty()
    );
}

#[test]
fn codex_turn_activity_reads_full_history_and_meaningful_calls() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("large.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let early_user = now - 50;
    let call = now - 40;
    let later_assistant = now - 10;
    writeln!(
        file,
        r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"before padding"}}]}}}}"#,
        rfc3339_at(early_user)
    )
    .unwrap();
    writeln!(file, "{}", "x".repeat(128 * 1024)).unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"after padding"}}]}}}}"#,
        rfc3339_at(later_assistant)
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":"{{}}","call_id":"call_1"}}}}"#,
        rfc3339_at(call)
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"custom_tool_call","name":"apply_patch","input":"patch","call_id":"call_2"}}}}"#,
        rfc3339_at(call)
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"not-a-timestamp","type":"response_item","payload":{{"type":"function_call","name":"exec_command"}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"response_item","payload":{{"type":"custom_tool_call","name":"apply_patch"}}}}"#
    )
    .unwrap();
    writeln!(file, "{{not json").unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"function_call","name":""}}}}"#,
        rfc3339_at(now - 35)
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"function_call_output","call_id":"call_1","output":"done"}}}}"#,
        rfc3339_at(now - 30)
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"function_call","name":"exec_command"}}}}"#,
        rfc3339_at(now + 3_600)
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"expired"}}]}}}}"#,
        rfc3339_at(now - 7_200)
    )
    .unwrap();
    drop(file);

    let backend = CodexBackend::new(PathBuf::from("/unused"));
    assert_eq!(
        backend
            .turn_activity(
                &codex_session(Some(path.clone())),
                Duration::from_secs(60 * 60)
            )
            .expect("full activity"),
        vec![
            early_user * 1_000,
            call * 1_000,
            call * 1_000,
            later_assistant * 1_000,
        ]
    );
    assert!(
        backend
            .turn_activity(&codex_session(Some(path)), Duration::ZERO)
            .expect("old turns excluded")
            .is_empty()
    );
    assert!(
        backend
            .turn_activity(&codex_session(None), Duration::MAX)
            .expect("no transcript")
            .is_empty()
    );
    assert!(
        backend
            .turn_activity(
                &codex_session(Some(dir.path().join("missing.jsonl"))),
                Duration::MAX
            )
            .expect("missing transcript")
            .is_empty()
    );
}

#[test]
fn codex_turn_activity_includes_only_requested_subtree() {
    let dir = tempfile::TempDir::new().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let root_path = dir.path().join("root.jsonl");
    let child_a_path = dir.path().join("child_a.jsonl");
    let child_b_path = dir.path().join("child_b.jsonl");
    let grandchild_path = dir.path().join("grandchild.jsonl");
    let unrelated_path = dir.path().join("unrelated.jsonl");
    let malformed_path = dir.path().join("malformed.jsonl");
    let missing_path = dir.path().join("missing.jsonl");
    let cycle_a_path = dir.path().join("cycle_a.jsonl");
    let cycle_b_path = dir.path().join("cycle_b.jsonl");

    write_codex_activity(&root_path, now - 6);
    write_codex_activity(&child_a_path, now - 5);
    write_codex_activity(&child_b_path, now - 4);
    write_codex_activity(&grandchild_path, now - 3);
    write_codex_activity(&unrelated_path, now - 2);
    write_codex_activity(&malformed_path, now - 1);
    write_codex_activity(&cycle_a_path, now - 8);
    write_codex_activity(&cycle_b_path, now - 7);

    let db_path = dir.path().join("state_5.sqlite");
    let conn = rusqlite::Connection::open(&db_path).expect("open registry");
    conn.execute_batch(&common::read_fixture("threads_schema.sql"))
        .expect("create registry schema");
    insert_codex_activity_thread(&conn, "root", &root_path, "cli", (now - 6) * 1_000);
    insert_codex_activity_thread(
        &conn,
        "child_a",
        &child_a_path,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}"#,
        (now - 5) * 1_000,
    );
    insert_codex_activity_thread(
        &conn,
        "child_b",
        &child_b_path,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}"#,
        (now - 4) * 1_000,
    );
    insert_codex_activity_thread(
        &conn,
        "grandchild",
        &grandchild_path,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"child_a"}}}"#,
        (now - 3) * 1_000,
    );
    insert_codex_activity_thread(
        &conn,
        "unrelated",
        &unrelated_path,
        "cli",
        (now - 2) * 1_000,
    );
    insert_codex_activity_thread(
        &conn,
        "malformed",
        &malformed_path,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"root"}"#,
        (now - 1) * 1_000,
    );
    insert_codex_activity_thread(
        &conn,
        "missing",
        &missing_path,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}"#,
        now * 1_000,
    );
    insert_codex_activity_thread(
        &conn,
        "cycle_a",
        &cycle_a_path,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"cycle_b"}}}"#,
        (now - 8) * 1_000,
    );
    insert_codex_activity_thread(
        &conn,
        "cycle_b",
        &cycle_b_path,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"cycle_a"}}}"#,
        (now - 7) * 1_000,
    );
    conn.close().expect("close registry");

    let backend = CodexBackend::new(dir.path().to_path_buf());
    assert_eq!(
        backend
            .turn_activity(
                &codex_activity_session("root", root_path.clone()),
                Duration::from_secs(60)
            )
            .expect("root subtree activity"),
        vec![
            (now - 6) * 1_000,
            (now - 5) * 1_000,
            (now - 4) * 1_000,
            (now - 3) * 1_000,
        ]
    );
    assert_eq!(
        backend
            .turn_activity(
                &codex_activity_session("child_a", child_a_path),
                Duration::from_secs(60)
            )
            .expect("child subtree activity"),
        vec![(now - 5) * 1_000, (now - 3) * 1_000]
    );
    assert_eq!(
        backend
            .turn_activity(
                &codex_activity_session("cycle_a", cycle_a_path),
                Duration::from_secs(60)
            )
            .expect("cycle activity"),
        vec![(now - 8) * 1_000, (now - 7) * 1_000]
    );

    let no_registry = tempfile::TempDir::new().unwrap();
    let backend_without_registry = CodexBackend::new(no_registry.path().to_path_buf());
    assert_eq!(
        backend_without_registry
            .turn_activity(
                &codex_activity_session("root", root_path),
                Duration::from_secs(60)
            )
            .expect("root activity without registry"),
        vec![(now - 6) * 1_000]
    );
}

/// `turn_activity` runs once per visible session on every activity refresh, so the registry's
/// spawn edges are scanned once and reused across those calls instead of rescanning every row
/// per session. A subagent spawned AFTER the first call must still join its parent's ribbon on
/// the next one - a cache that never expired would hide it for the life of the process.
#[test]
fn codex_turn_activity_sees_a_subagent_spawned_after_the_first_call() {
    let dir = tempfile::TempDir::new().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let root_path = dir.path().join("root.jsonl");
    write_codex_activity(&root_path, now - 4);

    let db_path = dir.path().join("state_5.sqlite");
    let conn = rusqlite::Connection::open(&db_path).expect("open registry");
    conn.execute_batch(&common::read_fixture("threads_schema.sql"))
        .expect("create registry schema");
    insert_codex_activity_thread(&conn, "root", &root_path, "cli", (now - 4) * 1_000);
    conn.close().expect("close registry");

    let backend = CodexBackend::new(dir.path().to_path_buf());
    let activity = || {
        backend
            .turn_activity(
                &codex_activity_session("root", root_path.clone()),
                Duration::from_secs(60),
            )
            .expect("root subtree activity")
    };
    assert_eq!(activity(), vec![(now - 4) * 1_000]);

    let child_path = dir.path().join("child.jsonl");
    write_codex_activity(&child_path, now - 2);
    let conn = rusqlite::Connection::open(&db_path).expect("reopen registry");
    insert_codex_activity_thread(
        &conn,
        "child",
        &child_path,
        r#"{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}"#,
        (now - 2) * 1_000,
    );
    conn.close().expect("close registry");
    // The insert already moved the db's mtime; pushing it forward makes the freshness key
    // differ by seconds, so the assertion below cannot ride on filesystem timestamp
    // granularity (a one-row insert need not change the file's length at all).
    std::fs::File::options()
        .write(true)
        .open(&db_path)
        .expect("open db for a timestamp bump")
        .set_modified(SystemTime::now() + Duration::from_secs(2))
        .expect("bump the db mtime");

    assert_eq!(
        activity(),
        vec![(now - 4) * 1_000, (now - 2) * 1_000],
        "a subagent inserted after the first call must invalidate the cached spawn edges"
    );
}

// --- v2 tail_state contract (tests 1-5) ---

#[test]
fn tail_state_complete() {
    // task_complete present, no later task_started -> Complete.
    let path = common::fixture_path("rollout_complete.jsonl");
    assert_eq!(tail_state(&path).expect("tail_state"), TailState::Complete);
}

#[test]
fn tail_state_midturn() {
    // No task_complete, no approval -> MidTurn.
    let path = common::fixture_path("rollout_midturn.jsonl");
    assert_eq!(tail_state(&path).expect("tail_state"), TailState::MidTurn);
}

#[test]
fn tail_state_stale_complete_then_started() {
    // Regression pin of the ae991-99 last-turn rule under the new enum: a stale
    // task_complete followed by a later task_started (resumed-then-abandoned) is NOT
    // complete.
    let (_dir, path) = common::copy_fixture_to_temp("rollout_complete.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"turn-2"}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"response_item","payload":{{"type":"message","id":"user_resume","role":"user","content":[{{"type":"input_text","text":"Now do more work."}}]}}}}"#
    )
    .unwrap();
    drop(f);
    assert_eq!(tail_state(&path).expect("tail_state"), TailState::MidTurn);
}

/// A turn stopped with `turn/interrupt` (this is what Ctrl+X does to a daemon-hosted row)
/// writes a `turn_aborted` event and nothing else: no task_complete, no new task_started. That
/// has to read as terminal, or the row it belongs to reads Working forever and the stop looks
/// like it did nothing. The appended line is the live capture's shape.
#[test]
fn tail_state_turn_aborted_is_terminal() {
    let (_dir, path) = common::copy_fixture_to_temp("rollout_midturn.jsonl");
    assert_eq!(
        tail_state(&path).expect("tail_state"),
        TailState::MidTurn,
        "the fixture starts mid-turn"
    );
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"turn_aborted","turn_id":"019fa12c-c9c8-7f53-a0b8-babf4e947d6e","reason":"interrupted","started_at":1785115494,"completed_at":1785115500}}}}"#
    )
    .unwrap();
    drop(f);
    assert_eq!(tail_state(&path).expect("tail_state"), TailState::Complete);
}

/// An approval that was never answered, on a turn that then got interrupted, is over: the tail
/// must not keep reporting AwaitingApproval once the turn is aborted.
#[test]
fn tail_state_turn_aborted_after_an_approval_is_terminal() {
    let (_dir, path) = common::copy_fixture_to_temp("rollout_approval.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted","completed_at":1785115500}}}}"#
    )
    .unwrap();
    drop(f);
    assert_eq!(tail_state(&path).expect("tail_state"), TailState::Complete);
}

#[test]
fn tail_state_awaiting_approval() {
    // task_started then an *_approval_request, no task_complete after -> AwaitingApproval.
    let path = common::fixture_path("rollout_approval.jsonl");
    assert_eq!(
        tail_state(&path).expect("tail_state"),
        TailState::AwaitingApproval
    );
}

#[test]
fn tail_state_approval_then_complete() {
    // The approval fixture with a later token_count + task_complete appended: the
    // approval was granted and the turn finished -> Complete (approval stops firing).
    let (_dir, path) = common::copy_fixture_to_temp("rollout_approval.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{}},"rate_limits":null}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn-1","completed_at":1,"duration_ms":1}}}}"#
    )
    .unwrap();
    drop(f);
    assert_eq!(tail_state(&path).expect("tail_state"), TailState::Complete);
}

// --- pending_approval contract ---

#[test]
fn pending_approval_parses_exec() {
    let path = common::fixture_path("rollout_approval.jsonl");
    let pending = pending_approval(&path).expect("pending_approval");
    assert_eq!(
        pending,
        Some(PendingApproval::Exec {
            command: vec!["rm".to_string(), "-rf".to_string(), "target".to_string()],
            cwd: Some("/home/user/project".to_string()),
        })
    );
    assert_eq!(pending.unwrap().summary(), "rm -rf target");
}

#[test]
fn exec_summary_quotes_whitespace_args() {
    // A whitespace-containing arg is single-quoted so the summary reflects what runs.
    let approval = PendingApproval::Exec {
        command: vec!["echo".to_string(), "a b".to_string()],
        cwd: None,
    };
    assert_eq!(approval.summary(), "echo 'a b'");
    // Shell metacharacters are quoted too, so the summary never misstates the argv.
    let meta = PendingApproval::Exec {
        command: vec!["$HOME".to_string(), "a;b".to_string(), "*.rs".to_string()],
        cwd: None,
    };
    assert_eq!(meta.summary(), "'$HOME' 'a;b' '*.rs'");
    // Plain args (including paths with `/`, `.`, `-`) stay bare.
    let plain = PendingApproval::Exec {
        command: vec![
            "rm".to_string(),
            "-rf".to_string(),
            "target".to_string(),
            "src/main.rs".to_string(),
        ],
        cwd: None,
    };
    assert_eq!(plain.summary(), "rm -rf target src/main.rs");
}

#[test]
fn pending_approval_parses_patch_with_sorted_files() {
    let path = common::fixture_path("rollout_patch_approval.jsonl");
    let pending = pending_approval(&path).expect("pending_approval");
    assert_eq!(
        pending,
        Some(PendingApproval::Patch {
            files: vec!["README.md".to_string(), "src/main.rs".to_string()],
        })
    );
    assert_eq!(
        pending.unwrap().summary(),
        "apply patch: README.md, src/main.rs"
    );
}

#[test]
fn pending_approval_none_when_complete() {
    // No approval in the tail -> None.
    let path = common::fixture_path("rollout_complete.jsonl");
    assert_eq!(pending_approval(&path).expect("pending_approval"), None);
}

#[test]
fn pending_approval_none_when_resolved() {
    // task_started, an exec_approval_request, THEN a task_complete after it: the approval was
    // granted and the turn finished -> not pending.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("resolved_approval.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"turn-1"}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"exec_approval_request","turn_id":"turn-1","command":["rm","-rf","target"],"cwd":"/tmp"}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn-1","completed_at":1,"duration_ms":1}}}}"#
    )
    .unwrap();
    drop(f);
    assert_eq!(pending_approval(&path).expect("pending_approval"), None);
}

/// Codex opens every thread with a `role: "user"` item that is not the user's: the plugin
/// catalogue, the whole of AGENTS.md, and an `<environment_context>` block. It wears the user's
/// role, so the developer/system filter does not catch it, and on a fresh thread it IS the
/// tail — measured live 2026-07-31, the pane opened on a plugin list instead of the task.
#[test]
fn the_codex_environment_preamble_is_dropped_from_the_tail() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("preamble.jsonl");
    let preamble = "<recommended_plugins>\\n- Slack (slack@openai-curated-remote)\\n\
</recommended_plugins># AGENTS.md instructions\\n<INSTRUCTIONS>be concise</INSTRUCTIONS>\
<environment_context>\\n  <cwd>/tmp/x</cwd>\\n  <shell>bash</shell>\\n</environment_context>";
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(
        file,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{preamble}"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"fix the drift"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"on it"}}]}}}}"#
    )
    .unwrap();

    let events = read_transcript_tail(&path).unwrap();
    assert_eq!(
        events,
        [
            TailEvent::User("fix the drift".to_string()),
            TailEvent::Agent("on it".to_string()),
        ],
        "the scaffolding blob must not reach the pane"
    );
}

/// The filter deletes conversation, so a false positive eats the user's own words. It is narrow
/// on BOTH axes: the tag codex composes, and only at the head of the thread. A human quoting
/// that tag mid-conversation keeps their message; showing a preamble is the smaller failure.
#[test]
fn a_later_message_carrying_the_environment_tag_is_kept() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("quoting.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(
        file,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"first, a real prompt"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"why does <environment_context> say bash here?"}}]}}}}"#
    )
    .unwrap();

    let events = read_transcript_tail(&path).unwrap();
    assert_eq!(
        events,
        [
            TailEvent::User("first, a real prompt".to_string()),
            TailEvent::User("why does <environment_context> say bash here?".to_string()),
        ],
        "only the LEADING scaffolding item may be dropped"
    );
}

/// The tail read must be BOUNDED. A live rollout grows without bound (18.6 MB measured on this
/// box) and the pane re-reads it on every 2s tick to show twelve events, so the reader looks
/// only at the end of the file. Written large enough that the early half is provably outside
/// the window.
#[test]
fn read_transcript_tail_touches_only_the_end_of_a_multi_megabyte_rollout() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("rollout-huge.jsonl");
    let file = std::fs::File::create(&path).unwrap();
    let mut writer = std::io::BufWriter::new(file);
    // ~2 KB of text per message; 1,000 of them is ~2 MB before the tail messages.
    let filler = "x".repeat(2_048);
    for n in 0..1_000 {
        writeln!(
            writer,
            "{}",
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": format!("early-{n} {filler}")}]
                }
            })
        )
        .unwrap();
    }
    for n in 0..3 {
        writeln!(
            writer,
            "{}",
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": format!("late-{n}")}]
                }
            })
        )
        .unwrap();
    }
    writer.flush().unwrap();
    drop(writer);
    assert!(std::fs::metadata(&path).unwrap().len() > 2_000_000);

    let events = read_transcript_tail(&path).expect("read tail");
    assert_eq!(
        events.last(),
        Some(&TailEvent::Agent("late-2".to_string())),
        "the newest message must survive"
    );
    assert!(
        events.len() < 1_003,
        "the whole file was parsed: {} events",
        events.len()
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            TailEvent::Agent(text) if text.starts_with("early-0 ")
        )),
        "a message megabytes back from the end must never be read"
    );
}
