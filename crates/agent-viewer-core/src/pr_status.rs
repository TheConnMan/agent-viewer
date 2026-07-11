//! PR badge status: parse a GitHub PR href, fetch its live state via `gh`, and map that
//! to a badge color. The pure mapping and URL parse are unit-tested; `fetch_pr_status`
//! shells out and degrades to `Default` on any failure.

/// The color bucket for a PR badge, mapped from live GitHub state. `Default` keeps the
/// current flat badge (unknown / unresolved / fetch failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrBadgeColor {
    Default,
    Attention, // checks pending OR failing OR review requested
    Passed,    // checks passed
    Merged,    // merged
    Muted,     // draft OR closed
}

/// Reduced state of a PR's status-check rollup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    None,
    Pending,
    Failing,
    Passing,
}

/// The GitHub facts `pr_color` maps from (parsed from `gh pr view --json ...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatusFacts {
    pub state: String,                   // "OPEN" | "MERGED" | "CLOSED" (any case)
    pub merged_at: Option<String>,       // gh `mergedAt` (RFC3339) or None
    pub is_draft: bool,
    pub review_decision: Option<String>, // "REVIEW_REQUIRED" | "APPROVED" | "CHANGES_REQUESTED" | None
    pub review_requested: bool,          // reviewRequests array nonempty (a reviewer is awaiting)
    pub checks: CheckState,
}

/// Parse a GitHub PR href into (owner, repo, number). None for non-github hosts,
/// non-pull URLs, or anything unparseable. Tolerates trailing path/query/fragment.
pub fn parse_pr_url(href: &str) -> Option<(String, String, u64)> {
    // Strip scheme (http/https only).
    let rest = href
        .strip_prefix("https://")
        .or_else(|| href.strip_prefix("http://"))?;
    // Split host from path at the first '/'.
    let (host, path) = rest.split_once('/')?;
    if host != "github.com" {
        return None;
    }
    // Drop any ?query or #fragment before splitting the path.
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut segs = path.split('/');
    let owner = segs.next().unwrap_or("");
    let repo = segs.next().unwrap_or("");
    let pull = segs.next().unwrap_or("");
    let number = segs.next().unwrap_or("");
    if owner.is_empty() || repo.is_empty() || pull != "pull" || number.is_empty() {
        return None;
    }
    let number: u64 = number.parse().ok()?;
    Some((owner.to_string(), repo.to_string(), number))
}

/// Reduce a `statusCheckRollup` JSON array (from gh) to a CheckState. Empty -> None.
pub fn rollup_check_state(rollup: &[serde_json::Value]) -> CheckState {
    if rollup.is_empty() {
        return CheckState::None;
    }
    let mut any_fail = false;
    let mut any_pending = false;
    for el in rollup {
        let typename = crate::json_str(el, "__typename").unwrap_or("");
        let is_pass_pending_fail = if typename == "CheckRun" {
            let status = crate::json_str(el, "status").unwrap_or("").to_uppercase();
            if status != "COMPLETED" {
                Bucket::Pending
            } else {
                let conclusion = crate::json_str(el, "conclusion").unwrap_or("").to_uppercase();
                match conclusion.as_str() {
                    "SUCCESS" | "NEUTRAL" | "SKIPPED" => Bucket::Pass,
                    _ => Bucket::Fail, // FAILURE, CANCELLED, etc. and unknown -> conservative fail
                }
            }
        } else {
            // StatusContext (or anything else / missing typename): read `state`.
            let state = crate::json_str(el, "state").unwrap_or("").to_uppercase();
            match state.as_str() {
                "SUCCESS" => Bucket::Pass,
                "PENDING" | "EXPECTED" => Bucket::Pending,
                _ => Bucket::Fail, // FAILURE, ERROR, and unknown -> fail
            }
        };
        match is_pass_pending_fail {
            Bucket::Fail => any_fail = true,
            Bucket::Pending => any_pending = true,
            Bucket::Pass => {}
        }
    }
    if any_fail {
        CheckState::Failing
    } else if any_pending {
        CheckState::Pending
    } else {
        CheckState::Passing
    }
}

/// Per-element category during rollup reduction.
enum Bucket {
    Pass,
    Pending,
    Fail,
}

/// Pure mapping from PR facts to a badge color.
pub fn pr_color(facts: &PrStatusFacts) -> PrBadgeColor {
    let state = facts.state.to_uppercase();
    if state == "MERGED" || facts.merged_at.is_some() {
        return PrBadgeColor::Merged;
    }
    if state == "CLOSED" || facts.is_draft {
        return PrBadgeColor::Muted;
    }
    if facts.checks == CheckState::Failing || facts.checks == CheckState::Pending {
        return PrBadgeColor::Attention;
    }
    if facts.review_decision.as_deref() == Some("REVIEW_REQUIRED") || facts.review_requested {
        return PrBadgeColor::Attention;
    }
    if facts.checks == CheckState::Passing {
        return PrBadgeColor::Passed;
    }
    PrBadgeColor::Default
}

