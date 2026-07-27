//! attach_command wiring for the claude and opencode backends (v2.1 contract, no network).
//! Claude splits live-vs-done; opencode drops the invalid `run -s -i` form. Commands are
//! built, not run, so we assert via std::process::Command getters.

use agent_viewer_core::backend::{Backend, BackendKind, Session, SessionOrigin, Status};
use agent_viewer_core::claude::ClaudeBackend;
use agent_viewer_core::opencode::OpencodeBackend;
use std::ffi::OsStr;
use std::path::PathBuf;

fn claude_session(
    short_id: Option<&str>,
    cwd: PathBuf,
    pid: Option<u32>,
    status: Status,
) -> Session {
    Session {
        backend: BackendKind::Claude,
        id: "sess-uuid-1234".to_string(),
        short_id: short_id.map(|s| s.to_string()),
        origin: SessionOrigin::Background,
        title: "t".to_string(),
        cwd,
        git_branch: None,
        status,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

fn opencode_session(cwd: PathBuf) -> Session {
    Session {
        backend: BackendKind::Opencode,
        id: "ses_abc".to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: "t".to_string(),
        cwd,
        git_branch: None,
        status: Status::Done,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
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

// --- Claude capability: native `claude rm` makes the remove action supported ---

#[test]
fn claude_capabilities_advertise_native_remove() {
    // `claude rm <short_id>` deletes a bg session (and its worktree), so remove is now a
    // real capability. The rest of the claude caps are unchanged by this.
    let caps = ClaudeBackend::new().capabilities();
    assert!(caps.delete, "claude advertises native rm as delete");
    assert!(caps.spawn);
    assert!(caps.attach);
    assert!(!caps.archive);
    assert!(!caps.stop);
    assert!(caps.needs_input);
    assert!(caps.pr_refs);
    assert!(caps.live_status);
    // Rename is a bg-job state.json write, gated per row on the short id (see rename_tests).
    assert!(caps.rename);
}

// --- Claude: `claude attach <short_id>` resumes the SAME thread, live OR done ---
// The old live-vs-done split (agents view + CLAUDE_AGENTS_SELECT for live, `-r` for done)
// is gone: with a short id present the command is always `claude attach <short_id>`, and it
// never sets CLAUDE_AGENTS_SELECT.

#[test]
fn claude_attach_live_with_short_id_uses_attach_subcommand() {
    // A live bg session (pid present, Working) attaches by short id -> `claude attach <short>`.
    let session = claude_session(
        Some("abc12345"),
        PathBuf::from("/some/proj"),
        Some(111),
        Status::Working,
    );
    let backend = ClaudeBackend::new();
    let cmd = backend
        .attach_command(&session)
        .expect("claude supports attach");

    assert_eq!(cmd.get_program(), OsStr::new("claude"));
    assert_eq!(
        args_of(&cmd),
        vec![OsStr::new("attach"), OsStr::new("abc12345")]
    );
    // The attach subcommand replaces the fragile agents-view preselect entirely.
    assert_eq!(env_set(&cmd, "CLAUDE_AGENTS_SELECT"), None);
}

#[test]
fn claude_attach_done_with_short_id_uses_attach_subcommand() {
    // A finished session with a short id takes the SAME `claude attach <short>` path -
    // `claude attach` wakes a done session, so there is no live-vs-done branch anymore.
    let session = claude_session(
        Some("abc12345"),
        PathBuf::from("/some/proj"),
        None,
        Status::Done,
    );
    let backend = ClaudeBackend::new();
    let cmd = backend
        .attach_command(&session)
        .expect("claude supports attach");

    assert_eq!(
        args_of(&cmd),
        vec![OsStr::new("attach"), OsStr::new("abc12345")]
    );
    assert_eq!(env_set(&cmd, "CLAUDE_AGENTS_SELECT"), None);
}

// --- Claude fallback: no short id -> resume by full id, pinned to cwd when it survives ---
// The fallback keys on the MISSING short id (None or ""), not on live-vs-done.

#[test]
fn claude_attach_without_short_id_resumes_in_existing_cwd() {
    let dir = tempfile::TempDir::new().unwrap();
    // short_id None (a rare row with no jobs "id" key) falls back to `claude -r <full id>`.
    let session = claude_session(None, dir.path().to_path_buf(), None, Status::Done);
    let backend = ClaudeBackend::new();
    let cmd = backend
        .attach_command(&session)
        .expect("claude supports attach");

    assert_eq!(cmd.get_program(), OsStr::new("claude"));
    assert_eq!(
        args_of(&cmd),
        vec![OsStr::new("-r"), OsStr::new("sess-uuid-1234")]
    );
    assert_eq!(cmd.get_current_dir(), Some(dir.path()));
    // The resume fallback never sets the agents-select env.
    assert_eq!(env_set(&cmd, "CLAUDE_AGENTS_SELECT"), None);
}

#[test]
fn claude_attach_empty_short_id_falls_back_and_leaves_cwd_unset() {
    // short_id Some("") is the same id-less case; a missing cwd must not be pinned.
    let session = claude_session(
        Some(""),
        PathBuf::from("/nonexistent/deleted-claude-dir"),
        None,
        Status::Done,
    );
    let backend = ClaudeBackend::new();
    let cmd = backend
        .attach_command(&session)
        .expect("claude supports attach");

    assert_eq!(
        args_of(&cmd),
        vec![OsStr::new("-r"), OsStr::new("sess-uuid-1234")]
    );
    // A deleted cwd must not be set, or spawning the resume command would fail.
    assert_eq!(cmd.get_current_dir(), None);
    assert_eq!(env_set(&cmd, "CLAUDE_AGENTS_SELECT"), None);
}

// --- Opencode: `opencode -s <id>` (the old `run -s <id> -i --dir` was invalid) ---

#[test]
fn opencode_attach_uses_session_flag_in_existing_cwd() {
    let dir = tempfile::TempDir::new().unwrap();
    let session = opencode_session(dir.path().to_path_buf());
    let backend = OpencodeBackend::new();
    let cmd = backend
        .attach_command(&session)
        .expect("opencode supports attach");

    assert_eq!(cmd.get_program(), OsStr::new("opencode"));
    assert_eq!(args_of(&cmd), vec![OsStr::new("-s"), OsStr::new("ses_abc")]);
    assert_eq!(cmd.get_current_dir(), Some(dir.path()));
}

#[test]
fn opencode_attach_leaves_cwd_unset_when_dir_missing() {
    let session = opencode_session(PathBuf::from("/nonexistent/opencode-dir"));
    let backend = OpencodeBackend::new();
    let cmd = backend
        .attach_command(&session)
        .expect("opencode supports attach");

    assert_eq!(args_of(&cmd), vec![OsStr::new("-s"), OsStr::new("ses_abc")]);
    assert_eq!(cmd.get_current_dir(), None);
}
