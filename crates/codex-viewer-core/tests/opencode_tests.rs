mod common;

use codex_viewer_core::Status;
use codex_viewer_core::backend::{Backend, BackendKind};
use codex_viewer_core::opencode::{OpencodeBackend, opencode_status};
use std::path::PathBuf;

#[test]
fn opencode_lists_rows_hidden_and_order() {
    let schema = common::read_fixture("opencode_session_schema.sql");
    let inserts = [
        // active parent
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_parent',NULL,'/home/user/oc-proj','Parent',1000,3000,NULL)",
        // active child (parent_id set -> source_label "subagent")
        "INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, time_archived) \
         VALUES ('ses_child','ses_parent','/home/user/oc-proj','Child',1100,2000,NULL)",
        // archived (time_archived set -> hidden)
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

    let child = &sessions[1];
    assert_eq!(child.source_label, "subagent"); // parent_id non-NULL
    assert!(!child.hidden);

    let arch = &sessions[2];
    assert!(arch.hidden); // time_archived IS NOT NULL
}

#[test]
fn opencode_missing_db_lists_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut backend = OpencodeBackend::with_db(dir.path().join("nope.db"));
    let sessions = backend.list().expect("missing DB must be Ok(empty)");
    assert!(sessions.is_empty());
}

#[test]
fn opencode_status_heuristic() {
    let now = 1_000_000_000_000_i64;
    // live process + recent update -> Running
    assert_eq!(opencode_status(true, now - 5_000, now), Status::Running);
    // no live process -> Done regardless of recency
    assert_eq!(opencode_status(false, now - 5_000, now), Status::Done);
    // live process but stale (> 60s) -> Done
    assert_eq!(opencode_status(true, now - 120_000, now), Status::Done);
    // boundary: exactly 60s ago is still Running (<= 60_000)
    assert_eq!(opencode_status(true, now - 60_000, now), Status::Running);
}
