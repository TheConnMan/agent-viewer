use agent_viewer_core::{
    BackendKind, GrokPrFallback, PrRef, Session, SessionOrigin, Status, ViewerDb,
};
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Debug, PartialEq, Eq)]
struct RecordedCommand {
    program: OsString,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    timeout: Duration,
}

#[derive(Default)]
struct RecordingRunner {
    responses: VecDeque<Option<String>>,
    calls: Vec<RecordedCommand>,
}

impl RecordingRunner {
    fn with_responses(responses: impl IntoIterator<Item = Option<String>>) -> RecordingRunner {
        RecordingRunner {
            responses: responses.into_iter().collect(),
            calls: Vec::new(),
        }
    }

    fn run(&mut self, command: Command, timeout: Duration) -> Option<String> {
        self.calls.push(RecordedCommand {
            program: command.get_program().to_os_string(),
            args: command.get_args().map(OsString::from).collect(),
            current_dir: command.get_current_dir().map(Path::to_path_buf),
            timeout,
        });
        self.responses.pop_front().flatten()
    }
}

fn grok_session(id: &str, cwd: &Path) -> Session {
    Session {
        backend: BackendKind::Grok,
        id: id.to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: id.to_string(),
        cwd: cwd.to_path_buf(),
        git_branch: None,
        status: Status::Unknown,
        created_at_ms: 1,
        updated_at_ms: 1,
        hidden: false,
        companion: false,
        subagent: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

fn git_identity(root: &Path, branch: &str, common_dir: &Path) -> String {
    format!("{}\n{}\n{}\n", root.display(), branch, common_dir.display())
}

fn seed_identity(
    db: &ViewerDb,
    cwd: &Path,
    root: &Path,
    branch: Option<&str>,
    common_dir: &Path,
    now_ms: i64,
) {
    let cwd = cwd.to_str().expect("UTF8 worktree path");
    let token = db
        .claim_grok_worktree_identity(cwd, now_ms)
        .expect("claim worktree identity")
        .expect("unclaimed worktree identity");
    assert!(
        db.complete_grok_worktree_identity(
            &token,
            root.to_str().expect("UTF8 worktree root"),
            branch,
            common_dir.to_str().expect("UTF8 git common directory"),
            now_ms,
        )
        .expect("complete worktree identity")
    );
}

fn seed_inventory(db: &ViewerDb, common_dir: &Path, branches: &[(&str, u64)], now_ms: i64) {
    let common_dir = common_dir.to_str().expect("UTF8 git common directory");
    let token = db
        .claim_grok_repository_prs(common_dir, now_ms)
        .expect("claim repository inventory")
        .expect("unclaimed repository inventory");
    let inventory = branches
        .iter()
        .map(|(branch, number)| {
            (
                (*branch).to_string(),
                vec![PrRef {
                    id: number.to_string(),
                    href: Some(format!(
                        "https://github.com/TheConnMan/agent-viewer/pull/{number}"
                    )),
                }],
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert!(
        db.complete_grok_repository_prs(
            &token,
            &serde_json::to_string(&inventory).expect("serialize repository inventory"),
            now_ms,
        )
        .expect("complete repository inventory")
    );
}

fn cli_inventory(branches: &[(&str, u64)]) -> String {
    serde_json::Value::Array(
        branches
            .iter()
            .map(|(branch, number)| {
                serde_json::json!({
                    "number": number,
                    "url": format!("https://github.com/TheConnMan/agent-viewer/pull/{number}"),
                    "isCrossRepository": false,
                    "headRefName": branch
                })
            })
            .collect(),
    )
    .to_string()
}

fn run_pass(
    fallback: &mut GrokPrFallback,
    db: &ViewerDb,
    rows: &mut [Session],
    now_ms: i64,
    runner: &mut RecordingRunner,
) {
    let mut invoke = |command, timeout| runner.run(command, timeout);
    let mut clock = || now_ms;
    fallback
        .attach_branch_pr_refs_with_runner(db, rows, &mut clock, &mut invoke)
        .expect("decorate Grok pull request refs");
}

fn ref_ids(row: &Session) -> Vec<&str> {
    row.pr_refs
        .iter()
        .map(|reference| reference.id.as_str())
        .collect()
}

#[test]
fn exact_commands_map_live_branch_without_a_base_filter() {
    let directory = tempfile::tempdir().expect("temporary repository parent");
    let root = directory.path().join("linked worktree with spaces");
    let cwd = root.join("nested directory");
    let common_dir = directory.path().join("shared git directory");
    std::fs::create_dir_all(&cwd).expect("worktree directory");
    std::fs::create_dir_all(&common_dir).expect("git common directory");
    let database =
        ViewerDb::open(&directory.path().join("viewer.sqlite")).expect("temporary viewer database");
    let branch = "task/grok-pr-branch-fallback";
    let inventory = serde_json::json!([
        {
            "number": 101,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/101",
            "isCrossRepository": false,
            "headRefName": branch
        },
        {
            "number": 102,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/102",
            "isCrossRepository": false,
            "headRefName": branch
        },
        {
            "number": 103,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/103",
            "isCrossRepository": false,
            "headRefName": branch
        },
        {
            "number": 104,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/104",
            "isCrossRepository": false,
            "headRefName": branch
        },
        {
            "number": 105,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/105",
            "isCrossRepository": false,
            "headRefName": branch
        },
        {
            "number": 999,
            "url": "https://github.com/a-fork/agent-viewer/pull/999",
            "isCrossRepository": true,
            "headRefName": branch
        },
        {
            "number": 220,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/220",
            "isCrossRepository": false,
            "headRefName": "task/another-branch"
        }
    ])
    .to_string();
    let mut runner = RecordingRunner::with_responses([
        Some(git_identity(&root, branch, &common_dir)),
        Some(inventory),
    ]);
    let mut fallback = GrokPrFallback::default();
    let mut rows = vec![grok_session("first", &cwd), grok_session("second", &cwd)];

    run_pass(&mut fallback, &database, &mut rows, 1_000, &mut runner);

    assert_eq!(
        runner.calls.len(),
        1,
        "one pass may execute only one command"
    );
    assert!(rows.iter().all(|row| row.pr_refs.is_empty()));
    assert_eq!(runner.calls[0].program, OsString::from("git"));
    assert_eq!(
        runner.calls[0].args,
        vec![
            OsString::from("-C"),
            cwd.as_os_str().to_os_string(),
            OsString::from("rev-parse"),
            OsString::from("--path-format=absolute"),
            OsString::from("--show-toplevel"),
            OsString::from("--abbrev-ref"),
            OsString::from("HEAD"),
            OsString::from("--git-common-dir"),
        ]
    );
    assert_eq!(runner.calls[0].current_dir, None);
    assert_eq!(runner.calls[0].timeout, Duration::from_secs(2));

    run_pass(&mut fallback, &database, &mut rows, 1_001, &mut runner);

    assert_eq!(runner.calls.len(), 2, "the second pass may run only GitHub");
    assert_eq!(runner.calls[1].program, OsString::from("gh"));
    assert_eq!(
        runner.calls[1].args,
        vec![
            OsString::from("pr"),
            OsString::from("list"),
            OsString::from("--state"),
            OsString::from("open"),
            OsString::from("--json"),
            OsString::from("number,url,isCrossRepository,headRefName"),
            OsString::from("--limit"),
            OsString::from("100"),
        ]
    );
    assert_eq!(runner.calls[1].current_dir.as_deref(), Some(root.as_path()));
    assert_eq!(runner.calls[1].timeout, Duration::from_secs(10));
    assert!(
        runner.calls[1]
            .args
            .iter()
            .all(|argument| !matches!(argument.to_str(), Some("--head" | "--base"))),
        "base and head filters would hide pull requests into next or another base"
    );
    assert_eq!(ref_ids(&rows[0]), vec!["102", "103", "104", "105"]);
    assert_eq!(rows[1].pr_refs, rows[0].pr_refs);
}

#[test]
fn transcript_refs_are_authoritative_and_suppress_fallback_commands() {
    let directory = tempfile::tempdir().expect("temporary viewer state");
    let database =
        ViewerDb::open(&directory.path().join("viewer.sqlite")).expect("temporary viewer database");
    let mut row = grok_session("transcript", directory.path());
    row.pr_refs.push(PrRef {
        id: "77".to_string(),
        href: Some("https://github.com/TheConnMan/agent-viewer/pull/77".to_string()),
    });
    let expected = row.pr_refs.clone();
    let mut rows = vec![row];
    let mut runner = RecordingRunner::default();

    run_pass(
        &mut GrokPrFallback::default(),
        &database,
        &mut rows,
        1_000,
        &mut runner,
    );

    assert!(runner.calls.is_empty());
    assert_eq!(rows[0].pr_refs, expected);
}

#[test]
fn malformed_repository_inventory_is_rejected_as_one_snapshot() {
    let directory = tempfile::tempdir().expect("temporary repository parent");
    let root = directory.path().join("worktree");
    let common_dir = directory.path().join("git-common");
    std::fs::create_dir_all(&root).expect("worktree directory");
    std::fs::create_dir_all(&common_dir).expect("git common directory");
    let database =
        ViewerDb::open(&directory.path().join("viewer.sqlite")).expect("temporary viewer database");
    let branch = "task/coherent-inventory";
    let malformed = serde_json::json!([
        {
            "number": 41,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/41",
            "isCrossRepository": false,
            "headRefName": branch
        },
        {
            "number": 42,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/42",
            "headRefName": branch
        }
    ])
    .to_string();
    let mut runner = RecordingRunner::with_responses([
        Some(git_identity(&root, branch, &common_dir)),
        Some(malformed),
    ]);
    let mut fallback = GrokPrFallback::default();
    let mut rows = vec![grok_session("coherent", &root)];

    run_pass(&mut fallback, &database, &mut rows, 1_000, &mut runner);
    run_pass(&mut fallback, &database, &mut rows, 1_001, &mut runner);

    assert_eq!(runner.calls.len(), 2);
    assert!(rows[0].pr_refs.is_empty());
}

#[test]
fn slow_commands_keep_the_full_claim_and_retry_from_completion() {
    {
        let directory = tempfile::tempdir().expect("temporary identity state");
        let path = directory.path().join("viewer.sqlite");
        let first = ViewerDb::open(&path).expect("first viewer database");
        let second = ViewerDb::open(&path).expect("second viewer database");
        let cwd = directory.path().join("identity-worktree");
        std::fs::create_dir_all(&cwd).expect("identity worktree");
        let cwd_text = cwd.to_str().expect("UTF8 identity worktree").to_string();
        let mut rows = vec![grok_session("slow-identity", &cwd)];
        let mut fallback = GrokPrFallback::default();
        let mut ticks = VecDeque::from([1_000, 2_900]);
        let mut clock = || ticks.pop_front().expect("identity clock sample");
        let mut calls = 0;
        let mut runner = |_command: Command, timeout: Duration| {
            calls += 1;
            assert_eq!(timeout, Duration::from_secs(2));
            assert!(
                second
                    .claim_grok_worktree_identity(&cwd_text, 2_899)
                    .expect("competing identity claim")
                    .is_none(),
                "the original claim must cover the full command timeout"
            );
            None
        };

        fallback
            .attach_branch_pr_refs_with_runner(&first, &mut rows, &mut clock, &mut runner)
            .expect("failed identity refresh");

        assert_eq!(calls, 1);
        assert!(rows[0].pr_refs.is_empty());
        assert!(
            second
                .claim_grok_worktree_identity(&cwd_text, 7_899)
                .expect("identity claim during retry fence")
                .is_none(),
            "the five second retry fence must start when the command completes"
        );
        assert!(
            second
                .claim_grok_worktree_identity(&cwd_text, 7_900)
                .expect("identity claim after retry fence")
                .is_some()
        );
    }

    {
        let directory = tempfile::tempdir().expect("temporary repository state");
        let path = directory.path().join("viewer.sqlite");
        let first = ViewerDb::open(&path).expect("first viewer database");
        let second = ViewerDb::open(&path).expect("second viewer database");
        let root = directory.path().join("repository-worktree");
        let common_dir = directory.path().join("repository-common");
        std::fs::create_dir_all(&root).expect("repository worktree");
        std::fs::create_dir_all(&common_dir).expect("repository common directory");
        seed_identity(
            &first,
            &root,
            &root,
            Some("task/slow-repository"),
            &common_dir,
            1_000,
        );
        let common_text = common_dir
            .to_str()
            .expect("UTF8 repository common directory")
            .to_string();
        let mut rows = vec![grok_session("slow-repository", &root)];
        let mut fallback = GrokPrFallback::default();
        let mut ticks = VecDeque::from([10_000, 19_900]);
        let mut clock = || ticks.pop_front().expect("repository clock sample");
        let mut calls = 0;
        let mut runner = |_command: Command, timeout: Duration| {
            calls += 1;
            assert_eq!(timeout, Duration::from_secs(10));
            assert!(
                second
                    .claim_grok_repository_prs(&common_text, 19_899)
                    .expect("competing repository claim")
                    .is_none(),
                "the original claim must cover the full command timeout"
            );
            None
        };

        fallback
            .attach_branch_pr_refs_with_runner(&first, &mut rows, &mut clock, &mut runner)
            .expect("failed repository refresh");

        assert_eq!(calls, 1);
        assert!(rows[0].pr_refs.is_empty());
        assert!(
            second
                .claim_grok_repository_prs(&common_text, 24_899)
                .expect("repository claim during retry fence")
                .is_none(),
            "the five second retry fence must start when the command completes"
        );
        assert!(
            second
                .claim_grok_repository_prs(&common_text, 24_900)
                .expect("repository claim after retry fence")
                .is_some()
        );
    }
}

#[test]
fn slow_successes_are_stamped_at_command_completion() {
    let directory = tempfile::tempdir().expect("temporary slow success state");
    let database =
        ViewerDb::open(&directory.path().join("viewer.sqlite")).expect("temporary viewer database");
    let root = directory.path().join("slow-success-worktree");
    let common_dir = directory.path().join("slow-success-common");
    std::fs::create_dir_all(&root).expect("slow success worktree");
    std::fs::create_dir_all(&common_dir).expect("slow success common directory");
    let branch = "task/slow-success";
    let mut fallback = GrokPrFallback::default();
    let mut rows = vec![grok_session("slow-success", &root)];
    let mut identity_ticks = VecDeque::from([1_000, 2_900]);
    let mut identity_clock = || identity_ticks.pop_front().expect("identity clock sample");
    let mut identity_runner =
        |_command: Command, _timeout: Duration| Some(git_identity(&root, branch, &common_dir));

    fallback
        .attach_branch_pr_refs_with_runner(
            &database,
            &mut rows,
            &mut identity_clock,
            &mut identity_runner,
        )
        .expect("slow identity success");

    let identities = database
        .grok_worktree_identities(&[root.to_str().expect("UTF8 worktree").to_string()])
        .expect("read completed identity");
    assert_eq!(identities[0].success_at_ms, 2_900);

    let mut repository_ticks = VecDeque::from([3_000, 12_900]);
    let mut repository_clock = || {
        repository_ticks
            .pop_front()
            .expect("repository clock sample")
    };
    let mut repository_runner = |_command: Command, _timeout: Duration| Some("[]".to_string());
    fallback
        .attach_branch_pr_refs_with_runner(
            &database,
            &mut rows,
            &mut repository_clock,
            &mut repository_runner,
        )
        .expect("slow repository success");

    let repositories = database
        .grok_repository_prs(&[common_dir
            .to_str()
            .expect("UTF8 common directory")
            .to_string()])
        .expect("read completed repository");
    assert_eq!(repositories[0].success_at_ms, 12_900);
}

#[test]
fn advisory_write_failure_keeps_warm_refs_and_retries_after_unlock() {
    let directory = tempfile::tempdir().expect("temporary advisory write state");
    let path = directory.path().join("viewer.sqlite");
    let database = ViewerDb::open(&path).expect("temporary viewer database");
    let root = directory.path().join("warm-worktree");
    let common_dir = directory.path().join("warm-common");
    std::fs::create_dir_all(&root).expect("warm worktree");
    std::fs::create_dir_all(&common_dir).expect("warm common directory");
    let branch = "task/warm-advisory-write";
    seed_identity(&database, &root, &root, Some(branch), &common_dir, 1_000);
    seed_inventory(&database, &common_dir, &[(branch, 511)], 1_000);

    let blocker = rusqlite::Connection::open(&path).expect("open blocking connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold advisory write lock");
    let mut fallback = GrokPrFallback::default();
    let mut rows = vec![grok_session("blocked-write", &root)];
    let mut blocked_calls = 0;
    let mut blocked_clock = || 61_000;
    let mut blocked_runner = |_command: Command, _timeout: Duration| {
        blocked_calls += 1;
        None
    };

    let blocked = fallback.attach_branch_pr_refs_with_runner(
        &database,
        &mut rows,
        &mut blocked_clock,
        &mut blocked_runner,
    );

    assert!(
        blocked.is_ok(),
        "an advisory write failure must not fail listing"
    );
    assert_eq!(blocked_calls, 0);
    assert_eq!(ref_ids(&rows[0]), vec!["511"]);
    blocker
        .execute_batch("ROLLBACK;")
        .expect("release write lock");

    let mut later_rows = vec![grok_session("later-pass", &root)];
    let mut later_calls = 0;
    let mut later_clock = || 61_001;
    let mut later_runner = |command: Command, _timeout: Duration| {
        later_calls += 1;
        if command.get_program() == "git" {
            Some(git_identity(&root, branch, &common_dir))
        } else {
            Some(cli_inventory(&[(branch, 511)]))
        }
    };
    fallback
        .attach_branch_pr_refs_with_runner(
            &database,
            &mut later_rows,
            &mut later_clock,
            &mut later_runner,
        )
        .expect("retry after releasing advisory write lock");

    assert_eq!(later_calls, 1);
    assert_eq!(ref_ids(&later_rows[0]), vec!["511"]);
}

#[test]
fn initial_cache_read_failure_returns_error_without_commands_or_mutation() {
    let directory = tempfile::tempdir().expect("temporary read failure state");
    let path = directory.path().join("viewer.sqlite");
    let database = ViewerDb::open(&path).expect("temporary viewer database");
    let root = directory.path().join("read-failure-worktree");
    let common_dir = directory.path().join("read-failure-common");
    std::fs::create_dir_all(&root).expect("read failure worktree");
    std::fs::create_dir_all(&common_dir).expect("read failure common directory");
    seed_identity(
        &database,
        &root,
        &root,
        Some("task/read-failure"),
        &common_dir,
        1_000,
    );
    seed_inventory(&database, &common_dir, &[("task/read-failure", 512)], 1_000);
    rusqlite::Connection::open(&path)
        .expect("open schema breaker")
        .execute_batch("DROP TABLE worktree_identity;")
        .expect("drop identity table");
    let mut rows = vec![grok_session("unreadable", &root)];
    let original = rows.clone();
    let mut calls = 0;
    let mut clock = || 2_000;
    let mut runner = |_command: Command, _timeout: Duration| {
        calls += 1;
        None
    };

    let result = GrokPrFallback::default().attach_branch_pr_refs_with_runner(
        &database,
        &mut rows,
        &mut clock,
        &mut runner,
    );

    assert!(result.is_err());
    assert_eq!(calls, 0);
    assert_eq!(rows, original);
}

#[test]
fn first_lookup_negative_cases_stay_empty_without_repeated_work() {
    let directory = tempfile::tempdir().expect("temporary negative state");
    let database =
        ViewerDb::open(&directory.path().join("viewer.sqlite")).expect("temporary viewer database");
    let missing = directory.path().join("missing-worktree");
    let mut missing_rows = vec![grok_session("missing", &missing)];
    let mut missing_runner = RecordingRunner::default();
    run_pass(
        &mut GrokPrFallback::default(),
        &database,
        &mut missing_rows,
        1_000,
        &mut missing_runner,
    );
    assert!(missing_runner.calls.is_empty());
    assert!(missing_rows[0].pr_refs.is_empty());

    let nonrepository = directory.path().join("not-a-repository");
    std::fs::create_dir_all(&nonrepository).expect("nonrepository directory");
    let mut failed_rows = vec![grok_session("nonrepository", &nonrepository)];
    let mut failed_runner = RecordingRunner::with_responses([None]);
    run_pass(
        &mut GrokPrFallback::default(),
        &database,
        &mut failed_rows,
        2_000,
        &mut failed_runner,
    );
    assert_eq!(failed_runner.calls.len(), 1);
    assert!(failed_rows[0].pr_refs.is_empty());

    let root = directory.path().join("negative-worktree");
    let common_dir = directory.path().join("negative-common");
    std::fs::create_dir_all(&root).expect("negative worktree");
    std::fs::create_dir_all(&common_dir).expect("negative common directory");
    let branch = "task/no-open-pr";
    let inventory = serde_json::json!([
        {
            "number": 91,
            "url": "https://github.com/a-fork/agent-viewer/pull/91",
            "isCrossRepository": true,
            "headRefName": branch
        },
        {
            "number": 92,
            "url": "https://github.com/TheConnMan/agent-viewer/pull/92",
            "isCrossRepository": false,
            "headRefName": "task/different-branch"
        }
    ])
    .to_string();
    let mut fallback = GrokPrFallback::default();
    let mut rows = vec![grok_session("no-match", &root)];
    let mut first_lookup = RecordingRunner::with_responses([
        Some(git_identity(&root, branch, &common_dir)),
        Some(inventory),
    ]);
    run_pass(
        &mut fallback,
        &database,
        &mut rows,
        10_000,
        &mut first_lookup,
    );
    run_pass(
        &mut fallback,
        &database,
        &mut rows,
        10_001,
        &mut first_lookup,
    );
    assert_eq!(first_lookup.calls.len(), 2);
    assert!(rows[0].pr_refs.is_empty());

    let mut warm_negative = RecordingRunner::default();
    run_pass(
        &mut fallback,
        &database,
        &mut rows,
        20_000,
        &mut warm_negative,
    );
    assert!(warm_negative.calls.is_empty());
    assert!(rows[0].pr_refs.is_empty());
}

#[test]
fn detached_head_is_a_successful_negative_for_the_full_freshness_window() {
    let directory = tempfile::tempdir().expect("temporary detached state");
    let database =
        ViewerDb::open(&directory.path().join("viewer.sqlite")).expect("temporary viewer database");
    let root = directory.path().join("detached-worktree");
    let common_dir = directory.path().join("detached-common");
    std::fs::create_dir_all(&root).expect("detached worktree");
    std::fs::create_dir_all(&common_dir).expect("detached common directory");
    let mut fallback = GrokPrFallback::default();
    let mut rows = vec![grok_session("detached", &root)];
    let mut initial =
        RecordingRunner::with_responses([Some(git_identity(&root, "HEAD", &common_dir))]);
    run_pass(&mut fallback, &database, &mut rows, 1_000, &mut initial);
    assert_eq!(initial.calls.len(), 1);
    assert!(rows[0].pr_refs.is_empty());

    let mut fresh = RecordingRunner::default();
    run_pass(&mut fallback, &database, &mut rows, 60_999, &mut fresh);
    assert!(fresh.calls.is_empty());

    let mut expired =
        RecordingRunner::with_responses([Some(git_identity(&root, "HEAD", &common_dir))]);
    run_pass(&mut fallback, &database, &mut rows, 61_000, &mut expired);
    assert_eq!(expired.calls.len(), 1);
    assert!(rows[0].pr_refs.is_empty());
}

#[test]
fn one_repository_inventory_serves_several_branches_and_new_worktrees() {
    let directory = tempfile::tempdir().expect("temporary shared repository state");
    let database =
        ViewerDb::open(&directory.path().join("viewer.sqlite")).expect("temporary viewer database");
    let common_dir = directory.path().join("shared-common");
    std::fs::create_dir_all(&common_dir).expect("shared common directory");
    let branches = [
        ("task/shared-a", 301),
        ("task/shared-b", 302),
        ("task/shared-c", 303),
        ("task/shared-d", 304),
    ];
    let mut rows = Vec::new();
    for (index, (branch, _)) in branches[..3].iter().enumerate() {
        let root = directory.path().join(format!("worktree-{index}"));
        std::fs::create_dir_all(&root).expect("shared worktree");
        seed_identity(
            &database,
            &root,
            &root,
            Some(*branch),
            &common_dir,
            100 + index as i64,
        );
        rows.push(grok_session(branch, &root));
    }
    let mut fallback = GrokPrFallback::default();
    let mut inventory_runner = RecordingRunner::with_responses([Some(cli_inventory(&branches))]);

    run_pass(
        &mut fallback,
        &database,
        &mut rows,
        1_000,
        &mut inventory_runner,
    );

    assert_eq!(inventory_runner.calls.len(), 1);
    assert_eq!(ref_ids(&rows[0]), vec!["301"]);
    assert_eq!(ref_ids(&rows[1]), vec!["302"]);
    assert_eq!(ref_ids(&rows[2]), vec!["303"]);

    let new_root = directory.path().join("worktree-new");
    std::fs::create_dir_all(&new_root).expect("new shared worktree");
    seed_identity(
        &database,
        &new_root,
        &new_root,
        Some(branches[3].0),
        &common_dir,
        1_100,
    );
    let mut new_rows = vec![grok_session("new-branch", &new_root)];
    let mut reuse_runner = RecordingRunner::default();
    run_pass(
        &mut fallback,
        &database,
        &mut new_rows,
        1_101,
        &mut reuse_runner,
    );
    assert!(reuse_runner.calls.is_empty());
    assert_eq!(ref_ids(&new_rows[0]), vec!["304"]);
}

#[test]
fn stale_values_survive_failures_and_a_branch_switch_remaps_directly() {
    let directory = tempfile::tempdir().expect("temporary stale state");
    let database =
        ViewerDb::open(&directory.path().join("viewer.sqlite")).expect("temporary viewer database");
    let root = directory.path().join("switch-worktree");
    let common_dir = directory.path().join("switch-common");
    std::fs::create_dir_all(&root).expect("switch worktree");
    std::fs::create_dir_all(&common_dir).expect("switch common directory");
    let old_branch = "task/old-branch";
    let new_branch = "task/new-branch";
    seed_identity(
        &database,
        &root,
        &root,
        Some(old_branch),
        &common_dir,
        1_000,
    );
    seed_inventory(
        &database,
        &common_dir,
        &[(old_branch, 401), (new_branch, 402)],
        1_000,
    );

    let mut identity_failure = GrokPrFallback::default();
    let mut failed_identity_rows = vec![grok_session("identity-failure", &root)];
    let mut failed_identity = RecordingRunner::with_responses([None]);
    run_pass(
        &mut identity_failure,
        &database,
        &mut failed_identity_rows,
        61_000,
        &mut failed_identity,
    );
    assert_eq!(failed_identity.calls.len(), 1);
    assert_eq!(ref_ids(&failed_identity_rows[0]), vec!["401"]);

    let mut switch_fallback = GrokPrFallback::default();
    let mut switched_rows = vec![grok_session("switched", &root)];
    let mut switch_runner =
        RecordingRunner::with_responses([Some(git_identity(&root, new_branch, &common_dir))]);
    run_pass(
        &mut switch_fallback,
        &database,
        &mut switched_rows,
        66_000,
        &mut switch_runner,
    );
    assert_eq!(switch_runner.calls.len(), 1);
    assert_eq!(ref_ids(&switched_rows[0]), vec!["402"]);

    let mut repository_failure_rows = vec![grok_session("repository-failure", &root)];
    let mut repository_failure = RecordingRunner::with_responses([None]);
    run_pass(
        &mut switch_fallback,
        &database,
        &mut repository_failure_rows,
        66_001,
        &mut repository_failure,
    );
    assert_eq!(repository_failure.calls.len(), 1);
    assert_eq!(ref_ids(&repository_failure_rows[0]), vec!["402"]);
}

#[test]
fn forty_cold_paths_make_fair_progress_with_one_command_per_pass() {
    let directory = tempfile::tempdir().expect("temporary fairness state");
    let database =
        ViewerDb::open(&directory.path().join("viewer.sqlite")).expect("temporary viewer database");
    let mut templates = Vec::new();
    let mut identities = HashMap::<String, (PathBuf, String, PathBuf, usize)>::new();
    let mut repository_inventories = HashMap::<usize, String>::new();
    for repository_index in 0..4 {
        let common_dir = directory
            .path()
            .join(format!("repository-{repository_index}-common"));
        std::fs::create_dir_all(&common_dir).expect("fairness common directory");
        let mut branches = Vec::new();
        for local_index in 0..10 {
            let index = repository_index * 10 + local_index;
            let root = directory.path().join(format!(
                "repository-{repository_index}-worktree-{local_index:02}"
            ));
            std::fs::create_dir_all(&root).expect("fairness worktree");
            let branch = format!("task/cold-{index:02}");
            identities.insert(
                root.to_str().expect("UTF8 fairness worktree").to_string(),
                (
                    root.clone(),
                    branch.clone(),
                    common_dir.clone(),
                    repository_index,
                ),
            );
            branches.push((branch, 1_000 + index as u64));
            templates.push(grok_session(&format!("cold-{index:02}"), &root));
        }
        let borrowed = branches
            .iter()
            .map(|(branch, number)| (branch.as_str(), *number))
            .collect::<Vec<_>>();
        repository_inventories.insert(repository_index, cli_inventory(&borrowed));
    }

    let mut fallback = GrokPrFallback::default();
    let mut command_order = Vec::<String>::new();
    let mut final_rows = None;
    for pass in 0..80 {
        let now_ms = 1_000 + pass;
        let mut rows = templates.clone();
        let mut calls_this_pass = 0;
        let mut clock = || now_ms;
        let mut runner = |command: Command, _timeout: Duration| {
            calls_this_pass += 1;
            let program = command.get_program().to_string_lossy();
            if program == "git" {
                let cwd = command
                    .get_args()
                    .nth(1)
                    .expect("git cwd argument")
                    .to_string_lossy()
                    .to_string();
                let (root, branch, common_dir, repository_index) =
                    identities.get(&cwd).expect("known fairness worktree");
                command_order.push(format!("git:{repository_index}"));
                Some(git_identity(root, branch, common_dir))
            } else {
                assert_eq!(program, "gh");
                let current_dir = command.get_current_dir().expect("GitHub worktree root");
                let repository_index = identities
                    .get(current_dir.to_str().expect("UTF8 GitHub worktree root"))
                    .expect("known GitHub worktree root")
                    .3;
                command_order.push(format!("gh:{repository_index}"));
                repository_inventories.get(&repository_index).cloned()
            }
        };
        fallback
            .attach_branch_pr_refs_with_runner(&database, &mut rows, &mut clock, &mut runner)
            .expect("fair fallback pass");
        assert!(
            calls_this_pass <= 1,
            "one pass executed more than one command"
        );
        let complete = rows.iter().all(|row| row.pr_refs.len() == 1);
        if complete {
            final_rows = Some(rows);
            break;
        }
    }

    assert_eq!(
        command_order
            .iter()
            .filter(|kind| kind.starts_with("git:"))
            .count(),
        40
    );
    assert_eq!(
        command_order
            .iter()
            .filter(|kind| kind.starts_with("gh:"))
            .count(),
        4
    );
    for repository_index in 0..4 {
        let git = format!("git:{repository_index}");
        let gh = format!("gh:{repository_index}");
        let first_git = command_order
            .iter()
            .position(|kind| kind == &git)
            .expect("repository identity received a turn");
        let repository = command_order
            .iter()
            .position(|kind| kind == &gh)
            .expect("repository inventory received a turn");
        assert_eq!(
            repository,
            first_git + 1,
            "repository work must run immediately after its first identity"
        );
    }
    let final_rows = final_rows.expect("all cold paths eventually received refs");
    assert_eq!(final_rows.len(), 40);
    assert!(final_rows.iter().all(|row| row.pr_refs.len() == 1));
}
