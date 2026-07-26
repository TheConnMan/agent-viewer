//! attach_command wiring for the codex backend (no network): the resume command must be
//! pinned to the session cwd so `codex resume` does not prompt "Choose working directory".
//! Also covers the codex source -> SessionOrigin mapping and the per-row stop capability.

mod common;

use agent_viewer_core::backend::{
    Backend, BackendKind, Capabilities, Session, SessionOrigin, Status,
};
use agent_viewer_core::codex::CodexBackend;
use std::path::PathBuf;

fn session_with_cwd(cwd: PathBuf) -> Session {
    Session {
        backend: BackendKind::Codex,
        id: "thread-123".to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: "t".to_string(),
        cwd,
        git_branch: None,
        status: Status::Idle,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        summary: String::new(),
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
    let cmd = backend
        .attach_command(&session)
        .expect("codex supports attach");
    assert_eq!(cmd.get_current_dir(), Some(dir.path()));
}

#[test]
fn attach_command_leaves_cwd_unset_when_dir_missing() {
    let backend = CodexBackend::new(PathBuf::from("/tmp/does-not-matter"));
    let session = session_with_cwd(PathBuf::from("/nonexistent/deleted-session-dir"));
    let cmd = backend
        .attach_command(&session)
        .expect("codex supports attach");
    // A deleted cwd must not be set, otherwise spawning the pty command would fail.
    assert_eq!(cmd.get_current_dir(), None);
}

// --- source -> SessionOrigin mapping ---

const INSERT_COLS: &str = "INSERT INTO threads \
    (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, \
     sandbox_policy, approval_mode, archived, model, git_branch, first_user_message, \
     preview, created_at_ms, updated_at_ms) VALUES ";

fn thread_row(id: &str, source: &str, updated_ms: i64) -> String {
    format!(
        "{INSERT_COLS}\
         ('{id}','/nonexistent/sessions/{id}.jsonl',1,1,'{source}','openai',\
          '/home/user/proj','{id} Title','workspace-write','on-request',0,\
          'gpt-5','main','first msg','preview',1000,{updated_ms})"
    )
}

/// `source == exec` is the ONLY source that becomes SessionOrigin::Exec; every other
/// source (cli, vscode, a subagent JSON blob) is Interactive. Asserting both branches from
/// one listing kills a mapping that collapsed every row onto a single origin.
#[test]
fn codex_origin_is_exec_only_for_exec_source() {
    let schema = common::read_fixture("threads_schema.sql");
    let subagent_row = format!(
        "{INSERT_COLS}\
         ('t_sub','/nonexistent/sessions/t_sub.jsonl',1,1,'{{\"subagent\":\"review\"}}','openai',\
          '/home/user/proj','Sub Title','workspace-write','on-request',0,\
          'gpt-5','main','first msg','preview',1000,1000)"
    );
    let rows = [
        thread_row("t_exec", "exec", 4000),
        thread_row("t_cli", "cli", 3000),
        thread_row("t_vscode", "vscode", 2000),
        subagent_row,
    ];
    let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    let (dir, path) = common::temp_db(&schema, &refs);
    // The backend globs `state_*.sqlite` under its codex home, so give the temp DB that name.
    std::fs::rename(&path, dir.path().join("state_1.sqlite")).expect("rename temp db");

    let mut backend = CodexBackend::new(dir.path().to_path_buf());
    let sessions = backend.list().expect("list codex sessions");
    let origin = |id: &str| {
        sessions
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no session {id}"))
            .origin
    };

    assert_eq!(origin("t_exec"), SessionOrigin::Exec);
    assert_eq!(origin("t_cli"), SessionOrigin::Interactive);
    assert_eq!(origin("t_vscode"), SessionOrigin::Interactive);
    assert_eq!(origin("t_sub"), SessionOrigin::Interactive);
}

// --- per-row stop capability ---

fn session_with_pid(pid: Option<u32>) -> Session {
    let mut session = session_with_cwd(PathBuf::from("/tmp"));
    session.pid = pid;
    session
}

/// `stop` SIGTERMs `session.pid`, so a row without a pid has nothing to signal and
/// `CodexBackend::stop` returns Unsupported for it. `capabilities_for` must say so up
/// front, and it must narrow nothing else.
#[test]
fn codex_stop_capability_is_per_row_and_requires_a_pid() {
    let backend = CodexBackend::new(PathBuf::from("/tmp/does-not-matter"));
    let base = backend.capabilities();
    assert!(base.stop, "backend wide stop stays true");

    let with_pid = backend.capabilities_for(&session_with_pid(Some(4242)));
    let without_pid = backend.capabilities_for(&session_with_pid(None));
    assert!(with_pid.stop, "a row carrying a pid can be stopped");
    assert!(!without_pid.stop, "no pid means no process to signal");
    // stop is the only field capabilities_for may narrow.
    assert_eq!(
        Capabilities {
            stop: false,
            ..with_pid
        },
        Capabilities {
            stop: false,
            ..base
        }
    );
    assert_eq!(
        Capabilities {
            stop: false,
            ..without_pid
        },
        Capabilities {
            stop: false,
            ..base
        }
    );
}
