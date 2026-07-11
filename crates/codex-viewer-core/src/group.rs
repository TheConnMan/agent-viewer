/// Fold cwd to its project root:
/// - walk ancestors starting at cwd; first ancestor containing ".git":
///     - .git is a dir  -> that ancestor.
///     - .git is a file (linked worktree) -> parse its "gitdir: <p>" line; if <p>
///       contains "/.git/worktrees/", return the path prefix before "/.git/"
///       (the main repo root); otherwise return the ancestor itself.
/// - no .git found (or cwd does not exist) -> return cwd unchanged.
pub fn project_root(cwd: &std::path::Path) -> std::path::PathBuf {
    let _ = cwd;
    todo!()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectGroup {
    pub root: std::path::PathBuf,
    pub sessions: Vec<crate::backend::Session>,
}

/// Group by project_root(cwd) ACROSS backends. Sort each group's sessions by
/// updated_at_ms DESC; order groups by their newest session's updated_at_ms DESC.
pub fn group_by_project(sessions: Vec<crate::backend::Session>) -> Vec<ProjectGroup> {
    let _ = sessions;
    todo!()
}
