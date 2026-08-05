//! Codex PR refs come from the successful `gh pr create` call and result in the rollout JSONL.
//! The registry has no PR column, and `threads.git_branch` is captured at thread start so it
//! goes stale the moment the agent branches. These cover URL parsing within successful
//! creation results, the incremental scanner that keeps the per-tick cost bounded, and the
//! end-to-end population of `Session.pr_refs` from `CodexBackend::list`.

mod common;

use agent_viewer_core::backend::Backend;
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::codex::pr_scan::{MAX_REFS_PER_SESSION, PrScanner, SCAN_BUDGET_BYTES};
use std::io::Write;
use std::path::Path;

/// One rollout line carrying `text` inside a plausible JSONL envelope.
fn line(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "content": [{"type": "output_text", "text": text}]}
        })
    )
}

fn custom_tool_call(call_id: &str, name: &str, input: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "call_id": call_id,
                "name": name,
                "input": input,
            }
        })
    )
}

fn gh_pr_create_call(call_id: &str) -> String {
    custom_tool_call(
        call_id,
        "exec",
        r#"const r = await tools.exec_command({cmd:"gh pr create"});"#,
    )
}

fn command_output(call_id: &str, exit_code: i32, output: &str) -> String {
    let result = serde_json::json!({
        "chunk_id": "test",
        "wall_time_seconds": 1.4,
        "exit_code": exit_code,
        "original_token_count": 20,
        "output": output,
    })
    .to_string();
    format!(
        "{}\n",
        serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": call_id,
                "output": [
                    {"type": "input_text", "text": "Script completed\nOutput:\n"},
                    {"type": "input_text", "text": result},
                ],
            }
        })
    )
}

fn append(path: &Path, contents: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open rollout for append");
    file.write_all(contents.as_bytes()).expect("append");
    file.flush().expect("flush");
}

fn hrefs(refs: &[agent_viewer_core::PrRef]) -> Vec<String> {
    refs.iter()
        .map(|r| r.href.clone().unwrap_or_default())
        .collect()
}

/// Drive the live incremental scanner over successful `gh pr create` output.
fn refs_in(text: &str) -> Vec<agent_viewer_core::PrRef> {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    append(&path, &gh_pr_create_call("call_extract"));
    append(&path, &command_output("call_extract", 0, text));
    let mut budget = SCAN_BUDGET_BYTES;
    PrScanner::new().refs_for(&path, &mut budget)
}

// --- pure extraction ---

// The extractor keeps only real github PR links, dedupes repeats (agentos/399 appears 51
// times in one real rollout), canonicalizes a deep link (/files) back to the PR, and leaves
// the display id as the bare number the badge renders. A stub returning every URL, or one
// that skipped dedupe, fails here.
#[test]
fn extract_keeps_distinct_github_prs_only() {
    let text = concat!(
        "opened https://github.com/example-org/example-repo/pull/1089 and ",
        "https://github.com/example-org/example-repo/pull/1089 again, plus ",
        "https://github.com/example-org/example-repo/pull/1088/files, see also ",
        "https://github.com/example-org/example-repo/issues/77 and ",
        "https://gitlab.com/o/r/pull/5 and https://github.com/example-org/example-repo ",
        "and https://api.github.com/repos/example-org/example-repo/pulls/12"
    );
    let refs = refs_in(text);
    assert_eq!(
        hrefs(&refs),
        vec![
            "https://github.com/example-org/example-repo/pull/1089".to_string(),
            "https://github.com/example-org/example-repo/pull/1088".to_string(),
        ]
    );
    assert_eq!(refs[0].id, "1089");
    assert_eq!(refs[1].id, "1088");
}

// URLs in a rollout are embedded in JSON, so the character right after the number is almost
// always a quote, a comma, or an escape. The extractor must stop at the path charset rather
// than swallow the delimiter into the repo or number.
#[test]
fn extract_stops_at_json_delimiters() {
    let text = r#"{"url":"https://github.com/example-org/example-app/pull/399","n":1} (https://github.com/o/r/pull/7)"#;
    assert_eq!(
        hrefs(&refs_in(text)),
        vec![
            "https://github.com/example-org/example-app/pull/399".to_string(),
            "https://github.com/o/r/pull/7".to_string(),
        ]
    );
}

