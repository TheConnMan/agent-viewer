//! The `agent-router` shell-out behind the composer's Auto entry: PATH gating, decision
//! parsing, and the failure path.
//!
//! `router_run_dry_run.json` was captured live from
//! `agent-router run --dry-run --json --dir /tmp "<task>"`, so the field shapes here are the
//! CLI's real output, not an assumed schema.

mod common;

use agent_viewer_core::BackendKind;
use agent_viewer_core::router::{ROUTER_BIN, RouterOutcome, find_on_path, parse_outcome};
use common::read_fixture;

/// The dispatched shape: the same live output with the `dispatch` object the router writes
/// once it has actually started the job (`{job_id, job_name}`).
fn dispatched_fixture() -> String {
    let dry_run = read_fixture("router_run_dry_run.json");
    assert!(
        dry_run.contains("\"dispatch\": null"),
        "the captured dry run must carry a null dispatch"
    );
    dry_run.replace(
        "\"dispatch\": null",
        "\"dispatch\": {\n    \"job_id\": \"0199c0de-thread\",\n    \"job_name\": \"Add a unit test for the trunc\"\n  }",
    )
}

#[test]
fn a_dry_run_decision_parses_provider_effort_and_weekly_headroom() {
    let outcome = parse_outcome(&read_fixture("router_run_dry_run.json")).expect("parsed decision");

    assert_eq!(outcome.provider, BackendKind::Codex);
    // Codex leaves the model to codex itself and routes at the highest reasoning effort.
    assert_eq!(outcome.model, None);
    assert_eq!(outcome.effort.as_deref(), Some("xhigh"));
    assert_eq!(outcome.gates, Vec::<String>::new());
    assert_eq!(outcome.codex_weekly_pct, 87.0);
    assert_eq!(outcome.claude_weekly_pct, 52.0);
    assert!(
        outcome.rationale.contains("codex_ready 6/6"),
        "the rationale must survive parsing, got {:?}",
        outcome.rationale
    );
    // A dry run dispatched nothing, so there is no job to select a row for.
    assert_eq!(outcome.job_id, None);
    assert_eq!(outcome.job_name, None);
}

#[test]
fn a_dispatched_decision_carries_the_job_identity_the_viewer_selects_by() {
    let outcome = parse_outcome(&dispatched_fixture()).expect("parsed decision");

    assert_eq!(outcome.job_id.as_deref(), Some("0199c0de-thread"));
    assert_eq!(
        outcome.job_name.as_deref(),
        Some("Add a unit test for the trunc")
    );
}

#[test]
fn the_footer_notice_is_one_line_naming_the_provider_and_both_headrooms() {
    let outcome = parse_outcome(&read_fixture("router_run_dry_run.json")).expect("parsed decision");

    assert_eq!(
        outcome.notice(),
        "auto: codex effort xhigh (codex weekly 87%, claude 52%)"
    );

    let dispatched = parse_outcome(&dispatched_fixture()).expect("parsed decision");
    let notice = dispatched.notice();
    assert_eq!(
        notice,
        "auto: codex effort xhigh job 0199c0de-thread (codex weekly 87%, claude 52%)"
    );
    assert!(!notice.contains('\n'), "the footer is one line: {notice:?}");
}

#[test]
fn gate_tags_reach_the_notice_so_a_flipped_decision_says_why() {
    let flipped = read_fixture("router_run_dry_run.json")
        .replace("\"gates\": []", "\"gates\": [\"headroom_tiebreak\"]");
    let outcome = parse_outcome(&flipped).expect("parsed decision");

    assert_eq!(outcome.gates, vec!["headroom_tiebreak".to_string()]);
    assert!(
        outcome.notice().contains("gates[headroom_tiebreak]"),
        "got {:?}",
        outcome.notice()
    );
}

/// Every unreadable decision is an error the user sees, never a default that would spawn on a
/// guessed provider (and never a panic on the mutation worker).
#[test]
fn an_unreadable_decision_is_an_error_not_a_panic_and_not_a_fallback() {
    let truncated = parse_outcome("{\"provider\": \"codex\"").unwrap_err();
    assert!(
        truncated.contains(ROUTER_BIN) && truncated.contains("unreadable json"),
        "got {truncated:?}"
    );

    let not_json = parse_outcome("agent-router: config is broken").unwrap_err();
    assert!(not_json.contains("unreadable json"), "got {not_json:?}");

    let no_provider = parse_outcome("{\"dispatch\": null}").unwrap_err();
    assert!(no_provider.contains("no provider"), "got {no_provider:?}");

    let unknown = parse_outcome("{\"provider\": \"gemini\"}").unwrap_err();
    assert!(
        unknown.contains("unknown provider") && unknown.contains("gemini"),
        "got {unknown:?}"
    );
}

/// The Auto entry is gated on the binary being discoverable, tested against a controlled PATH
/// value rather than the real environment (where the router happens to be installed).
#[test]
fn the_auto_gate_is_off_when_the_router_binary_is_not_on_the_path() {
    let empty = tempfile::tempdir().expect("temp dir");
    let installed = tempfile::tempdir().expect("temp dir");
    std::fs::write(installed.path().join(ROUTER_BIN), b"#!/bin/sh\n").expect("write router");

    assert_eq!(
        find_on_path(ROUTER_BIN, Some(empty.path().as_os_str())),
        None,
        "a PATH without the router must not offer Auto"
    );
    assert_eq!(
        find_on_path(ROUTER_BIN, None),
        None,
        "no PATH at all must not offer Auto"
    );

    let path = std::env::join_paths([empty.path(), installed.path()]).expect("join paths");
    assert_eq!(
        find_on_path(ROUTER_BIN, Some(path.as_os_str())),
        Some(installed.path().join(ROUTER_BIN)),
        "the router must be found in the second PATH entry"
    );
}

/// The Auto model entry exists so the picker has something to show; the router owns the real
/// model and effort choice, so a routed decision's own model must survive untouched.
#[test]
fn the_router_model_is_reported_when_the_router_chose_one() {
    let claude = read_fixture("router_run_dry_run.json")
        .replace("\"provider\": \"codex\"", "\"provider\": \"claude\"")
        .replace("\"model\": null", "\"model\": \"opus[1m]\"")
        .replace("\"effort\": \"xhigh\"", "\"effort\": null");
    let outcome: RouterOutcome = parse_outcome(&claude).expect("parsed decision");

    assert_eq!(outcome.provider, BackendKind::Claude);
    assert_eq!(outcome.model.as_deref(), Some("opus[1m]"));
    assert_eq!(outcome.effort, None);
    assert_eq!(
        outcome.notice(),
        "auto: claude opus[1m] (codex weekly 87%, claude 52%)"
    );
}
