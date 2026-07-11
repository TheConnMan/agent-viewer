mod common;

use agent_viewer_core::backend::{Backend, BackendKind, PrRef, Status};
use agent_viewer_core::claude::{
    ClaudeBackend, parse_agents_json, parse_claude_json_models, parse_job_state,
    read_claude_transcript,
};
use agent_viewer_core::codex::rollout::TranscriptItem;
use std::io::Write;
use std::path::PathBuf;

/// Find the session whose title matches. parse_agents_json now returns `Vec<Session>`
/// with the short id folded into `Session.short_id` (no more (Session, String) tuple).
fn by_title<'a>(
    parsed: &'a [agent_viewer_core::Session],
    title: &str,
) -> &'a agent_viewer_core::Session {
    parsed
        .iter()
        .find(|s| s.title == title)
        .unwrap_or_else(|| panic!("no session titled {title}"))
}

#[test]
fn claude_parse_maps_six_states_and_pid() {
    let json = common::read_fixture("claude_agents_all.json");
    let parsed = parse_agents_json(&json).expect("parse agents json");
    // 7 valid entries (the malformed missing-sessionId entry is skipped).
    assert_eq!(parsed.len(), 7);
    assert!(parsed.iter().all(|s| s.title != "Orphan Missing SessionId"));

    let working = by_title(&parsed, "Working Task");
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
    // short id (entry "id") is now folded into the Session itself.
    assert_eq!(working.short_id, Some("work0001".to_string()));

    assert_eq!(by_title(&parsed, "Blocked Task").status, Status::NeedsInput);
    assert_eq!(by_title(&parsed, "Idle Task").status, Status::Idle);
    assert_eq!(by_title(&parsed, "Done Task").status, Status::Done);
    assert_eq!(by_title(&parsed, "Failed Task").status, Status::Failed);
    assert_eq!(by_title(&parsed, "Stopped Task").status, Status::Stopped);

    // Missing state field -> Idle (verified live occurrence), pid absent -> None.
    let nostate = by_title(&parsed, "No State Task");
    assert_eq!(nostate.status, Status::Idle);
    assert_eq!(nostate.pid, None);
    assert_eq!(nostate.short_id, Some("nost0001".to_string()));
}

#[test]
fn parse_claude_json_models_cache_first_then_sorted_usage_union() {
    let json = common::read_fixture("claude_config_models.json");
    let got = parse_claude_json_models(&json);
    // additionalModelOptionsCache values first, in array order; then the union of all
    // projects' lastModelUsage keys sorted ascending (overlapping sonnet appears once).
    assert_eq!(
        got,
        vec![
            "opusplan",
            "sonnet[1m]",
            "claude-haiku-4-5",
            "claude-opus-4-8",
            "claude-sonnet-4-5",
        ]
    );
}

#[test]
fn parse_claude_json_models_malformed_is_empty() {
    assert!(parse_claude_json_models("not json").is_empty());
    assert!(parse_claude_json_models("{}").is_empty());
}

#[test]
fn claude_parse_rejects_non_array_top_level() {
    // Documented contract: a non-array top level is Err(Json), not silently empty.
    assert!(parse_agents_json("{\"sessions\": []}").is_err());
    assert!(parse_agents_json("not json").is_err());
}

#[test]
fn parse_job_state_blocked_prefers_needs() {
    let text = common::read_fixture("claude_state_blocked.json");
    let detail = parse_job_state(&text);
    assert_eq!(
        detail.summary,
        "Approve running the migration against the live DB"
    );
    assert_eq!(
        detail.transcript_path,
        Some(PathBuf::from(
            "/home/user/.claude/jobs/block001/transcript.jsonl"
        ))
    );
    // children[kind=="pr"] only — the "note" child is skipped, order preserved. Each
    // carries its id and github href.
    assert_eq!(
        detail.prs,
        vec![
            PrRef {
                id: "315".to_string(),
                href: Some("https://github.com/acme/repo/pull/315".to_string()),
            },
            PrRef {
                id: "318".to_string(),
                href: Some("https://github.com/acme/repo/pull/318".to_string()),
            },
        ]
    );
}

#[test]
fn parse_job_state_no_children_has_empty_prs() {
    let text = common::read_fixture("claude_state_working.json");
    let detail = parse_job_state(&text);
    assert!(detail.prs.is_empty());
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

#[test]
fn read_claude_transcript_skips_text_empty_message() {
    // An assistant message whose content is only a thinking/tool block extracts to "" and
    // must be dropped (no blank role-only line in peek).
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("empty_assistant.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"user","message":{{"content":"a real user line"}}}}"#
    )
    .unwrap();
    // Only thinking + tool_use blocks -> text == "" -> skipped.
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"content":[{{"type":"thinking","thinking":"hmm"}},{{"type":"tool_use","name":"bash"}}]}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"a real reply"}}]}}}}"#
    )
    .unwrap();
    drop(f);

    let items = read_claude_transcript(&path, 100).expect("read transcript");
    assert_eq!(
        items,
        vec![
            TranscriptItem {
                role: "user".to_string(),
                text: "a real user line".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "a real reply".to_string(),
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
