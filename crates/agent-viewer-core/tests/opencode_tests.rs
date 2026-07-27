mod common;

use agent_viewer_core::Status;
use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Session, SessionOrigin};
use agent_viewer_core::opencode::{
    OpencodeBackend, is_run_mode_permission, opencode_status, parse_opencode_models,
    read_opencode_last_message, rename_sql,
};
use std::path::PathBuf;

// --- Preserved v1 listing shape (order / labels / hidden) ---

#[test]
fn opencode_lists_rows_hidden_and_order() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_parent',NULL,'/home/user/oc-proj','Parent',1000,3000,NULL)",
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_child','ses_parent','/home/user/oc-proj','Child',1100,2000,NULL)",
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_arch',NULL,'/home/user/oc-proj','Archived',900,1000,5000)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let mut backend = OpencodeBackend::with_db(path);
    let sessions = backend.list().expect("list opencode sessions");
    assert_eq!(sessions.len(), 3);

    // time_updated DESC: parent (3000), child (2000), arch (1000).
    assert_eq!(sessions[0].id, "ses_parent");
    assert_eq!(sessions[1].id, "ses_child");
    assert_eq!(sessions[2].id, "ses_arch");

    let parent = &sessions[0];
    assert_eq!(parent.backend, BackendKind::Opencode);
    assert_eq!(parent.cwd, PathBuf::from("/home/user/oc-proj"));
    assert_eq!(parent.title, "Parent");
    assert_eq!(parent.created_at_ms, 1000);
    assert_eq!(parent.updated_at_ms, 3000);
    assert_eq!(parent.origin, SessionOrigin::Interactive);
    assert!(!parent.hidden);
    assert_eq!(parent.short_id, None); // opencode sessions carry no claude short id

    assert!(sessions[1].companion); // parent_id non-NULL
    assert!(!sessions[1].hidden);
    assert!(sessions[2].hidden); // time_archived IS NOT NULL
}

#[test]
fn opencode_missing_db_lists_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut backend = OpencodeBackend::with_db(dir.path().join("nope.db"));
    let sessions = backend.list().expect("missing DB must be Ok(empty)");
    assert!(sessions.is_empty());
}

// --- v2: three-tier heuristic (test 17) ---

#[test]
fn opencode_status_three_tiers() {
    let now = 1_000_000_000_000_i64;
    // live + fresh -> Working (age <= 60_000, boundary inclusive).
    assert_eq!(opencode_status(true, now - 59_000, now), Status::Working);
    assert_eq!(opencode_status(true, now - 60_000, now), Status::Working);
    // live + <= 30 min -> Idle (boundary inclusive at 1_800_000).
    assert_eq!(opencode_status(true, now - 61_000, now), Status::Idle);
    assert_eq!(opencode_status(true, now - 1_800_000, now), Status::Idle);
    // live but older than 30 min -> Done.
    assert_eq!(opencode_status(true, now - 1_800_001, now), Status::Done);
    // no live process -> Done regardless of recency.
    assert_eq!(opencode_status(false, now - 5_000, now), Status::Done);
}

// --- v2: companion flag from parent_id (test 18) ---

#[test]
fn opencode_lists_companion_flag() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_root',NULL,'/home/user/oc-proj','Root',1000,3000,NULL)",
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_kid','ses_root','/home/user/oc-proj','Kid',1100,2000,NULL)",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let mut backend = OpencodeBackend::with_db(path);
    let sessions = backend.list().expect("list opencode sessions");

    let root = sessions.iter().find(|s| s.id == "ses_root").unwrap();
    let kid = sessions.iter().find(|s| s.id == "ses_kid").unwrap();
    assert!(!root.companion); // parent_id IS NULL
    assert!(kid.companion); // parent_id set -> companion
}

// --- v2: last-message reader (peek) ---

#[test]
fn opencode_last_message_returns_newest_text_concatenated() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_1',NULL,'/home/user/oc-proj','Proj',1000,3000,NULL)",
        // Older assistant message with a single text part.
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_old','ses_1',1000,1000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_old','msg_old','ses_1',1000,1000,'{\"type\":\"text\",\"text\":\"older reply\"}')",
        // Newer assistant message: a tool part THEN a text part (tool must be skipped).
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_new','ses_1',2000,2000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_new_tool','msg_new','ses_1',2001,2001,'{\"type\":\"tool\",\"tool\":\"bash\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_new_text','msg_new','ses_1',2002,2002,'{\"type\":\"text\",\"text\":\"newer reply text\"}')",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let item = read_opencode_last_message(&path, "ses_1")
        .expect("read ok")
        .expect("a text message exists");
    assert_eq!(item.role, "assistant");
    // The NEWER message's text only, tool part skipped.
    assert_eq!(item.text, "newer reply text");
}

