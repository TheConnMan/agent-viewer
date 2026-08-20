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

#[cfg(target_os = "linux")]
mod agent_runner_contracts {
    use agent_viewer_core::agent_runner::AgentRunnerBackend;
    use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Status};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    const THREAD_ID: &str = "019fce12-3456-789a-bcde-f0123456789a";
    const TRANSCRIPT_SENTINEL: &str = "PRIVATE TRANSCRIPT MUST NEVER RENDER";
    static AGENT_RUNNER_COMMAND_LOCK: Mutex<()> = Mutex::new(());

    fn command_environment_guard() -> MutexGuard<'static, ()> {
        AGENT_RUNNER_COMMAND_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_executable(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write fake executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("make fake executable runnable");
    }

    fn run(
        run_id: &str,
        runner: &str,
        provider: &str,
        status: &str,
        native_session_id: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "run_id": run_id,
            "request_id": format!("request-{run_id}"),
            "runner": runner,
            "provider": provider,
            "native_session_id": native_session_id,
            "native_turn_id": null,
            "snapshot_id": format!("snapshot-{run_id}"),
            "status": status,
            "outcome": {
                "kind": "succeeded",
                "private_transcript": TRANSCRIPT_SENTINEL
            },
            "submitted_at": "2026-08-05T00:00:00Z",
            "started_at": "2026-08-05T00:00:01Z",
            "finished_at": "2026-08-05T00:01:00Z",
            "updated_at": "2026-08-05T00:01:00Z"
        })
    }

    fn list_response(runs: Vec<serde_json::Value>) -> String {
        list_response_with_cursor(runs, None)
    }

    fn list_response_with_cursor(
        runs: Vec<serde_json::Value>,
        next_before: Option<&str>,
    ) -> String {
        serde_json::json!({
            "schema_version": 1,
            "ok": true,
            "data": {
                "runs": runs,
                "next_before": next_before
            }
        })
        .to_string()
    }

    fn backend_for_list(response: &str) -> (tempfile::TempDir, AgentRunnerBackend) {
        let dir = tempfile::TempDir::new().expect("fake command directory");
        let runner = dir.path().join("agent-runner");
        write_executable(
            &runner,
            &format!(
                "#!/bin/sh\n[ \"$*\" = \"--json run list --status reviewable --limit 200\" ] || exit 91\nprintf '%s\\n' '{}'\n",
                response.replace('\'', "'\\''")
            ),
        );
        let backend = AgentRunnerBackend::with_binary(runner);
        (dir, backend)
    }

    #[test]
    fn discovery_lists_only_reviewable_retained_kubernetes_codex_runs() {
        let _guard = command_environment_guard();
        let response = list_response(vec![
            run(
                "eligible",
                "kubernetes",
                "codex",
                "reviewable",
                Some(THREAD_ID),
            ),
            run(
                "still-running",
                "kubernetes",
                "codex",
                "running",
                Some(THREAD_ID),
            ),
            run(
                "wrong-runner",
                "local",
                "codex",
                "reviewable",
                Some(THREAD_ID),
            ),
            run(
                "wrong-provider",
                "kubernetes",
                "claude",
                "reviewable",
                Some(THREAD_ID),
            ),
            run(
                "missing-native-thread",
                "kubernetes",
                "codex",
                "reviewable",
                None,
            ),
        ]);
        let (_dir, mut backend) = backend_for_list(&response);

        let sessions = backend.list().expect("list retained runs");

        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.backend, BackendKind::AgentRunner);
        assert_eq!(session.id, "eligible");
        assert_eq!(session.status, Status::Done);
        assert!(session.cwd.as_os_str().is_empty());
        assert_eq!(session.rollout_path, None);
        assert!(!session.summary.contains(TRANSCRIPT_SENTINEL));
        assert!(
            backend
                .tail(session, usize::MAX)
                .expect("runner has no transcript surface")
                .is_empty(),
            "Agent Runner discovery must not turn public outcome data into a transcript"
        );
    }

    #[test]
    fn discovery_filters_reviewable_runs_at_the_controller_and_reads_every_page() {
        let _guard = command_environment_guard();
        let dir = tempfile::TempDir::new().expect("fake command directory");
        let runner = dir.path().join("agent-runner");
        let log = dir.path().join("runner.log");
        let first = list_response_with_cursor(
            vec![run(
                "newer-eligible",
                "kubernetes",
                "codex",
                "reviewable",
                Some(THREAD_ID),
            )],
            Some("cursor-for-older-runs"),
        );
        let second = list_response_with_cursor(
            vec![run(
                "older-eligible",
                "kubernetes",
                "codex",
                "reviewable",
                Some(THREAD_ID),
            )],
            None,
        );
        write_executable(
            &runner,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  \"--json run list --status reviewable --limit 200\") printf '%s\\n' '{}' ;;\n  \"--json run list --status reviewable --limit 200 --before cursor-for-older-runs\") printf '%s\\n' '{}' ;;\n  *) exit 91 ;;\nesac\n",
                log.display(),
                first.replace('\'', "'\\''"),
                second.replace('\'', "'\\''"),
            ),
        );
        let mut backend = AgentRunnerBackend::with_binary(runner);

        let sessions = backend.list().expect("list every reviewable page");

        assert_eq!(
            sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["newer-eligible", "older-eligible"]
        );
        assert!(
            sessions
                .iter()
                .all(|session| !session.summary.contains(TRANSCRIPT_SENTINEL))
        );
        assert_eq!(
            std::fs::read_to_string(log).expect("read list command log"),
            concat!(
                "--json run list --status reviewable --limit 200\n",
                "--json run list --status reviewable --limit 200 --before cursor-for-older-runs\n"
            )
        );
    }

    #[test]
    fn agent_runner_rows_are_attach_only_and_have_no_synthetic_conversation_surface() {
        let _guard = command_environment_guard();
        let response = list_response(vec![run(
            "eligible",
            "kubernetes",
            "codex",
            "reviewable",
            Some(THREAD_ID),
        )]);
        let (_dir, mut backend) = backend_for_list(&response);
        let session = backend.list().unwrap().remove(0);

        assert_eq!(
            backend.capabilities_for(&session),
            Capabilities {
                attach: true,
                ..Capabilities::none()
            }
        );
        assert!(backend.available_models().is_empty());
        assert!(backend.tail(&session, 12).unwrap().is_empty());
        assert!(
            backend
                .turn_activity(&session, std::time::Duration::MAX)
                .unwrap()
                .is_empty()
        );
        assert!(
            backend
                .spawn(Path::new("/tmp"), "task", None, None)
                .is_err()
        );
        assert!(backend.stop(&session).is_err());
        assert!(backend.remove(&session).is_err());
        assert!(backend.rename(&session, "name").is_err());
        assert!(backend.hide(&session.id).is_err());
        assert!(backend.unhide(&session.id).is_err());
    }

    #[test]
    fn an_unavailable_controller_is_a_bounded_listing_failure() {
        let _guard = command_environment_guard();
        let dir = tempfile::TempDir::new().expect("fake command directory");
        let runner = dir.path().join("agent-runner");
        write_executable(
            &runner,
            "#!/bin/sh\nprintf '%s\\n' '{\"schema_version\":1,\"ok\":false,\"error\":{\"code\":\"controller_unavailable\",\"message\":\"controller socket unavailable\",\"details\":{}}}' >&2\nexit 3\n",
        );
        let mut backend = AgentRunnerBackend::with_binary(runner);

        let error = backend
            .list()
            .expect_err("controller failure must be visible");
        let message = error.to_string();
        assert!(message.contains("controller_unavailable"));
        assert!(message.contains("controller socket unavailable"));
        assert!(!message.contains('\n') && message.len() < 512);
    }

    #[test]
    fn a_missing_agent_runner_binary_lists_empty_without_affecting_other_backends() {
        let _guard = command_environment_guard();
        let mut backend =
            AgentRunnerBackend::with_binary(PathBuf::from("/definitely/missing/agent-runner"));

        assert!(backend.list().expect("missing optional backend").is_empty());
    }
}
