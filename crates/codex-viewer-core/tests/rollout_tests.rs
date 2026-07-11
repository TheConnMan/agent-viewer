mod common;

use codex_viewer_core::codex::rollout::{
    TranscriptItem, has_task_complete_tail, read_session_meta, read_transcript,
};
use std::io::Write;
use std::path::PathBuf;

#[test]
fn session_meta_parses_first_line() {
    let path = common::fixture_path("rollout_complete.jsonl");
    let meta = read_session_meta(&path).expect("parse session_meta");
    assert_eq!(meta.id, "019f4dda-fecb-7b71-adba-9fda570e4cdb");
    assert_eq!(meta.cwd, PathBuf::from("/home/user/project"));
    assert_eq!(meta.originator, "codex_exec");
    assert_eq!(meta.cli_version, "0.144.1");
}

#[test]
fn session_meta_rejects_empty_and_garbage() {
    let empty_dir = tempfile::TempDir::new().unwrap();
    let empty = empty_dir.path().join("empty.jsonl");
    std::fs::write(&empty, b"").unwrap();
    assert!(read_session_meta(&empty).is_err());

    let garbage = common::fixture_path("rollout_garbage.jsonl");
    assert!(read_session_meta(&garbage).is_err());
}

#[test]
fn task_complete_detected_in_tail() {
    let path = common::fixture_path("rollout_complete.jsonl");
    assert!(has_task_complete_tail(&path).expect("read tail"));
}

#[test]
fn task_complete_absent_mid_turn() {
    let path = common::fixture_path("rollout_midturn.jsonl");
    assert!(!has_task_complete_tail(&path).expect("read tail"));
}

#[test]
fn task_complete_found_when_not_last_line() {
    // Append two trailing non-terminal events after task_complete: the check must scan a
    // tail *window*, not just the final line.
    let (_dir, path) = common::copy_fixture_to_temp("rollout_complete.jsonl");
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
        r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{}},"rate_limits":null}}}}"#
    )
    .unwrap();
    drop(f);
    assert!(has_task_complete_tail(&path).expect("read tail"));
}

#[test]
fn task_complete_stale_before_abandoned_resume() {
    // Regression: a session completes a turn (task_complete), is resumed, then abandoned
    // mid-turn. The STALE task_complete is still inside the tail window, but the resumed
    // turn emitted a later `task_started` (plus a user message) and never a new
    // task_complete. Done requires the last task_complete to occur AFTER the last
    // task_started, so this must resolve to NOT done (else it is misclassified Done).
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("rollout_stale_resume.jsonl");
    let lines = [
        r#"{"timestamp":"2026-07-10T21:00:00.000Z","type":"session_meta","payload":{"id":"019f-stale","cwd":"/home/user/project","originator":"codex_exec","cli_version":"0.144.1","source":"exec"}}"#,
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{},"rate_limits":null}}"#,
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":"first turn done","completed_at":1783716000,"duration_ms":1000}}"#,
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
        r#"{"type":"response_item","payload":{"type":"message","id":"user_resume","role":"user","content":[{"type":"input_text","text":"Now do more work."}]}}"#,
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    assert!(!has_task_complete_tail(&path).expect("read tail"));
}

#[test]
fn transcript_extracts_text_in_order() {
    let path = common::fixture_path("rollout_complete.jsonl");
    let items = read_transcript(&path).expect("read transcript");
    // Exactly the user then assistant message; the function_call item is skipped.
    assert_eq!(
        items,
        vec![
            TranscriptItem {
                role: "user".to_string(),
                text: "Reply with exactly the word DONE.".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "DONE".to_string(),
            },
        ]
    );
}
