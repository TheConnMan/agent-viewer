//! Contract tests for the pure PR-badge status logic in `agent_viewer_core::pr_status`.
//! These encode the REQUIRED behavior and are expected to FAIL against the current
//! placeholder bodies (parse returns None, colors return Default). The next pass
//! implements the logic to make them pass.

use agent_viewer_core::PrBadgeColor;
use agent_viewer_core::pr_status::{
    CheckState, PrStatusFacts, aggregate_color, parse_pr_url, pr_color, rollup_check_state,
};

// Build PrStatusFacts with sane defaults (open, no CI, not draft, no review), so each
// test overrides only the fields it exercises.
fn facts() -> PrStatusFacts {
    PrStatusFacts {
        state: "OPEN".to_string(),
        merged_at: None,
        is_draft: false,
        review_decision: None,
        review_requested: false,
        checks: CheckState::None,
    }
}

// ===========================================================================
// A) parse_pr_url
// ===========================================================================

#[test]
fn parse_pr_url_basic_https() {
    assert_eq!(
        parse_pr_url("https://github.com/acme/repo/pull/315"),
        Some(("acme".to_string(), "repo".to_string(), 315))
    );
}

#[test]
fn parse_pr_url_http_allowed() {
    assert_eq!(
        parse_pr_url("http://github.com/acme/repo/pull/315"),
        Some(("acme".to_string(), "repo".to_string(), 315))
    );
}

#[test]
fn parse_pr_url_trailing_path_files() {
    assert_eq!(
        parse_pr_url("https://github.com/acme/repo/pull/315/files"),
        Some(("acme".to_string(), "repo".to_string(), 315))
    );
}

#[test]
fn parse_pr_url_trailing_fragment() {
    assert_eq!(
        parse_pr_url("https://github.com/acme/repo/pull/315#issuecomment-9"),
        Some(("acme".to_string(), "repo".to_string(), 315))
    );
}

#[test]
fn parse_pr_url_trailing_query() {
    assert_eq!(
        parse_pr_url("https://github.com/acme/repo/pull/315?w=1"),
        Some(("acme".to_string(), "repo".to_string(), 315))
    );
}

#[test]
fn parse_pr_url_dots_and_dashes_in_names() {
    assert_eq!(
        parse_pr_url("https://github.com/octo-org/my.repo/pull/7"),
        Some(("octo-org".to_string(), "my.repo".to_string(), 7))
    );
}

#[test]
fn parse_pr_url_issues_is_not_pull() {
    assert_eq!(
        parse_pr_url("https://github.com/acme/repo/issues/315"),
        None
    );
}

#[test]
fn parse_pr_url_non_github_host() {
    assert_eq!(parse_pr_url("https://gitlab.com/acme/repo/pull/315"), None);
}

#[test]
fn parse_pr_url_enterprise_host_rejected() {
    assert_eq!(
        parse_pr_url("https://github.example.com/acme/repo/pull/315"),
        None
    );
}

#[test]
fn parse_pr_url_non_numeric_number() {
    assert_eq!(parse_pr_url("https://github.com/acme/repo/pull/abc"), None);
}

#[test]
fn parse_pr_url_missing_number() {
    assert_eq!(parse_pr_url("https://github.com/acme/repo/pull/"), None);
}

#[test]
fn parse_pr_url_no_pull_segment() {
    assert_eq!(parse_pr_url("https://github.com/acme/repo"), None);
}

#[test]
fn parse_pr_url_empty_string() {
    assert_eq!(parse_pr_url(""), None);
}

#[test]
fn parse_pr_url_not_a_url() {
    assert_eq!(parse_pr_url("not a url"), None);
}

// ===========================================================================
// B) pr_color — precedence: merged > closed > draft > checks(fail/pending) >
//    review_required > checks(passing) > default
// ===========================================================================

#[test]
fn pr_color_merged_state_wins_over_passing() {
    let f = PrStatusFacts {
        state: "MERGED".to_string(),
        checks: CheckState::Passing,
        is_draft: false,
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Merged);
}

#[test]
fn pr_color_merged_at_forces_merged_even_when_open() {
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        merged_at: Some("2026-07-11T00:00:00Z".to_string()),
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Merged);
}

#[test]
fn pr_color_closed_is_muted() {
    let f = PrStatusFacts {
        state: "CLOSED".to_string(),
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Muted);
}

#[test]
fn pr_color_draft_beats_passing() {
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        is_draft: true,
        checks: CheckState::Passing,
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Muted);
}

#[test]
fn pr_color_open_failing_is_attention() {
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        checks: CheckState::Failing,
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Attention);
}

#[test]
fn pr_color_open_pending_is_attention() {
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        checks: CheckState::Pending,
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Attention);
}

#[test]
fn pr_color_review_required_beats_passing() {
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        checks: CheckState::Passing,
        review_decision: Some("REVIEW_REQUIRED".to_string()),
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Attention);
}

#[test]
fn pr_color_review_requested_beats_passing() {
    // No required-review rule (review_decision None) but a reviewer is awaiting: Attention.
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        checks: CheckState::Passing,
        review_decision: None,
        review_requested: true,
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Attention);
}

