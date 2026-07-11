use std::collections::HashMap;
use std::path::PathBuf;

/// Fold cwd to its project root:
/// - walk ancestors starting at cwd; first ancestor containing ".git":
///     - .git is a dir  -> that ancestor.
///     - .git is a file (linked worktree) -> parse its "gitdir: <p>" line; if <p>
///       contains "/.git/worktrees/", return the path prefix before "/.git/"
///       (the main repo root); otherwise return the ancestor itself.
/// - no .git found (or cwd does not exist) -> return cwd unchanged.
pub fn project_root(cwd: &std::path::Path) -> std::path::PathBuf {
    if !cwd.exists() {
        return cwd.to_path_buf();
    }
    for ancestor in cwd.ancestors() {
        let git = ancestor.join(".git");
        if git.is_dir() {
            return ancestor.to_path_buf();
        }
        if git.is_file() {
            if let Ok(contents) = std::fs::read_to_string(&git)
                && let Some(gitdir) = contents
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("gitdir:"))
            {
                let gitdir = gitdir.trim();
                if let Some(idx) = gitdir.find("/.git/worktrees/") {
                    return PathBuf::from(&gitdir[..idx]);
                }
            }
            return ancestor.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectGroup {
    pub root: std::path::PathBuf,
    pub sessions: Vec<crate::backend::Session>,
}

/// Group by project_root(cwd) ACROSS backends. Order groups by directory path
/// (case-insensitive) so the list stays stable regardless of session activity;
/// sort each group's sessions by start time (created_at_ms) DESC.
pub fn group_by_project(sessions: Vec<crate::backend::Session>) -> Vec<ProjectGroup> {
    let mut by_root: HashMap<PathBuf, Vec<crate::backend::Session>> = HashMap::new();
    for session in sessions {
        let root = project_root(&session.cwd);
        by_root.entry(root).or_default().push(session);
    }
    let mut groups: Vec<ProjectGroup> = by_root
        .into_iter()
        .map(|(root, mut sessions)| {
            sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at_ms));
            ProjectGroup { root, sessions }
        })
        .collect();
    // Stable order: by directory path, case-insensitive, independent of activity.
    groups.sort_by(|a, b| {
        a.root
            .to_string_lossy()
            .to_lowercase()
            .cmp(&b.root.to_string_lossy().to_lowercase())
            .then_with(|| a.root.cmp(&b.root))
    });
    groups
}