// --- incremental scanner ---

#[test]
fn scanner_reports_pr_written_into_the_rollout() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    append(&path, &line("no links here"));
    append(&path, &gh_pr_create_call("call_created"));
    append(
        &path,
        &command_output(
            "call_created",
            0,
            "https://github.com/example-org/example-repo/pull/1089",
        ),
    );

    let mut scanner = PrScanner::new();
    let mut budget = SCAN_BUDGET_BYTES;
    assert_eq!(
        hrefs(&scanner.refs_for(&path, &mut budget)),
        vec!["https://github.com/example-org/example-repo/pull/1089".to_string()]
    );
}

#[test]
fn scanner_reports_only_pr_created_by_matching_gh_command() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    let created_url = "https://github.com/example-org/example-repo/pull/1089";

    append(
        &path,
        &line("reviewing https://github.com/example-org/example-repo/pull/1088"),
    );
    append(&path, &gh_pr_create_call("call_FRPkAWWvQ5m9v9AkofnESCvX"));
    append(
        &path,
        &command_output(
            "call_unrelated",
            0,
            "https://github.com/example-org/example-repo/pull/1087",
        ),
    );
    append(
        &path,
        &command_output("call_FRPkAWWvQ5m9v9AkofnESCvX", 0, created_url),
    );

    let mut scanner = PrScanner::new();
    let mut budget = SCAN_BUDGET_BYTES;
    assert_eq!(
        hrefs(&scanner.refs_for(&path, &mut budget)),
        vec![created_url.to_string()]
    );
}

#[test]
fn scanner_ignores_pr_from_failed_matching_gh_command() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    let failed_url = "https://github.com/example-org/example-repo/pull/1090";

    append(&path, &gh_pr_create_call("call_failed_create"));
    append(&path, &command_output("call_failed_create", 1, failed_url));

    let mut scanner = PrScanner::new();
    let mut budget = SCAN_BUDGET_BYTES;
    assert!(scanner.refs_for(&path, &mut budget).is_empty());
}

#[test]
fn scanner_ignores_pr_from_matching_nonexec_call() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    append(
        &path,
        &custom_tool_call(
            "call_nonexec",
            "read_file",
            r#"const r = await tools.exec_command({cmd:"gh pr create"});"#,
        ),
    );
    append(
        &path,
        &command_output(
            "call_nonexec",
            0,
            "https://github.com/example-org/example-repo/pull/1091",
        ),
    );

    let mut scanner = PrScanner::new();
    let mut budget = SCAN_BUDGET_BYTES;
    assert!(scanner.refs_for(&path, &mut budget).is_empty());
}

#[test]
fn scanner_ignores_pr_from_exec_that_did_not_create_one() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    append(
        &path,
        &custom_tool_call(
            "call_noncreate",
            "exec",
            r#"const r = await tools.exec_command({cmd:"gh pr view"});"#,
        ),
    );
    append(
        &path,
        &command_output(
            "call_noncreate",
            0,
            "https://github.com/example-org/example-repo/pull/1092",
        ),
    );

    let mut scanner = PrScanner::new();
    let mut budget = SCAN_BUDGET_BYTES;
    assert!(scanner.refs_for(&path, &mut budget).is_empty());
}

