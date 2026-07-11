mod common;

use agent_viewer_core::backend::Status;
use agent_viewer_core::codex::status::{StatusResolver, resolve_status};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

fn empty_map() -> HashMap<PathBuf, u32> {
    HashMap::new()
}

/// An open map holding the canonical form of `path` mapped to an arbitrary owning PID.
fn open_with(path: &std::path::Path) -> HashMap<PathBuf, u32> {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut map = HashMap::new();
    map.insert(canon, 4242);
    map
}

#[test]
fn needs_input_when_open_and_awaiting() {
    let path = common::fixture_path("rollout_approval.jsonl");
    assert_eq!(resolve_status(&path, &open_with(&path)), Status::NeedsInput);
}

#[test]
fn working_when_open_and_midturn() {
    let path = common::fixture_path("rollout_midturn.jsonl");
    assert_eq!(resolve_status(&path, &open_with(&path)), Status::Working);
}

#[test]
fn idle_when_open_and_complete() {
    // Live session between turns: held open + tail Complete -> Idle.
    let path = common::fixture_path("rollout_complete.jsonl");
    assert_eq!(resolve_status(&path, &open_with(&path)), Status::Idle);
}

#[test]
fn done_when_closed_and_complete() {
    let path = common::fixture_path("rollout_complete.jsonl");
    assert_eq!(resolve_status(&path, &empty_map()), Status::Done);
}

#[test]
fn failed_when_closed_and_midturn() {
    let path = common::fixture_path("rollout_midturn.jsonl");
    assert_eq!(resolve_status(&path, &empty_map()), Status::Failed);
}

#[test]
fn failed_when_file_missing() {
    // Missing file, empty map: Failed, never panics.
    let path = PathBuf::from("/nonexistent/rollout-does-not-exist.jsonl");
    assert_eq!(resolve_status(&path, &empty_map()), Status::Failed);
}

#[test]
fn resolver_matches_pure_and_recomputes_on_change() {
    // The cache wrapper agrees with the pure function across states...
    let complete = common::fixture_path("rollout_complete.jsonl");
    let midturn = common::fixture_path("rollout_midturn.jsonl");
    let missing = PathBuf::from("/nonexistent/x.jsonl");
    let mut resolver = StatusResolver::new();
    for p in [&complete, &midturn, &missing] {
        assert_eq!(
            resolver.resolve(p, &empty_map()),
            resolve_status(p, &empty_map())
        );
    }

    // ...and invalidates its (mtime, len) cache when the file changes.
    let (_dir, path) = common::copy_fixture_to_temp("rollout_midturn.jsonl");
    let mut resolver = StatusResolver::new();
    assert_eq!(resolver.resolve(&path, &empty_map()), Status::Failed);
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"t","completed_at":1,"duration_ms":1}}}}"#
    )
    .unwrap();
    drop(f);
    assert_eq!(resolver.resolve(&path, &empty_map()), Status::Done);
}
