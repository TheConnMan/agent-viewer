//! mark_dead_dirs — flags any session whose cwd no longer exists on disk as a companion
//! (so the default view hides deleted-dir noise, e.g. agentos /tmp sessions).

use agent_viewer_core::backend::{BackendKind, Session, SessionOrigin, Status};
use agent_viewer_core::mark_dead_dirs;
use std::path::PathBuf;
use tempfile::TempDir;

fn sess(cwd: PathBuf, companion: bool) -> Session {
    Session {
        backend: BackendKind::Codex,
        id: "s".to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: "t".to_string(),
        cwd,
        git_branch: None,
        status: Status::Done,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion,
        subagent: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

#[test]
fn mark_dead_dirs_flags_only_missing_cwds() {
    let live_dir = TempDir::new().unwrap();
    let dead_dir = TempDir::new().unwrap();
    let dead_path = dead_dir.path().to_path_buf();
    drop(dead_dir); // delete the dir so its cwd path no longer exists.

    let mut sessions = vec![
        sess(live_dir.path().to_path_buf(), false), // existing cwd -> untouched
        sess(dead_path, false),                     // missing cwd -> flagged companion
        sess(PathBuf::from(""), false),             // empty cwd -> not a dead dir, untouched
    ];
    mark_dead_dirs(&mut sessions);

    assert!(!sessions[0].companion); // dir still exists
    assert!(sessions[1].companion); // dir is gone
    assert!(!sessions[2].companion); // empty cwd is skipped
}

#[test]
fn mark_dead_dirs_leaves_existing_companions() {
    let live_dir = TempDir::new().unwrap();
    let mut sessions = vec![sess(live_dir.path().to_path_buf(), true)];
    mark_dead_dirs(&mut sessions);
    // Already a companion with a live dir -> unchanged (the helper only sets, never clears).
    assert!(sessions[0].companion);
}
