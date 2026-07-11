mod common;

use codex_viewer_core::backend::Status;
use codex_viewer_core::codex::status::{resolve_status, StatusResolver};
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

fn empty_set() -> HashSet<PathBuf> {
    HashSet::new()
}

#[test]
fn running_wins_over_task_complete() {
    // A completed rollout whose file is still held open by a live process reports Running.
    let path = common::fixture_path("rollout_complete.jsonl");
    let canon = std::fs::canonicalize(&path).unwrap();
    let mut open = HashSet::new();
    open.insert(canon);
    assert_eq!(resolve_status(&path, &open), Status::Running);
}

#[test]
fn done_when_complete_and_not_open() {
    let path = common::fixture_path("rollout_complete.jsonl");
    assert_eq!(resolve_status(&path, &empty_set()), Status::Done);
}

#[test]
fn errored_when_midturn_and_not_open() {
    let path = common::fixture_path("rollout_midturn.jsonl");
    assert_eq!(resolve_status(&path, &empty_set()), Status::Errored);
}

#[test]
fn errored_when_file_missing() {
    let path = PathBuf::from("/nonexistent/rollout-does-not-exist.jsonl");
    assert_eq!(resolve_status(&path, &empty_set()), Status::Errored);
}

#[test]
fn resolver_recomputes_on_file_change() {
    let (_dir, path) = common::copy_fixture_to_temp("rollout_midturn.jsonl");
    let mut resolver = StatusResolver::new();
    // Mid-turn, not held open -> Errored.
    assert_eq!(resolver.resolve(&path, &empty_set()), Status::Errored);

    // Append token_count + task_complete: (mtime, len) changes, cache must invalidate.
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{}},"rate_limits":null}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"t","completed_at":1,"duration_ms":1}}}}"#
    )
    .unwrap();
    drop(f);

    assert_eq!(resolver.resolve(&path, &empty_set()), Status::Done);
}

#[test]
fn resolver_result_matches_pure_function() {
    let complete = common::fixture_path("rollout_complete.jsonl");
    let midturn = common::fixture_path("rollout_midturn.jsonl");
    let missing = PathBuf::from("/nonexistent/x.jsonl");
    let mut resolver = StatusResolver::new();
    for p in [&complete, &midturn, &missing] {
        assert_eq!(resolver.resolve(p, &empty_set()), resolve_status(p, &empty_set()));
    }
}
