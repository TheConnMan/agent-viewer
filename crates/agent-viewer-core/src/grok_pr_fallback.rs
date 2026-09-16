use crate::backend::{BackendKind, PrRef, Session};
use crate::error::Result;
use crate::grok::{GROK_MAX_PR_REFS, push_capped_pr_ref};
use crate::state::{
    GROK_PR_SUCCESS_FRESHNESS_MS, GrokRepositoryPrCacheRow, GrokWorktreeIdentityCacheRow, ViewerDb,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const IDENTITY_TIMEOUT: Duration = Duration::from_secs(2);
const REPOSITORY_TIMEOUT: Duration = Duration::from_secs(10);
const PRUNE_INTERVAL_MS: i64 = 60_000;

#[derive(Clone, Copy)]
enum RefreshKind {
    Identity,
    Repository,
}

pub struct GrokPrFallback {
    next_kind: RefreshKind,
    last_prune_ms: Option<i64>,
}

impl Default for GrokPrFallback {
    fn default() -> Self {
        Self {
            next_kind: RefreshKind::Identity,
            last_prune_ms: None,
        }
    }
}

#[derive(Clone)]
struct ValidIdentity {
    root: String,
    branch: Option<String>,
    common_dir: String,
}

struct CacheView {
    identity_rows: HashMap<String, GrokWorktreeIdentityCacheRow>,
    identities: HashMap<String, ValidIdentity>,
    repository_rows: HashMap<String, GrokRepositoryPrCacheRow>,
    inventories: HashMap<String, BTreeMap<String, Vec<PrRef>>>,
}

struct IdentityOutput {
    root: String,
    branch: Option<String>,
    common_dir: String,
}

impl GrokPrFallback {
    pub fn attach_branch_pr_refs(&mut self, db: &ViewerDb, sessions: &mut [Session]) -> Result<()> {
        let mut clock = crate::spawn::now_ms;
        let mut runner = |command, timeout| crate::spawn::run_with_timeout(command, timeout);
        self.attach_branch_pr_refs_with_runner(db, sessions, &mut clock, &mut runner)
    }

    #[doc(hidden)]
    pub fn attach_branch_pr_refs_with_runner(
        &mut self,
        db: &ViewerDb,
        sessions: &mut [Session],
        clock: &mut dyn FnMut() -> i64,
        runner: &mut dyn FnMut(Command, Duration) -> Option<String>,
    ) -> Result<()> {
        let eligible = eligible_rows(sessions);
        if eligible.is_empty() {
            return Ok(());
        }
        let cwds = eligible
            .iter()
            .map(|(_, cwd)| cwd.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut cache = load_cache(db, &cwds)?;
        project_refs(sessions, &eligible, &cache);

        let claim_now_ms = clock();
        let mut maintenance_now_ms = claim_now_ms;
        let identity_due = identity_candidates(&cwds, &cache, claim_now_ms);
        let repository_roots = repository_roots(&cache);
        let repository_due = repository_candidates(&repository_roots, &cache, claim_now_ms);
        let selected = match (identity_due.first(), repository_due.first()) {
            (Some(_), Some(_)) => Some(self.next_kind),
            (Some(_), None) => Some(RefreshKind::Identity),
            (None, Some(_)) => Some(RefreshKind::Repository),
            (None, None) => None,
        };

        match selected {
            Some(RefreshKind::Identity) => {
                self.next_kind = RefreshKind::Repository;
                if let Some((_, cwd)) = identity_due.first()
                    && let Some(token) = db
                        .claim_grok_worktree_identity(cwd, claim_now_ms)
                        .unwrap_or_default()
                {
                    let mut command = Command::new("git");
                    command.arg("-C").arg(cwd).args([
                        "rev-parse",
                        "--path-format=absolute",
                        "--show-toplevel",
                        "--abbrev-ref",
                        "HEAD",
                        "--git-common-dir",
                    ]);
                    let output = runner(command, IDENTITY_TIMEOUT);
                    let completed_at_ms = clock();
                    maintenance_now_ms = completed_at_ms;
                    match output.and_then(|out| parse_identity(&out)) {
                        Some(identity) => {
                            let _ = db.complete_grok_worktree_identity(
                                &token,
                                &identity.root,
                                identity.branch.as_deref(),
                                &identity.common_dir,
                                completed_at_ms,
                            );
                        }
                        None => {
                            let _ = db.fail_grok_worktree_identity(&token, completed_at_ms);
                        }
                    }
                }
            }
            Some(RefreshKind::Repository) => {
                self.next_kind = RefreshKind::Identity;
                if let Some((_, common_dir)) = repository_due.first()
                    && let Some(root) = repository_roots.get(common_dir)
                    && let Some(token) = db
                        .claim_grok_repository_prs(common_dir, claim_now_ms)
                        .unwrap_or_default()
                {
                    let mut command = Command::new("gh");
                    command
                        .args([
                            "pr",
                            "list",
                            "--state",
                            "open",
                            "--json",
                            "number,url,isCrossRepository,headRefName",
                            "--limit",
                            "100",
                        ])
                        .current_dir(root);
                    let output = runner(command, REPOSITORY_TIMEOUT);
                    let completed_at_ms = clock();
                    maintenance_now_ms = completed_at_ms;
                    let parsed = output
                        .and_then(|out| parse_repository_inventory(&out))
                        .and_then(|inventory| serde_json::to_string(&inventory).ok());
                    match parsed {
                        Some(prs_json) => {
                            let _ =
                                db.complete_grok_repository_prs(&token, &prs_json, completed_at_ms);
                        }
                        None => {
                            let _ = db.fail_grok_repository_prs(&token, completed_at_ms);
                        }
                    }
                }
            }
            None => {}
        }

        if let Ok(reloaded) = load_cache(db, &cwds) {
            cache = reloaded;
            project_refs(sessions, &eligible, &cache);
        }
        if prune_is_due(self.last_prune_ms, maintenance_now_ms) {
            self.last_prune_ms = Some(maintenance_now_ms);
            let _ = db.prune_grok_pr_fallback(maintenance_now_ms);
        }
        Ok(())
    }
}

fn eligible_rows(sessions: &[Session]) -> Vec<(usize, String)> {
    sessions
        .iter()
        .enumerate()
        .filter_map(|(index, session)| {
            if session.backend != BackendKind::Grok || !session.pr_refs.is_empty() {
                return None;
            }
            let cwd = session.cwd.to_str()?;
            if cwd.is_empty()
                || !session.cwd.is_absolute()
                || !session.cwd.is_dir()
                || !safe_text(cwd)
            {
                return None;
            }
            Some((index, cwd.to_string()))
        })
        .collect()
}

fn load_cache(db: &ViewerDb, cwds: &[String]) -> Result<CacheView> {
    let identity_rows = db
        .grok_worktree_identities(cwds)?
        .into_iter()
        .map(|row| (row.cwd.clone(), row))
        .collect::<HashMap<_, _>>();
    let identities = identity_rows
        .values()
        .filter_map(valid_identity)
        .collect::<HashMap<_, _>>();
    let common_dirs = identities
        .values()
        .filter(|identity| identity.branch.is_some())
        .map(|identity| identity.common_dir.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let repository_rows = db
        .grok_repository_prs(&common_dirs)?
        .into_iter()
        .map(|row| (row.common_dir.clone(), row))
        .collect::<HashMap<_, _>>();
    let inventories = repository_rows
        .values()
        .filter_map(valid_stored_inventory)
        .collect::<HashMap<_, _>>();
    Ok(CacheView {
        identity_rows,
        identities,
        repository_rows,
        inventories,
    })
}

fn valid_identity(row: &GrokWorktreeIdentityCacheRow) -> Option<(String, ValidIdentity)> {
    if row.success_at_ms <= 0 {
        return None;
    }
    let root = row.root.as_deref()?;
    let common_dir = row.common_dir.as_deref()?;
    if !valid_absolute_path(root) || !valid_absolute_path(common_dir) {
        return None;
    }
    let branch = match row.branch.as_deref() {
        Some(branch) if !branch.is_empty() && safe_text(branch) && branch != "HEAD" => {
            Some(branch.to_string())
        }
        None => None,
        _ => return None,
    };
    Some((
        row.cwd.clone(),
        ValidIdentity {
            root: root.to_string(),
            branch,
            common_dir: common_dir.to_string(),
        },
    ))
}

fn valid_stored_inventory(
    row: &GrokRepositoryPrCacheRow,
) -> Option<(String, BTreeMap<String, Vec<PrRef>>)> {
    if row.success_at_ms <= 0 {
        return None;
    }
    let inventory =
        serde_json::from_str::<BTreeMap<String, Vec<PrRef>>>(row.prs_json.as_deref()?).ok()?;
    for (branch, refs) in &inventory {
        if branch.is_empty() || !safe_text(branch) || refs.len() > GROK_MAX_PR_REFS {
            return None;
        }
        let mut seen = HashSet::new();
        for reference in refs {
            let number = reference
                .id
                .parse::<u64>()
                .ok()
                .filter(|number| *number > 0)?;
            let href = reference.href.as_deref()?;
            if !canonical_pr_url(href, number).is_some_and(|_| seen.insert(href)) {
                return None;
            }
        }
    }
    Some((row.common_dir.clone(), inventory))
}

fn project_refs(sessions: &mut [Session], eligible: &[(usize, String)], cache: &CacheView) {
    for (index, cwd) in eligible {
        let refs = cache
            .identities
            .get(cwd)
            .and_then(|identity| {
                identity
                    .branch
                    .as_deref()
                    .map(|branch| (identity.common_dir.as_str(), branch))
            })
            .and_then(|(common_dir, branch)| cache.inventories.get(common_dir)?.get(branch))
            .cloned()
            .unwrap_or_default();
        sessions[*index].pr_refs = refs;
    }
}

fn identity_candidates(cwds: &[String], cache: &CacheView, now_ms: i64) -> Vec<(i64, String)> {
    let mut due = cwds
        .iter()
        .filter_map(|cwd| match cache.identity_rows.get(cwd) {
            Some(row)
                if row.busy_until_ms <= now_ms && !success_is_fresh(row.success_at_ms, now_ms) =>
            {
                Some((row.attempt_ms, cwd.clone()))
            }
            None => Some((0, cwd.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    due.sort();
    due
}

fn repository_roots(cache: &CacheView) -> BTreeMap<String, String> {
    let mut roots = BTreeMap::new();
    for identity in cache
        .identities
        .values()
        .filter(|identity| identity.branch.is_some())
    {
        roots
            .entry(identity.common_dir.clone())
            .and_modify(|root: &mut String| {
                if identity.root < *root {
                    *root = identity.root.clone();
                }
            })
            .or_insert_with(|| identity.root.clone());
    }
    roots
}

fn repository_candidates(
    roots: &BTreeMap<String, String>,
    cache: &CacheView,
    now_ms: i64,
) -> Vec<(i64, String)> {
    let mut due = roots
        .keys()
        .filter_map(|common_dir| match cache.repository_rows.get(common_dir) {
            Some(row)
                if row.busy_until_ms <= now_ms && !success_is_fresh(row.success_at_ms, now_ms) =>
            {
                Some((row.attempt_ms, common_dir.clone()))
            }
            None => Some((0, common_dir.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    due.sort();
    due
}

fn success_is_fresh(success_at_ms: i64, now_ms: i64) -> bool {
    success_at_ms > 0
        && (0..GROK_PR_SUCCESS_FRESHNESS_MS).contains(&now_ms.saturating_sub(success_at_ms))
}

fn prune_is_due(last_prune_ms: Option<i64>, now_ms: i64) -> bool {
    last_prune_ms.is_none_or(|last| !(0..PRUNE_INTERVAL_MS).contains(&now_ms.saturating_sub(last)))
}

fn parse_identity(output: &str) -> Option<IdentityOutput> {
    if output
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return None;
    }
    let lines = output.lines().collect::<Vec<_>>();
    if lines.len() != 3 {
        return None;
    }
    let root = lines[0];
    let branch = lines[1];
    let common_dir = lines[2];
    if !valid_absolute_path(root) || !valid_absolute_path(common_dir) || !safe_text(branch) {
        return None;
    }
    Some(IdentityOutput {
        root: root.to_string(),
        branch: (!branch.is_empty() && branch != "HEAD").then(|| branch.to_string()),
        common_dir: common_dir.to_string(),
    })
}

fn parse_repository_inventory(output: &str) -> Option<BTreeMap<String, Vec<PrRef>>> {
    let entries = serde_json::from_str::<Vec<serde_json::Value>>(output).ok()?;
    if entries.len() > 100 {
        return None;
    }
    let mut inventory = BTreeMap::<String, Vec<PrRef>>::new();
    for entry in entries {
        let object = entry.as_object()?;
        let cross_repository = object.get("isCrossRepository")?.as_bool()?;
        let number = object
            .get("number")?
            .as_u64()
            .filter(|number| *number > 0)?;
        let url = object.get("url")?.as_str()?;
        let head = object.get("headRefName")?.as_str()?;
        if head.is_empty() || !safe_text(head) {
            return None;
        }
        let (owner, repo) = canonical_pr_url(url, number)?;
        if cross_repository {
            continue;
        }
        push_capped_pr_ref(
            inventory.entry(head.to_string()).or_default(),
            owner,
            repo,
            number,
        );
    }
    Some(inventory)
}

fn canonical_pr_url(url: &str, expected_number: u64) -> Option<(String, String)> {
    if !safe_text(url) {
        return None;
    }
    let (owner, repo, number) = crate::pr_status::parse_pr_url(url)?;
    if number != expected_number
        || url != format!("https://github.com/{owner}/{repo}/pull/{number}")
    {
        return None;
    }
    Some((owner, repo))
}

fn valid_absolute_path(value: &str) -> bool {
    !value.is_empty() && safe_text(value) && Path::new(value).is_absolute()
}

fn safe_text(value: &str) -> bool {
    !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::{parse_identity, parse_repository_inventory, prune_is_due, success_is_fresh};

    fn repository_entry(number: u64, url: String) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "url": url,
            "isCrossRepository": false,
            "headRefName": "task/parser-proof"
        })
    }

    #[test]
    fn identity_parser_rejects_controls_extra_lines_and_relative_paths() {
        assert!(parse_identity("/work/root\ntask/good\n/work/common\n").is_some());
        assert!(parse_identity("/work/root\ntask/good\n/work/common\nextra\n").is_none());
        assert!(parse_identity("relative/root\ntask/good\n/work/common\n").is_none());
        assert!(parse_identity("/work/root\ntask/good\nrelative/common\n").is_none());
        assert!(parse_identity("/work/root\ntask/bad\u{1b}\n/work/common\n").is_none());
        assert!(parse_identity("/work/root\ntask/good\n/work/common\u{7}\n").is_none());
    }

    #[test]
    fn repository_parser_rejects_mismatched_and_noncanonical_urls() {
        let mismatched = serde_json::json!([repository_entry(
            41,
            "https://github.com/TheConnMan/agent-viewer/pull/42".to_string()
        )]);
        assert!(parse_repository_inventory(&mismatched.to_string()).is_none());

        for url in [
            "http://github.com/TheConnMan/agent-viewer/pull/41",
            "https://github.com/TheConnMan/agent-viewer/pull/41/files",
            "https://github.com/TheConnMan/agent-viewer/pull/041",
        ] {
            let noncanonical = serde_json::json!([repository_entry(41, url.to_string())]);
            assert!(parse_repository_inventory(&noncanonical.to_string()).is_none());
        }

        let control_head = serde_json::json!([{
            "number": 41,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/41",
            "isCrossRepository": false,
            "headRefName": "task/control\u{1b}"
        }]);
        assert!(parse_repository_inventory(&control_head.to_string()).is_none());
    }

    #[test]
    fn repository_parser_rejects_more_than_the_requested_limit() {
        let entries = (1..=101)
            .map(|number| {
                repository_entry(
                    number,
                    format!("https://github.com/TheConnMan/agent-viewer/pull/{number}"),
                )
            })
            .collect::<Vec<_>>();

        assert!(
            parse_repository_inventory(
                &serde_json::to_string(&entries).expect("serialize oversized inventory")
            )
            .is_none()
        );
    }

    #[test]
    fn freshness_and_pruning_treat_future_or_expired_stamps_as_due() {
        assert!(success_is_fresh(1_000, 60_999));
        assert!(!success_is_fresh(1_000, 61_000));
        assert!(!success_is_fresh(1_001, 1_000));

        assert!(prune_is_due(None, 1_000));
        assert!(!prune_is_due(Some(1_000), 60_999));
        assert!(prune_is_due(Some(1_000), 61_000));
        assert!(prune_is_due(Some(1_001), 1_000));
    }
}
