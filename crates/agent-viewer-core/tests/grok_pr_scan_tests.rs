//! Grok transcript PR refs come from `chat_history.jsonl`: an explicit PR creation
//! tool call paired with its successful `tool_result` by `tool_call_id`. These
//! fixtures are the live JSONL shape. Transcript evidence is authoritative. Live
//! branch attribution is a later viewer owned decorator and runs only when this
//! scanner returns no refs. Reverting `list` to `pr_refs: Vec::new()` fails here.

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

// This scanner protects direct transcript attribution. A successful `gh pr create`
// and a successful `create_pull_request` connector call both populate
// `Session.pr_refs`, which the existing TUI badge and live status path consume.
// Duplicate URLs and a `/files` suffix collapse to the canonical href. Incidental
// PR links in the same history, including a prior `gh pr view`, user prose, and a
// grep of SPEC.md, are not evidence. The later branch decorator must preserve any
// refs proved here and may act only when this result is empty.
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

// Failed, unpaired, and incidental link histories must stay empty. A scanner
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

/// Live Grok histories are per-job files (median ~200 KiB). Charging every
/// session against one 32 MiB cap in directory order skipped later cwds forever,
/// including Entlify jobs whose own transcripts already contained `gh pr create`
/// URLs. Newest-first reads plus free cache hits are what let each of those
/// files get scanned.
#[cfg(target_os = "linux")]
#[test]
fn list_scans_each_job_history_newest_first_and_keeps_cached_refs() {
    const FILLER_BYTES: usize = 1_900_000;
    const FILLER_COUNT: usize = 18;

    let home = tempfile::tempdir().expect("temporary Grok home");
    write_pr_scan_session(
        home.path(),
        "aaa-old-pr",
        "2026-08-01T00:00:00.000Z",
        &padded_pr_history(7, FILLER_BYTES),
    );
    let filler = padded_filler_history(FILLER_BYTES);
    for index in 0..FILLER_COUNT {
        write_pr_scan_session(
            home.path(),
            &format!("filler-{index:02}"),
            "2026-09-01T00:00:00.000Z",
            &filler,
        );
    }
    write_pr_scan_session(
        home.path(),
        "zzz-new-pr",
        "2026-09-21T14:00:00.000Z",
        grok_pr_create_history(42).as_bytes(),
    );

    let lifecycle = GrokLifecycle::new("missing-grok", home.path());
    let first = lifecycle.list().expect("first budgeted PR listing");
    assert_eq!(
        hrefs(&find(&first, "zzz-new-pr").pr_refs),
        vec!["https://github.com/TheConnMan/agent-viewer/pull/42".to_string()],
        "the newest job transcript must be scanned on a cold pass even when older files exceed the read budget"
    );
    assert!(
        find(&first, "aaa-old-pr").pr_refs.is_empty(),
        "an older large transcript must wait when the unread budget is spent on newer jobs"
    );

    let second = lifecycle.list().expect("second budgeted PR listing");
    assert_eq!(
        hrefs(&find(&second, "zzz-new-pr").pr_refs),
        vec!["https://github.com/TheConnMan/agent-viewer/pull/42".to_string()],
        "a cache hit must keep the newest job's refs instead of hiding them to save budget"
    );
    assert_eq!(
        hrefs(&find(&second, "aaa-old-pr").pr_refs),
        vec!["https://github.com/TheConnMan/agent-viewer/pull/7".to_string()],
        "free cache hits must leave budget to scan the job transcripts the first pass skipped"
    );
}

#[cfg(target_os = "linux")]
fn grok_pr_create_history(number: u64) -> String {
    let assistant = serde_json::json!({
        "type": "assistant",
        "content": "Creating the pull request.",
        "tool_calls": [{
            "id": "call-create-1",
            "name": "run_terminal_command",
            "arguments": "{\"command\": \"gh pr create --title t --body b\", \"description\": \"Create GitHub pull request\"}"
        }]
    });
    let result = serde_json::json!({
        "type": "tool_result",
        "tool_call_id": "call-create-1",
        "content": format!("exit: 0\nhttps://github.com/TheConnMan/agent-viewer/pull/{number}\n")
    });
    format!("{assistant}\n{result}\n")
}

#[cfg(target_os = "linux")]
fn padded_filler_history(min_bytes: usize) -> Vec<u8> {
    let line = b"{\"type\":\"assistant\",\"content\":\"budget filler\"}\n";
    let mut body = Vec::with_capacity(min_bytes + line.len());
    while body.len() < min_bytes {
        body.extend_from_slice(line);
    }
    body
}

#[cfg(target_os = "linux")]
fn padded_pr_history(number: u64, min_bytes: usize) -> Vec<u8> {
    let mut body = padded_filler_history(min_bytes);
    body.extend_from_slice(grok_pr_create_history(number).as_bytes());
    body
}

#[cfg(target_os = "linux")]
fn write_pr_scan_session(home: &Path, session_id: &str, updated_at: &str, history: &[u8]) {
    let directory = home.join("sessions/project").join(session_id);
    std::fs::create_dir_all(&directory).expect("PR-scan session directory");
    let summary = serde_json::json!({
        "info":{"id":session_id,"cwd":"/home/user/project"},
        "session_summary":session_id,
        "created_at":"2026-08-20T01:02:03.004Z",
        "updated_at":updated_at,
        "num_messages":2,
        "current_model_id":"grok-4.6"
    });
    std::fs::write(directory.join("summary.json"), summary.to_string()).expect("PR-scan summary");
    std::fs::write(directory.join("chat_history.jsonl"), history).expect("PR-scan history");
}
