//! Grok PR refs come from `chat_history.jsonl`: an explicit PR-creation tool call
//! paired with its successful `tool_result` by `tool_call_id`. Grok records PRs
//! nowhere else. These fixtures are the live JSONL shape (assistant `tool_calls`
//! with `id`/`name`/`arguments`, and `tool_result` records with `tool_call_id` plus
//! a `content` string). Reverting `list` to `pr_refs: Vec::new()` fails here.

use agent_viewer_core::GrokLifecycle;
use std::path::{Path, PathBuf};

fn fixture_home() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok/pr-scan")
}

fn hrefs(refs: &[agent_viewer_core::PrRef]) -> Vec<String> {
    refs.iter()
        .map(|r| r.href.clone().unwrap_or_default())
        .collect()
}

fn list_fixture_rows() -> Vec<agent_viewer_core::Session> {
    GrokLifecycle::new("missing-grok", fixture_home())
        .list()
        .expect("durable Grok PR-scan fixture listing")
}

fn find<'a>(rows: &'a [agent_viewer_core::Session], id: &str) -> &'a agent_viewer_core::Session {
    rows.iter()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("missing fixture session {id}"))
}

// The bug this module exists for: Grok session construction hardcoded
// `pr_refs: Vec::new()`, so a session that successfully opened PRs showed no
// badge. A successful `gh pr create` and a successful `create_pull_request`
// connector call both populate Session.pr_refs, which the existing TUI badge
// and live-status path already consume. Duplicate URLs and a `/files` suffix
// collapse to the canonical href. Incidental PR links in the same history
// (a prior `gh pr view`, user prose, grep of SPEC.md) are not a source.
#[test]
fn list_populates_pr_refs_from_successful_grok_pr_creates() {
    let rows = list_fixture_rows();
    let created = find(&rows, "session-created");
    assert_eq!(
        hrefs(&created.pr_refs),
        vec![
            "https://github.com/TheConnMan/agent-viewer/pull/42".to_string(),
            "https://github.com/TheConnMan/agent-viewer/pull/99".to_string(),
        ]
    );
    assert_eq!(created.pr_refs[0].id, "42");
    assert_eq!(created.pr_refs[1].id, "99");
}

// Failed, unpaired, and incidental-link histories must stay empty. A scanner
// that badges any github.com/.../pull/n URL, or that does not require a
// matching successful creation, starts failing these four assertions.
#[test]
fn list_does_not_badge_failed_unpaired_or_incidental_pr_urls() {
    let rows = list_fixture_rows();
    assert!(
        find(&rows, "session-failed").pr_refs.is_empty(),
        "nonzero exit and is_error connector results must not badge"
    );
    assert!(
        find(&rows, "session-unpaired").pr_refs.is_empty(),
        "a tool_result without a matching PR-create tool_call_id must not badge"
    );
    assert!(
        find(&rows, "session-incidental").pr_refs.is_empty(),
        "research, issue history, gh pr view/list, python that mentions gh pr create, and arbitrary tool output must not badge"
    );
}