#[test]
fn opencode_last_message_skips_whitespace_only_newest() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_1',NULL,'/home/user/oc-proj','Proj',1000,3000,NULL)",
        // Older assistant message with real text.
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_old','ses_1',1000,1000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_old','msg_old','ses_1',1000,1000,'{\"type\":\"text\",\"text\":\"real prior message\"}')",
        // Newest message whose only text part is whitespace (newline-only around a tool
        // transition) -> it must be skipped so the real prior message surfaces.
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_new','ses_1',2000,2000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_new','msg_new','ses_1',2000,2000,'{\"type\":\"text\",\"text\":\"\\n\\n\"}')",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let item = read_opencode_last_message(&path, "ses_1")
        .expect("read ok")
        .expect("a text message exists");
    assert_eq!(item.text, "real prior message");
}

#[test]
fn opencode_last_message_none_when_no_text_message() {
    let schema = common::read_fixture("opencode_message_schema.sql");
    let inserts = [
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_1',NULL,'/home/user/oc-proj','Proj',1000,3000,NULL)",
        // A message whose only part is a tool part -> no text -> Ok(None).
        "INSERT INTO message (id, session_id, time_created, time_updated, data) \
         VALUES ('msg_1','ses_1',1000,1000,'{\"role\":\"assistant\"}')",
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_1','msg_1','ses_1',1000,1000,'{\"type\":\"tool\",\"tool\":\"bash\"}')",
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);
    // Also nothing for an unknown session id.
    assert!(
        read_opencode_last_message(&path, "ses_1")
            .expect("read ok")
            .is_none()
    );
    assert!(
        read_opencode_last_message(&path, "nope")
            .expect("read ok")
            .is_none()
    );
}

#[test]
fn opencode_last_message_missing_db_is_none() {
    let dir = tempfile::TempDir::new().unwrap();
    let missing = dir.path().join("nope.db");
    assert!(
        read_opencode_last_message(&missing, "ses_1")
            .expect("read ok")
            .is_none()
    );
}

// --- v2: `opencode models` stdout parse ---

#[test]
fn parse_opencode_models_trims_and_drops_blanks() {
    let stdout = "\
anthropic/claude-opus-4-8
  openai/gpt-5.6

github-copilot/gpt-5

";
    let got = parse_opencode_models(stdout);
    assert_eq!(
        got,
        vec![
            "anthropic/claude-opus-4-8",
            "openai/gpt-5.6",
            "github-copilot/gpt-5",
        ]
    );
}

// --- per-row stop capability ---

fn session_with_pid(pid: Option<u32>) -> Session {
    Session {
        backend: BackendKind::Opencode,
        id: "ses_cap".to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: "probe".to_string(),
        cwd: PathBuf::from("/tmp"),
        git_branch: None,
        status: Status::Idle,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

/// `stop` signals `session.pid`, so a row without one has no process to terminate. The
/// capability must be advertised per row rather than backend wide, and `capabilities_for`
/// must narrow nothing but `stop`.
#[test]
fn opencode_stop_capability_is_per_row_and_requires_a_pid() {
    let dir = tempfile::TempDir::new().unwrap();
    let backend = OpencodeBackend::with_db(dir.path().join("nope.db"));
    let base = backend.capabilities();
    assert!(base.stop, "backend wide stop stays true");

    let with_pid = backend.capabilities_for(&session_with_pid(Some(4242)));
    let without_pid = backend.capabilities_for(&session_with_pid(None));
    assert!(with_pid.stop, "a row carrying a pid can be stopped");
    assert!(!without_pid.stop, "no pid means no process to signal");
    assert_eq!(
        Capabilities {
            stop: false,
            ..with_pid
        },
        Capabilities {
            stop: false,
            ..base
        }
    );
    assert_eq!(
        Capabilities {
            stop: false,
            ..without_pid
        },
        Capabilities {
            stop: false,
            ..base
        }
    );
}

// --- v2: rename SQL escaping (test 19) ---

#[test]
fn opencode_rename_sql_escapes_quotes() {
    // SQL-92 single-quote doubling in BOTH the title and id literals.
    let sql = rename_sql("ses_1", "it's 'quoted'");
    assert_eq!(
        sql,
        "UPDATE session SET title='it''s ''quoted''' WHERE id='ses_1'"
    );
    // No un-doubled apostrophe survives inside the title literal.
    assert!(sql.contains("''s"));
}

// --- run-mode companions: `opencode run` sessions are one-shots, not fleet members ---

#[test]
fn opencode_run_mode_permission_marks_companion() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    // The exact blob `opencode run` writes (verified live on this box, opencode 1.17.20:
    // a `run` session stores this triple, a TUI session stores NULL).
    let run_perm = "[{\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"deny\"},\
                    {\"permission\":\"plan_enter\",\"pattern\":\"*\",\"action\":\"deny\"},\
                    {\"permission\":\"plan_exit\",\"pattern\":\"*\",\"action\":\"deny\"}]";
    let inserts = [
        // TUI session: no parent, no permission override -> a real fleet row.
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
         VALUES ('ses_tui',NULL,'/home/user/oc-proj','Interactive',1000,5000,NULL,NULL)"
            .to_string(),
        // `opencode run` one-shot (an /implement review pass): no parent, so parent_id
        // alone would have shown it.
        format!(
            "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
             VALUES ('ses_run',NULL,'/home/user/oc-proj','CUR-1667 billing bug fix review',1000,4000,NULL,'{run_perm}')"
        ),
        // A permission override that is NOT the run marker must stay visible.
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
         VALUES ('ses_other',NULL,'/home/user/oc-proj','Custom perms',1000,3000,NULL,'[{\"permission\":\"read\",\"pattern\":\"*\",\"action\":\"allow\"}]')"
            .to_string(),
        // Empty string (not NULL) is the same as no override.
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived, permission) \
         VALUES ('ses_empty',NULL,'/home/user/oc-proj','Empty perms',1000,2000,NULL,'')"
            .to_string(),
    ];
    let refs: Vec<&str> = inserts.iter().map(String::as_str).collect();
    let (_dir, path) = common::temp_db(&schema, &refs);

    let mut backend = OpencodeBackend::with_db(path);
    let sessions = backend.list().expect("list opencode sessions");
    let by = |id: &str| {
        sessions
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("{id} missing"))
            .companion
    };

    assert!(!by("ses_tui"), "interactive TUI session must stay visible");
    assert!(by("ses_run"), "`opencode run` one-shot must be a companion");
    assert!(
        !by("ses_other"),
        "unrelated permission override is not a run marker"
    );
    assert!(!by("ses_empty"), "empty permission is not a run marker");
}