/// Rank a color for aggregation: higher wins. Attention > Passed > Merged > Muted > Default.
fn color_rank(c: PrBadgeColor) -> u8 {
    match c {
        PrBadgeColor::Attention => 4,
        PrBadgeColor::Passed => 3,
        PrBadgeColor::Merged => 2,
        PrBadgeColor::Muted => 1,
        PrBadgeColor::Default => 0,
    }
}

/// Combine per-PR colors for a multi-PR session badge. Empty -> Default.
pub fn aggregate_color(colors: &[PrBadgeColor]) -> PrBadgeColor {
    colors
        .iter()
        .copied()
        .max_by_key(|c| color_rank(*c))
        .unwrap_or(PrBadgeColor::Default)
}

/// Parse the JSON emitted by `gh pr view --json ...` into PrStatusFacts. None only when the
/// payload does not parse as JSON at all; missing fields degrade to sane defaults.
fn parse_pr_json(stdout: &str) -> Option<PrStatusFacts> {
    let v: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let state = crate::json_str(&v, "state").unwrap_or("").to_string();
    let merged_at = crate::json_str(&v, "mergedAt").map(String::from);
    let is_draft = v.get("isDraft").and_then(|b| b.as_bool()).unwrap_or(false);
    let review_decision = crate::json_str(&v, "reviewDecision")
        .filter(|s| !s.is_empty())
        .map(String::from);
    let review_requested = v
        .get("reviewRequests")
        .and_then(|r| r.as_array())
        .is_some_and(|a| !a.is_empty());
    let empty: Vec<serde_json::Value> = Vec::new();
    let rollup = v
        .get("statusCheckRollup")
        .and_then(|r| r.as_array())
        .unwrap_or(&empty);
    let checks = rollup_check_state(rollup);
    Some(PrStatusFacts {
        state,
        merged_at,
        is_draft,
        review_decision,
        review_requested,
        checks,
    })
}

/// Shell `gh pr view <url> --json ...` for owner/repo/number, parse, and map to a color.
/// Returns `PrBadgeColor::Default` on ANY failure. Never errors.
pub fn fetch_pr_status(owner: &str, repo: &str, number: u64) -> PrBadgeColor {
    // Pass the full URL so gh resolves the repo independent of cwd (this repo has no remote).
    let url = format!("https://github.com/{owner}/{repo}/pull/{number}");
    let output = std::process::Command::new("gh")
        .args([
            "pr",
            "view",
            &url,
            "--json",
            "state,mergedAt,isDraft,reviewDecision,reviewRequests,statusCheckRollup",
        ])
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return PrBadgeColor::Default,
    };
    if output.stdout.is_empty() {
        return PrBadgeColor::Default;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_pr_json(&stdout) {
        Some(facts) => pr_color(&facts),
        None => PrBadgeColor::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A no-required-review repo returns reviewDecision="" but a nonempty reviewRequests
    // array. Parse must derive review_requested=true so pr_color reports Attention.
    #[test]
    fn parse_full_payload_review_requested_is_attention() {
        let json = r#"{"state":"OPEN","mergedAt":null,"isDraft":false,"reviewDecision":"","reviewRequests":[{"login":"alice"},{"login":"bob"}],"statusCheckRollup":[{"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"}]}"#;
        let facts = parse_pr_json(json).expect("payload parses");
        assert_eq!(facts.state, "OPEN");
        assert_eq!(facts.merged_at, None);
        assert!(!facts.is_draft);
        assert_eq!(facts.review_decision, None);
        assert!(facts.review_requested);
        assert_eq!(facts.checks, CheckState::Passing);
        assert_eq!(pr_color(&facts), PrBadgeColor::Attention);
    }

    // A merged PR: mergedAt set, no reviewers awaiting -> Merged.
    #[test]
    fn parse_merged_payload_is_merged() {
        let json = r#"{"state":"MERGED","mergedAt":"2026-07-11T00:00:00Z","isDraft":false,"reviewDecision":null,"reviewRequests":[],"statusCheckRollup":[]}"#;
        let facts = parse_pr_json(json).expect("payload parses");
        assert!(facts.merged_at.is_some());
        assert!(!facts.review_requested);
        assert_eq!(pr_color(&facts), PrBadgeColor::Merged);
    }

    // Non-JSON input yields None rather than panicking.
    #[test]
    fn parse_malformed_payload_is_none() {
        assert!(parse_pr_json("").is_none());
        assert!(parse_pr_json("not json").is_none());
    }
}
