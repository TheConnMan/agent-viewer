mod common;

use agent_viewer_core::backend::{Backend, BackendKind, Status};
use agent_viewer_core::claude::{ClaudeBackend, parse_agents_json};
use std::path::PathBuf;

#[test]
fn claude_parse_maps_fields_and_states() {
    let json = common::read_fixture("claude_agents.json");
    let sessions = parse_agents_json(&json).expect("parse agents json");
    // 5 valid entries (the malformed missing-sessionId entry is skipped).
    assert_eq!(sessions.len(), 5);

    let working = sessions.iter().find(|s| s.title == "Working Task").unwrap();
    assert_eq!(working.backend, BackendKind::Claude);
    assert_eq!(working.id, "aaaaaaaa-6603-4ad5-be5b-f3ad6391d595"); // id = sessionId
    assert_eq!(working.cwd, PathBuf::from("/home/user/proj-a"));
    assert_eq!(working.created_at_ms, 1783659603260); // = startedAt (epoch-ms int)
    assert_eq!(working.updated_at_ms, 1783659603260);
    assert_eq!(working.source_label, "background"); // = kind
    assert!(!working.hidden);
    assert_eq!(working.status, Status::Running);

    let done = sessions.iter().find(|s| s.title == "Done Task").unwrap();
    assert_eq!(done.status, Status::Done);

    let blocked = sessions.iter().find(|s| s.title == "Blocked Task").unwrap();
    assert_eq!(blocked.status, Status::Errored); // blocked = needs attention
}

#[test]
fn claude_parse_skips_malformed_and_maps_unknown_state() {
    let json = common::read_fixture("claude_agents.json");
    let sessions = parse_agents_json(&json).expect("parse agents json");

    // Malformed entry (missing sessionId) is absent.
    assert!(
        sessions
            .iter()
            .all(|s| s.title != "Orphan Missing SessionId")
    );
    assert_eq!(sessions.len(), 5);

    // Unknown state string -> Errored (attention-safe default).
    let unknown = sessions.iter().find(|s| s.title == "Paused Task").unwrap();
    assert_eq!(unknown.status, Status::Errored);

    // Entry missing the optional `pid` field still parses.
    let nopid = sessions.iter().find(|s| s.title == "No Pid Task").unwrap();
    assert_eq!(nopid.status, Status::Running);
}

#[test]
fn claude_missing_binary_lists_empty() {
    // A missing binary is a quiet empty backend, not an error.
    let mut backend = ClaudeBackend::with_binary("/nonexistent/claude");
    let sessions = backend.list().expect("missing binary must be Ok(empty)");
    assert!(sessions.is_empty());
}
