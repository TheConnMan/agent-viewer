use agent_viewer_core::group::project_root;

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