#[test]
fn opencode_run_mode_permission_marker_shapes() {
    // Absent / empty -> interactive.
    assert!(!is_run_mode_permission(None));
    assert!(!is_run_mode_permission(Some("")));
    assert!(!is_run_mode_permission(Some("   ")));
    // Not JSON, or JSON of the wrong shape -> interactive (never panics).
    assert!(!is_run_mode_permission(Some("not json at all")));
    assert!(!is_run_mode_permission(Some("{}")));
    assert!(!is_run_mode_permission(Some("[]")));
    assert!(!is_run_mode_permission(Some(
        "[{\"permission\":\"question\"}]"
    )));
    // A `question` entry that is allowed, not denied -> interactive.
    assert!(!is_run_mode_permission(Some(
        "[{\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"allow\"}]"
    )));
    // The stored key order is not the source order, so order must not matter.
    assert!(is_run_mode_permission(Some(
        "[{\"action\":\"deny\",\"permission\":\"question\",\"pattern\":\"*\"}]"
    )));
    // The github-action variant writes the `question` deny alone, without the plan pair.
    assert!(is_run_mode_permission(Some(
        "[{\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"deny\"}]"
    )));
    // Extra unrelated entries around the marker must not hide it.
    assert!(is_run_mode_permission(Some(
        "[{\"permission\":\"read\",\"pattern\":\"*\",\"action\":\"allow\"},\
          {\"permission\":\"question\",\"pattern\":\"*\",\"action\":\"deny\"}]"
    )));
}
