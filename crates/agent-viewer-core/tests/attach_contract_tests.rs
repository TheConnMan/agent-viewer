//! attach_command wiring for the claude and opencode backends (v2.1 contract, no network).
//! Claude splits live-vs-done; opencode drops the invalid `run -s -i` form. Commands are
//! built, not run, so we assert via std::process::Command getters.

use agent_viewer_core::backend::{Backend, BackendKind, Session, Status};
use agent_viewer_core::claude::ClaudeBackend;
use agent_viewer_core::opencode::OpencodeBackend;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

fn claude_session(short_id: Option<&str>, cwd: PathBuf, pid: Option<u32>, status: Status) -> Session {
    Session {
        backend: BackendKind::Claude,
        id: "sess-uuid-1234".to_string(),
        short_id: short_id.map(|s| s.to_string()),
        title: "t".to_string(),
        cwd,
        created_at_ms: 0,
        updated_at_ms: 0,
        status,
        hidden: false,
        source_label: "background".to_string(),
        summary: String::new(),
        companion: false,
        pid,
        rollout_path: None,
        pr_refs: Vec::new(),
    }
}

fn opencode_session(cwd: PathBuf) -> Session {
    Session {
        backend: BackendKind::Opencode,
        id: "ses_abc".to_string(),
        short_id: None,
        title: "t".to_string(),
        cwd,
        created_at_ms: 0,
        updated_at_ms: 0,
        status: Status::Done,
        hidden: false,
        source_label: "opencode".to_string(),
        summary: String::new(),
        companion: false,
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
    }
}

/// The value set for an env key via Command::env (Some(v)), or None if never set.
fn env_set(cmd: &std::process::Command, key: &str) -> Option<String> {
    for (k, v) in cmd.get_envs() {
        if k == OsStr::new(key) {
            return v.map(|v| v.to_string_lossy().into_owned());
        }
    }
    None
}

fn args_of(cmd: &std::process::Command) -> Vec<&OsStr> {
    cmd.get_args().collect()
}

// --- Claude: live session attaches into the agents view selecting the short id ---

#[test]
fn claude_attach_live_by_pid_uses_agents_select() {
    let home = std::env::var("HOME").expect("HOME set in test env");
    // A live bg session (pid present) -> `claude agents` with CLAUDE_AGENTS_SELECT=<short>,
    // run from $HOME (the agents view is not tied to the session cwd).
    let session = claude_session(Some("work0001"), PathBuf::from("/some/proj"), Some(111), Status::Working);
    let backend = ClaudeBackend::new();
    let cmd = backend.attach_command(&session).expect("claude supports attach");

    assert_eq!(cmd.get_program(), OsStr::new("claude"));
    assert_eq!(args_of(&cmd), vec![OsStr::new("agents")]);
    assert_eq!(env_set(&cmd, "CLAUDE_AGENTS_SELECT"), Some("work0001".to_string()));
    assert_eq!(cmd.get_current_dir(), Some(Path::new(&home)));
}

#[test]
fn claude_attach_live_by_status_without_pid_uses_agents_select() {
    let home = std::env::var("HOME").expect("HOME set in test env");
    // No pid, but a Working/NeedsInput status still counts as live -> agents view.
    let session = claude_session(Some("block001"), PathBuf::from("/some/proj"), None, Status::NeedsInput);
    let backend = ClaudeBackend::new();
    let cmd = backend.attach_command(&session).expect("claude supports attach");

    assert_eq!(args_of(&cmd), vec![OsStr::new("agents")]);
    assert_eq!(env_set(&cmd, "CLAUDE_AGENTS_SELECT"), Some("block001".to_string()));
    assert_eq!(cmd.get_current_dir(), Some(Path::new(&home)));
}

// --- Claude: finished session resumes by full id, pinned to its cwd when it survives ---

#[test]
fn claude_attach_done_resumes_in_existing_cwd() {
    let dir = tempfile::TempDir::new().unwrap();
    let session = claude_session(None, dir.path().to_path_buf(), None, Status::Done);
    let backend = ClaudeBackend::new();
    let cmd = backend.attach_command(&session).expect("claude supports attach");

    assert_eq!(cmd.get_program(), OsStr::new("claude"));
    assert_eq!(args_of(&cmd), vec![OsStr::new("-r"), OsStr::new("sess-uuid-1234")]);
    assert_eq!(cmd.get_current_dir(), Some(dir.path()));
    // The resume path never sets the agents-select env.
    assert_eq!(env_set(&cmd, "CLAUDE_AGENTS_SELECT"), None);
}

#[test]
fn claude_attach_done_leaves_cwd_unset_when_dir_missing() {
    let session = claude_session(None, PathBuf::from("/nonexistent/deleted-claude-dir"), None, Status::Stopped);
    let backend = ClaudeBackend::new();
    let cmd = backend.attach_command(&session).expect("claude supports attach");

    assert_eq!(args_of(&cmd), vec![OsStr::new("-r"), OsStr::new("sess-uuid-1234")]);
    // A deleted cwd must not be set, or spawning the resume command would fail.
    assert_eq!(cmd.get_current_dir(), None);
}

// --- Opencode: `opencode -s <id>` (the old `run -s <id> -i --dir` was invalid) ---

#[test]
fn opencode_attach_uses_session_flag_in_existing_cwd() {
    let dir = tempfile::TempDir::new().unwrap();
    let session = opencode_session(dir.path().to_path_buf());
    let backend = OpencodeBackend::new();
    let cmd = backend.attach_command(&session).expect("opencode supports attach");

    assert_eq!(cmd.get_program(), OsStr::new("opencode"));
    assert_eq!(args_of(&cmd), vec![OsStr::new("-s"), OsStr::new("ses_abc")]);
    assert_eq!(cmd.get_current_dir(), Some(dir.path()));
}

#[test]
fn opencode_attach_leaves_cwd_unset_when_dir_missing() {
    let session = opencode_session(PathBuf::from("/nonexistent/opencode-dir"));
    let backend = OpencodeBackend::new();
    let cmd = backend.attach_command(&session).expect("opencode supports attach");

    assert_eq!(args_of(&cmd), vec![OsStr::new("-s"), OsStr::new("ses_abc")]);
    assert_eq!(cmd.get_current_dir(), None);
}
