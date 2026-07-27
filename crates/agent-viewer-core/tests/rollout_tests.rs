mod common;

use agent_viewer_core::codex::rollout::{
    PendingApproval, TailState, TranscriptItem, pending_approval, read_session_meta,
    read_transcript, tail_state,
};
use std::io::Write;
use std::path::PathBuf;

// --- Preserved v1 tests (unchanged behavior) ---

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

    let items = read_transcript(&path).expect("read transcript");
    assert_eq!(
        items,
        vec![
            TranscriptItem {
                role: "user".to_string(),
                text: "hello".to_string(),
            },
            TranscriptItem {
                role: "assistant".to_string(),
                text: "world".to_string(),
            },
        ]
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
