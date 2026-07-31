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