// The whole point of the offset cache: a second pass over an unchanged file reads zero
// bytes, and a pass over a grown file reads only the delta. Asserting on the budget
// consumed is the observable proof — an implementation that re-reads the file from byte 0
// every tick still returns the right refs but burns the budget, and fails here.
#[test]
fn scanner_reads_only_the_appended_bytes() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    let filler = line(&"x".repeat(4000));
    append(&path, &filler);
    append(&path, &gh_pr_create_call("call_1088"));
    let cold_len = std::fs::metadata(&path).expect("stat").len();

    let mut scanner = PrScanner::new();

    let mut budget = SCAN_BUDGET_BYTES;
    let first = scanner.refs_for(&path, &mut budget);
    assert!(first.is_empty(), "the call has no result yet");
    assert_eq!(SCAN_BUDGET_BYTES - budget, cold_len, "cold pass reads all");

    // Unchanged file: no read at all, same answer.
    let mut budget = SCAN_BUDGET_BYTES;
    assert_eq!(hrefs(&scanner.refs_for(&path, &mut budget)), hrefs(&first));
    assert_eq!(budget, SCAN_BUDGET_BYTES, "unchanged file must not re-read");

    // Grown file: the pending call result and a new create both arrive in the appended bytes.
    let delta = format!(
        "{}{}{}",
        command_output(
            "call_1088",
            0,
            "https://github.com/example-org/example-repo/pull/1088",
        ),
        gh_pr_create_call("call_1089"),
        command_output(
            "call_1089",
            0,
            "https://github.com/example-org/example-repo/pull/1089",
        )
    );
    append(&path, &delta);
    let mut budget = SCAN_BUDGET_BYTES;
    let grown = scanner.refs_for(&path, &mut budget);
    assert_eq!(
        SCAN_BUDGET_BYTES - budget,
        delta.len() as u64,
        "hot pass reads only the delta"
    );
    assert_eq!(
        hrefs(&grown),
        vec![
            "https://github.com/example-org/example-repo/pull/1088".to_string(),
            "https://github.com/example-org/example-repo/pull/1089".to_string(),
        ]
    );
}

// A rollout is appended to live, so a scan can land mid-line. Parsing the partial tail would
// mint a WRONG PR number (`.../pull/10` for an in-flight `1089`) and, because refs are
// sticky, that wrong badge would never heal. Complete lines only.
#[test]
fn scanner_ignores_a_partial_line_until_its_newline_lands() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    // A complete line FOLLOWED by a half-written one, which is what a live rollout looks like
    // mid-turn: the scan must consume the complete line and stop at the newline.
    append(&path, &line("thinking about it"));
    append(&path, &gh_pr_create_call("call_partial"));
    let full = command_output(
        "call_partial",
        0,
        "https://github.com/example-org/example-repo/pull/1089",
    );
    let (head, tail) = full.split_at(full.find("1089").expect("number in line") + 2);
    append(&path, head);

    let mut scanner = PrScanner::new();
    let mut budget = SCAN_BUDGET_BYTES;
    assert!(
        scanner.refs_for(&path, &mut budget).is_empty(),
        "a half-written line must not produce a truncated PR ref"
    );

    append(&path, tail);
    let mut budget = SCAN_BUDGET_BYTES;
    assert_eq!(
        hrefs(&scanner.refs_for(&path, &mut budget)),
        vec!["https://github.com/example-org/example-repo/pull/1089".to_string()],
        "the completed line resolves to the real PR, and no truncated ref lingers"
    );
}

// One real rollout (a batch review session) mentions 115 distinct PRs. Every ref costs a
// live `gh` subprocess in the TUI's status cache, so the per-session list is capped — and
// the cap keeps the MOST RECENT mentions, because the PR a session opened is written near
// the end of its transcript, after whatever PRs it read along the way.
#[test]
fn scanner_caps_refs_keeping_the_most_recent() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    let last = MAX_REFS_PER_SESSION + 5;
    for n in 1..=last {
        let call_id = format!("call_{n}");
        append(&path, &gh_pr_create_call(&call_id));
        append(
            &path,
            &command_output(&call_id, 0, &format!("https://github.com/o/r/pull/{n}")),
        );
    }

    let mut scanner = PrScanner::new();
    let mut budget = SCAN_BUDGET_BYTES;
    let refs = scanner.refs_for(&path, &mut budget);
    let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
    let expected: Vec<String> = ((last - MAX_REFS_PER_SESSION + 1)..=last)
        .map(|n| n.to_string())
        .collect();
    assert_eq!(ids, expected.iter().map(String::as_str).collect::<Vec<_>>());
}

