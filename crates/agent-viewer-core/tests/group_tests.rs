use agent_viewer_core::backend::{BackendKind, Session, SessionOrigin, Status};
use agent_viewer_core::group::{group_by_project, project_root};
use std::path::{Path, PathBuf};

fn sess(id: &str, cwd: &str, updated_at_ms: i64) -> Session {
    Session {
        backend: BackendKind::Codex,
        id: id.to_string(),
        short_id: None,
        origin: SessionOrigin::Exec,
        title: id.to_string(),
        cwd: PathBuf::from(cwd),
        git_branch: None,
        status: Status::Done,
        created_at_ms: updated_at_ms,
        updated_at_ms,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

#[test]
fn project_root_finds_git_dir_ancestor() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let cwd = repo.join("a").join("b");
    std::fs::create_dir_all(&cwd).unwrap();
    assert_eq!(project_root(&cwd), repo);
}

#[test]
fn project_root_resolves_linked_worktree() {
    let dir = tempfile::TempDir::new().unwrap();
    let main = dir.path().join("main");
    std::fs::create_dir_all(main.join(".git")).unwrap();
    let wt = dir.path().join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    // Linked worktree: .git is a FILE pointing into main/.git/worktrees/wt.
    let gitdir = main.join(".git").join("worktrees").join("wt");
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    // Worktrees fold into the main repo root, not the worktree dir.
    assert_eq!(project_root(&wt), main);
}

#[test]
fn project_root_falls_back_to_cwd() {
    let dir = tempfile::TempDir::new().unwrap();
    // No .git anywhere and the cwd does not exist -> input returned unchanged.
    let cwd = dir.path().join("does").join("not").join("exist");
    assert_eq!(project_root(&cwd), cwd);
}

/// Session with start time and update time set independently, so a test can prove the
/// sort keys on start time (created_at_ms) rather than activity (updated_at_ms).
fn sess_ct(id: &str, cwd: &str, created_at_ms: i64, updated_at_ms: i64) -> Session {
    Session {
        created_at_ms,
        ..sess(id, cwd, updated_at_ms)
    }
}

#[test]
fn group_by_project_orders_groups_by_directory_name() {
    // Two distinct roots (nonexistent paths -> project_root falls back to cwd).
    // root-z holds the most-recently-updated session but must still sort AFTER root-a,
    // proving group order is by directory name and does NOT jerk around with activity.
    let root_a = "/synthetic/root-a";
    let root_z = "/synthetic/root-z";
    let sessions = vec![
        sess("z1", root_z, 999),
        sess("a1", root_a, 100),
        sess("a2", root_a, 300),
    ];
    let groups = group_by_project(sessions);
    assert_eq!(groups.len(), 2);
    // Alphabetical by path: root-a before root-z, regardless of recency.
    assert_eq!(groups[0].root, Path::new(root_a));
    assert_eq!(groups[1].root, Path::new(root_z));
}

#[test]
fn group_by_project_sorts_sessions_by_start_time_ascending() {
    let root = "/synthetic/root";
    // s_old started first (created 100) but was updated most recently (updated 900).
    // s_new started later (created 500) but updated earlier (updated 200).
    // Equal creation values retain their input order even when update order disagrees.
    let sessions = vec![
        sess_ct("s_new", root, 500, 200),
        sess_ct("tie_z", root, 300, 800),
        sess_ct("s_old", root, 100, 900),
        sess_ct("tie_a", root, 300, 100),
    ];
    let groups = group_by_project(sessions);
    assert_eq!(
        groups[0]
            .sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec!["s_old", "tie_z", "tie_a", "s_new"]
    );
}
