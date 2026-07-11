use agent_viewer_core::backend::{BackendKind, Session, Status};
use agent_viewer_core::group::{group_by_project, project_root};
use std::path::{Path, PathBuf};

fn sess(id: &str, cwd: &str, updated_at_ms: i64) -> Session {
    Session {
        backend: BackendKind::Codex,
        id: id.to_string(),
        short_id: None,
        title: id.to_string(),
        cwd: PathBuf::from(cwd),
        created_at_ms: updated_at_ms,
        updated_at_ms,
        status: Status::Done,
        hidden: false,
        source_label: "exec".to_string(),
        summary: String::new(),
        companion: false,
        pid: None,
        rollout_path: None,
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

#[test]
fn group_by_project_orders_groups_by_newest_session() {
    // Two distinct roots (nonexistent paths -> project_root falls back to cwd).
    let root_a = "/synthetic/root-a";
    let root_b = "/synthetic/root-b";
    let sessions = vec![
        sess("a1", root_a, 100),
        sess("b3", root_b, 200),
        sess("a2", root_a, 300),
    ];
    let groups = group_by_project(sessions);
    assert_eq!(groups.len(), 2);
    // Group order by newest session DESC: root-a (300) before root-b (200).
    assert_eq!(groups[0].root, Path::new(root_a));
    assert_eq!(groups[1].root, Path::new(root_b));
    // Intra-group recency DESC.
    assert_eq!(
        groups[0]
            .sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a2", "a1"]
    );
    assert_eq!(
        groups[1]
            .sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec!["b3"]
    );
}
