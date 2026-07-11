mod common;

use codex_viewer_core::codex::rollout::{
    has_task_complete_tail, read_session_meta, read_transcript, TranscriptItem,
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
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
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
