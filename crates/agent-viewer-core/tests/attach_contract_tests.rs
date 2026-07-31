//! attach_command wiring for the claude and codex backends (v2.1 contract, no network).
//! Claude splits live-vs-done. Commands are built, not run, so we assert via
//! std::process::Command getters.

use agent_viewer_core::backend::{Backend, BackendKind, Session, SessionOrigin, Status};
use agent_viewer_core::claude::{
    ClaudeBackend, capabilities_for_platform as claude_capabilities_for_platform,
    capabilities_for_session_on_platform as claude_capabilities_for_session_on_platform,
};
use agent_viewer_core::codex::{
    AttachRoute, attach_route_for_platform,
    capabilities_for_platform as codex_capabilities_for_platform,
};
use agent_viewer_core::platform::Platform;
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
        subagent: false,
        summary: String::new(),
        pid,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

fn codex_session(status: Status) -> Session {
    Session {
        backend: BackendKind::Codex,
        id: "019fce12-3456-789a-bcde-f0123456789a".to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: "portable codex session".to_string(),
        cwd: PathBuf::from("/some/proj"),
        git_branch: None,
        status,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        subagent: false,
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

// --- Claude capability: native stop and delete mutations are supported ---

#[test]
fn claude_capabilities_advertise_native_mutations() {
    // Claude provides native stop and delete mutations for supported background sessions.
    let caps = claude_capabilities_for_platform(Platform::Linux);
    assert!(caps.delete, "claude advertises native rm as delete");
    assert!(caps.spawn);
    assert!(caps.attach);
    assert!(!caps.archive);
    assert!(caps.stop);
    // Rename is a bg-job state.json write, gated per row on the short id (see rename_tests).
    assert!(caps.rename);
}

#[test]
fn portable_codex_capabilities_disable_unsafe_process_actions() {
    for platform in [Platform::Macos, Platform::Windows] {
        let caps = codex_capabilities_for_platform(platform);
        assert!(!caps.spawn, "{platform:?} cannot safely spawn Codex");
        assert!(!caps.attach, "{platform:?} cannot safely attach Codex");
        assert!(!caps.stop, "{platform:?} cannot safely stop Codex");
        assert!(caps.rename);
        assert!(caps.archive);
        assert!(caps.delete);
    }
}

#[test]
fn windows_claude_capabilities_refuse_rename() {
    let caps = claude_capabilities_for_platform(Platform::Windows);
    assert!(!caps.rename);
    assert!(caps.spawn);
    assert!(caps.attach);
}

#[test]
fn portable_claude_delete_requires_a_finished_row_with_valid_short_id() {
    for platform in [Platform::Macos, Platform::Windows] {
        let live_with_pid = claude_session(
            Some("abc12345"),
            PathBuf::from("/some/proj"),
            Some(111),
            Status::Working,
        );
        assert!(
            !claude_capabilities_for_session_on_platform(platform, &live_with_pid).delete,
            "{platform:?} must not delete a row whose process cannot be terminated safely"
        );

        for status in [
            Status::Working,
            Status::NeedsInput { reason: None },
            Status::Idle,
            Status::Unknown,
        ] {
            let unresolved = claude_session(
                Some("abc12345"),
                PathBuf::from("/some/proj"),
                None,
                status.clone(),
            );
            assert!(
                !claude_capabilities_for_session_on_platform(platform, &unresolved).delete,
                "{platform:?} must not delete a {status:?} row without process evidence"
            );
        }

        for status in [Status::Done, Status::Error] {
            let finished =
                claude_session(Some("abc12345"), PathBuf::from("/some/proj"), None, status);
            assert!(
                claude_capabilities_for_session_on_platform(platform, &finished).delete,
                "{platform:?} may delete a finished row with a valid short id"
            );
        }

        for short_id in [None, Some("")] {
            let missing_id =
                claude_session(short_id, PathBuf::from("/some/proj"), None, Status::Done);
            assert!(
                !claude_capabilities_for_session_on_platform(platform, &missing_id).delete,
                "{platform:?} must not delete a finished row without a valid short id"
            );
        }
    }
}

#[test]
fn portable_codex_attach_refuses_when_completion_is_not_proven() {
    let session = codex_session(Status::Unknown);
    for platform in [Platform::Macos, Platform::Windows] {
        assert!(matches!(
            attach_route_for_platform(&session, None, platform),
            AttachRoute::Refuse(_)
        ));
    }
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