#[test]
fn pr_color_review_requested_no_ci_is_attention() {
    // No-CI repo with a pending reviewer: Attention even without any checks.
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        checks: CheckState::None,
        review_decision: None,
        review_requested: true,
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Attention);
}

#[test]
fn pr_color_approved_and_passing_is_passed() {
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        checks: CheckState::Passing,
        review_decision: Some("APPROVED".to_string()),
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Passed);
}

#[test]
fn pr_color_passing_no_review_is_passed() {
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        checks: CheckState::Passing,
        review_decision: None,
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Passed);
}

#[test]
fn pr_color_open_no_ci_no_review_is_default() {
    let f = PrStatusFacts {
        state: "OPEN".to_string(),
        checks: CheckState::None,
        review_decision: None,
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Default);
}

#[test]
fn pr_color_merged_beats_draft() {
    let f = PrStatusFacts {
        state: "MERGED".to_string(),
        is_draft: true,
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Merged);
}

#[test]
fn pr_color_state_lowercase_merged() {
    let f = PrStatusFacts {
        state: "merged".to_string(),
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Merged);
}

#[test]
fn pr_color_state_lowercase_closed() {
    let f = PrStatusFacts {
        state: "closed".to_string(),
        ..facts()
    };
    assert_eq!(pr_color(&f), PrBadgeColor::Muted);
}

// ===========================================================================
// C) rollup_check_state — reduce a gh statusCheckRollup array
// ===========================================================================

#[test]
fn rollup_empty_is_none() {
    let arr = serde_json::json!([]);
    assert_eq!(
        rollup_check_state(arr.as_array().unwrap()),
        CheckState::None
    );
}

#[test]
fn rollup_all_checkrun_success_is_passing() {
    let arr = serde_json::json!([
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
    ]);
    assert_eq!(
        rollup_check_state(arr.as_array().unwrap()),
        CheckState::Passing
    );
}

#[test]
fn rollup_one_failure_among_successes_is_failing() {
    let arr = serde_json::json!([
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"},
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
    ]);
    assert_eq!(
        rollup_check_state(arr.as_array().unwrap()),
        CheckState::Failing
    );
}

#[test]
fn rollup_one_in_progress_among_successes_is_pending() {
    let arr = serde_json::json!([
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": serde_json::Value::Null},
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
    ]);
    assert_eq!(
        rollup_check_state(arr.as_array().unwrap()),
        CheckState::Pending
    );
}

#[test]
fn rollup_failing_dominates_pending() {
    let arr = serde_json::json!([
        {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": serde_json::Value::Null},
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"},
    ]);
    assert_eq!(
        rollup_check_state(arr.as_array().unwrap()),
        CheckState::Failing
    );
}

#[test]
fn rollup_statuscontext_success_plus_checkrun_success_is_passing() {
    let arr = serde_json::json!([
        {"__typename": "StatusContext", "state": "SUCCESS"},
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
    ]);
    assert_eq!(
        rollup_check_state(arr.as_array().unwrap()),
        CheckState::Passing
    );
}

#[test]
fn rollup_statuscontext_pending_is_pending() {
    let arr = serde_json::json!([
        {"__typename": "StatusContext", "state": "PENDING"},
    ]);
    assert_eq!(
        rollup_check_state(arr.as_array().unwrap()),
        CheckState::Pending
    );
}

#[test]
fn rollup_statuscontext_failure_is_failing() {
    let arr = serde_json::json!([
        {"__typename": "StatusContext", "state": "FAILURE"},
    ]);
    assert_eq!(
        rollup_check_state(arr.as_array().unwrap()),
        CheckState::Failing
    );
}

#[test]
fn rollup_neutral_and_skipped_are_passing() {
    let arr = serde_json::json!([
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "NEUTRAL"},
        {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SKIPPED"},
    ]);
    assert_eq!(
        rollup_check_state(arr.as_array().unwrap()),
        CheckState::Passing
    );
}

// ===========================================================================
// D) aggregate_color — priority: Attention > Passed > Merged > Muted > Default
// ===========================================================================

#[test]
fn aggregate_empty_is_default() {
    assert_eq!(aggregate_color(&[]), PrBadgeColor::Default);
}

#[test]
fn aggregate_passed_and_attention_is_attention() {
    assert_eq!(
        aggregate_color(&[PrBadgeColor::Passed, PrBadgeColor::Attention]),
        PrBadgeColor::Attention
    );
}

#[test]
fn aggregate_merged_and_passed_is_passed() {
    assert_eq!(
        aggregate_color(&[PrBadgeColor::Merged, PrBadgeColor::Passed]),
        PrBadgeColor::Passed
    );
}

#[test]
fn aggregate_muted_and_merged_is_merged() {
    assert_eq!(
        aggregate_color(&[PrBadgeColor::Muted, PrBadgeColor::Merged]),
        PrBadgeColor::Merged
    );
}

#[test]
fn aggregate_default_and_muted_is_muted() {
    assert_eq!(
        aggregate_color(&[PrBadgeColor::Default, PrBadgeColor::Muted]),
        PrBadgeColor::Muted
    );
}

#[test]
fn aggregate_single_merged_passthrough() {
    assert_eq!(
        aggregate_color(&[PrBadgeColor::Merged]),
        PrBadgeColor::Merged
    );
}