// The budget is what keeps a first listing from reading every rollout on the box at once
// (417 MB here). An exhausted budget defers a cold file to a later tick instead of dropping
// it: the row badges a second later, never never.
#[test]
fn scanner_defers_a_cold_file_when_the_budget_is_spent() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    append(&path, &gh_pr_create_call("call_deferred"));
    append(
        &path,
        &command_output(
            "call_deferred",
            0,
            "https://github.com/example-org/example-repo/pull/1089",
        ),
    );

    let mut scanner = PrScanner::new();
    let mut spent = 0;
    assert!(
        scanner.refs_for(&path, &mut spent).is_empty(),
        "no budget means no scan"
    );

    let mut budget = SCAN_BUDGET_BYTES;
    assert_eq!(scanner.refs_for(&path, &mut budget).len(), 1);
}

// A rollout that shrinks is a different file at the same path (rotation, or a rewrite).
// Keeping the old offset would skip everything before it forever, so a shorter file rescans
// from zero and the stale refs go with it.
#[test]
fn scanner_rescans_after_the_file_shrinks() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("rollout.jsonl");
    append(&path, &line(&"x".repeat(500)));
    append(&path, &gh_pr_create_call("call_one"));
    append(
        &path,
        &command_output("call_one", 0, "https://github.com/o/r/pull/1"),
    );

    let mut scanner = PrScanner::new();
    let mut budget = SCAN_BUDGET_BYTES;
    assert_eq!(scanner.refs_for(&path, &mut budget).len(), 1);

    std::fs::write(
        &path,
        format!(
            "{}{}",
            gh_pr_create_call("call_two"),
            command_output("call_two", 0, "https://github.com/o/r/pull/2"),
        ),
    )
    .expect("rewrite rollout");
    let mut budget = SCAN_BUDGET_BYTES;
    assert_eq!(
        hrefs(&scanner.refs_for(&path, &mut budget)),
        vec!["https://github.com/o/r/pull/2".to_string()],
        "a shrunken file rescans from zero and drops the old refs"
    );
}

// Most registry rows point at a rollout that has been pruned. That is the common case, not
// an error, and it must not cost budget.
#[test]
fn scanner_is_empty_for_a_missing_rollout() {
    let mut scanner = PrScanner::new();
    let mut budget = SCAN_BUDGET_BYTES;
    assert!(
        scanner
            .refs_for(Path::new("/nonexistent/rollout.jsonl"), &mut budget)
            .is_empty()
    );
    assert_eq!(budget, SCAN_BUDGET_BYTES);
}

// --- end to end through the backend ---

const INSERT_COLS: &str = "INSERT INTO threads \
    (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, \
     sandbox_policy, approval_mode, archived, model, git_branch, first_user_message, \
     preview, created_at_ms, updated_at_ms) VALUES ";

// The bug this whole module exists for: a codex thread that opened a PR showed no badge,
// because `list` hardcoded `pr_refs: Vec::new()`. Reverting to that hardcode fails here.
#[test]
fn list_populates_pr_refs_from_the_rollout() {
    let schema = common::read_fixture("threads_schema.sql");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let with_pr = dir.path().join("with_pr.jsonl");
    append(&with_pr, &gh_pr_create_call("call_backend"));
    append(
        &with_pr,
        &command_output(
            "call_backend",
            0,
            "https://github.com/example-org/example-repo/pull/1089",
        ),
    );
    let without_pr = dir.path().join("without_pr.jsonl");
    append(&without_pr, &line("no PR here"));

    let row = |id: &str, rollout: &std::path::Path| {
        format!(
            "{INSERT_COLS}('{id}','{}',1,1,'cli','openai','/home/user/proj','{id} Title',\
             'workspace-write','on-request',0,'gpt-5','main','first msg','preview',1000,1000)",
            rollout.display()
        )
    };
    let rows = [row("t_pr", &with_pr), row("t_none", &without_pr)];
    let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    let (db_dir, db_path) = common::temp_db(&schema, &refs);
    std::fs::rename(&db_path, db_dir.path().join("state_1.sqlite")).expect("rename temp db");

    let mut backend = CodexBackend::new(db_dir.path().to_path_buf());
    let sessions = backend.list().expect("list codex sessions");
    let find = |id: &str| {
        sessions
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no session {id}"))
    };

    assert_eq!(
        hrefs(&find("t_pr").pr_refs),
        vec!["https://github.com/example-org/example-repo/pull/1089".to_string()]
    );
    assert!(find("t_none").pr_refs.is_empty());
}
