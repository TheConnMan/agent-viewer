//! attach_command wiring for the codex backend (no network): the resume command must be
//! pinned to the session cwd so `codex resume` does not prompt "Choose working directory".

use agent_viewer_core::backend::{Backend, BackendKind, Session, Status};
use agent_viewer_core::codex::CodexBackend;
use std::path::PathBuf;

fn session_with_cwd(cwd: PathBuf) -> Session {
    Session {
        backend: BackendKind::Codex,
        id: "thread-123".to_string(),
        short_id: None,
        title: "t".to_string(),
        cwd,
        created_at_ms: 0,
        updated_at_ms: 0,
        status: Status::Idle,
        hidden: false,
        source_label: "test".to_string(),
        summary: String::new(),
        companion: false,
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
    }
}

#[test]
fn attach_command_pins_existing_cwd() {
    let dir = tempfile::TempDir::new().unwrap();
    let backend = CodexBackend::new(PathBuf::from("/tmp/does-not-matter"));
    let session = session_with_cwd(dir.path().to_path_buf());
    let cmd = backend.attach_command(&session).expect("codex supports attach");
    assert_eq!(cmd.get_current_dir(), Some(dir.path()));
}

#[test]
fn attach_command_leaves_cwd_unset_when_dir_missing() {
    let backend = CodexBackend::new(PathBuf::from("/tmp/does-not-matter"));
    let session = session_with_cwd(PathBuf::from("/nonexistent/deleted-session-dir"));
    let cmd = backend.attach_command(&session).expect("codex supports attach");
    // A deleted cwd must not be set, otherwise spawning the pty command would fail.
    assert_eq!(cmd.get_current_dir(), None);
}
