mod common;

use agent_viewer_core::Status;
use agent_viewer_core::backend::{Backend, BackendKind};
use agent_viewer_core::opencode::{OpencodeBackend, opencode_status, rename_sql};
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
    assert_eq!(parent.source_label, "opencode");
    assert!(!parent.hidden);
    assert_eq!(parent.short_id, None); // opencode sessions carry no claude short id

    assert_eq!(sessions[1].source_label, "subagent"); // parent_id non-NULL
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
