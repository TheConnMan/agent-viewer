mod common;

use agent_viewer_core::backend::{Backend, BackendKind, Status};
use agent_viewer_core::claude::{
    ClaudeBackend, parse_agents_json, parse_job_state, read_claude_transcript,
};
use agent_viewer_core::codex::rollout::TranscriptItem;
use std::path::PathBuf;

/// Find the (session, short_id) pair whose session title matches.
fn by_title<'a>(
    parsed: &'a [(agent_viewer_core::Session, String)],
    title: &str,
) -> &'a (agent_viewer_core::Session, String) {
    parsed
        .iter()
        .find(|(s, _)| s.title == title)
        .unwrap_or_else(|| panic!("no session titled {title}"))
}

#[test]
fn claude_parse_maps_six_states_and_pid() {
    let json = common::read_fixture("claude_agents_all.json");
    let parsed = parse_agents_json(&json).expect("parse agents json");
    // 7 valid entries (the malformed missing-sessionId entry is skipped).
    assert_eq!(parsed.len(), 7);
    assert!(parsed.iter().all(|(s, _)| s.title != "Orphan Missing SessionId"));

    let (working, working_short) = by_title(&parsed, "Working Task");
    assert_eq!(working.backend, BackendKind::Claude);
    assert_eq!(working.id, "work0001-6603-4ad5-be5b-f3ad6391d595"); // id = sessionId
    assert_eq!(working.cwd, PathBuf::from("/home/user/proj-working"));
    assert_eq!(working.created_at_ms, 1783659603260); // = startedAt
    assert_eq!(working.updated_at_ms, 1783659603260);
    assert_eq!(working.source_label, "background"); // = kind
    assert!(!working.hidden);
    assert!(!working.companion); // claude rows are never companions
    assert_eq!(working.status, Status::Working);
    assert_eq!(working.pid, Some(111));
    assert_eq!(working_short, "work0001"); // short id = entry "id"

    assert_eq!(by_title(&parsed, "Blocked Task").0.status, Status::NeedsInput);
    assert_eq!(by_title(&parsed, "Idle Task").0.status, Status::Idle);
    assert_eq!(by_title(&parsed, "Done Task").0.status, Status::Done);
    assert_eq!(by_title(&parsed, "Failed Task").0.status, Status::Failed);
    assert_eq!(by_title(&parsed, "Stopped Task").0.status, Status::Stopped);

    // Missing state field -> Idle (verified live occurrence), pid absent -> None.
    let (nostate, _) = by_title(&parsed, "No State Task");
    assert_eq!(nostate.status, Status::Idle);
    assert_eq!(nostate.pid, None);
}

#[test]
fn parse_job_state_blocked_prefers_needs() {
    let text = common::read_fixture("claude_state_blocked.json");
    let detail = parse_job_state(&text);
    assert_eq!(detail.summary, "Approve running the migration against the live DB");
    assert_eq!(
        detail.transcript_path,
        Some(PathBuf::from(
            "/home/user/.claude/jobs/block001/transcript.jsonl"
        ))
    );
}

#[test]
fn parse_job_state_working_uses_detail_and_empty_default() {
    let text = common::read_fixture("claude_state_working.json");
    let detail = parse_job_state(&text);
    // No `needs`, so the summary falls back to `detail`.
    assert_eq!(detail.summary, "Running the test suite");

    // Empty object -> empty summary, no transcript path.
    let empty = parse_job_state("{}");
    assert_eq!(empty.summary, "");
    assert_eq!(empty.transcript_path, None);
}

#[test]
fn read_claude_transcript_extracts_text_tail() {
    let path = common::fixture_path("claude_transcript.jsonl");

    // Full read: string-content user, list-content user, assistant text; thinking,
    // tool_use, and system/attachment/queue noise all skipped.
    let all = read_claude_transcript(&path, 100).expect("read transcript");
    assert_eq!(
        all,
        vec![
            TranscriptItem {
                role: "user".to_string(),
                text: "first user line as a plain string".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "assistant reply one".to_string(),
            },
            TranscriptItem {
                role: "user".to_string(),
                text: "second user line from a block list".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "assistant reply two".to_string(),
            },
        ]
    );

    // max_items caps to the LAST two.
    let tail = read_claude_transcript(&path, 2).expect("read transcript");
    assert_eq!(
        tail,
        vec![
            TranscriptItem {
                role: "user".to_string(),
                text: "second user line from a block list".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "assistant reply two".to_string(),
            },
        ]
    );
}

// --- Preserved v1 test ---

#[test]
fn claude_missing_binary_lists_empty() {
    // A missing binary is a quiet empty backend, not an error.
    let mut backend = ClaudeBackend::with_binary("/nonexistent/claude");
    let sessions = backend.list().expect("missing binary must be Ok(empty)");
    assert!(sessions.is_empty());
}
