//! attach_command wiring for the codex backend (no network): the resume command must be
//! pinned to the session cwd so `codex resume` does not prompt "Choose working directory".
//! Also covers the codex source -> SessionOrigin mapping, the per-row stop capability, and
//! the pure spawn/attach/stop routing decisions for the app-server daemon path.

mod common;

use agent_viewer_core::backend::{
    Backend, BackendKind, Capabilities, Session, SessionOrigin, Status,
};
use agent_viewer_core::codex::app_server::{Daemon, remote_endpoint};
use agent_viewer_core::codex::{
    AttachRoute, CodexBackend, SpawnRoute, StopRoute, attach_route, spawn_route, stop_route,
};
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
        daemon_hosted: false,
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

// --- daemon routing: spawn, attach, stop ---
//
// The bug this section pins: `codex resume <id>` on a thread that is CURRENTLY RUNNING does
// not join it. The resuming process gets its own ThreadManager, sees no live thread, replays
// the rollout, finds it ends mid-turn and appends a synthesized
// `TurnAborted{reason: Interrupted}` -- which the TUI renders as "Conversation interrupted,
// tell the model what to do differently". Verified live on codex-cli 0.145.0: a foreign
// resume of a live thread reported `status: idle` with turn status "interrupted" while the
// real session kept running untouched. A real join exists only INSIDE the one app-server
// process that hosts the thread, reached with `codex resume --remote unix://<sock> <id>`.

/// The socket path from the `daemon_version.json` live capture.
const SOCKET: &str = "/home/user/.codex/app-server-control/app-server-control.sock";

fn daemon() -> Daemon {
    Daemon {
        socket_path: PathBuf::from(SOCKET),
    }
}

/// A codex row with the three fields every routing decision reads.
fn routed_session(status: Status, pid: Option<u32>, daemon_hosted: bool) -> Session {
    let mut session = session_with_cwd(PathBuf::from("/tmp"));
    session.status = status;
    session.pid = pid;
    session.daemon_hosted = daemon_hosted;
    session
}

#[test]
fn spawn_route_prefers_the_daemon_whenever_one_is_reachable() {
    // `codex exec` runs its app-server IN PROCESS, so an exec-spawned thread can never be
    // joined from outside no matter what attach does later. Spawning on the shared daemon is
    // the only way a spawned session stays joinable, so the daemon wins whenever it answers.
    assert_eq!(spawn_route(Some(&daemon())), SpawnRoute::Daemon);
    // With no daemon, exec is still better than not spawning at all.
    assert_eq!(spawn_route(None), SpawnRoute::Exec);
}

#[test]
fn attach_route_joins_a_daemon_hosted_thread_through_the_remote_socket() {
    // A thread the daemon hosts is joinable at any status: `thread/resume` inside that
    // process returns the history and atomically subscribes the caller to live updates (two
    // clients on one app-server received byte-identical event streams, no interrupt marker).
    for status in [
        Status::Working,
        Status::needs_input(),
        Status::Idle,
        Status::Done,
    ] {
        assert_eq!(
            attach_route(&routed_session(status.clone(), None, true), Some(&daemon())),
            AttachRoute::Remote(remote_endpoint(&daemon())),
            "daemon-hosted {status:?} must join over the socket"
        );
    }
}

#[test]
fn attach_route_refuses_a_foreign_live_turn_rather_than_fabricating_an_interrupt() {
    // Mid-turn (Working or NeedsInput) with a live pid we do not host: a plain resume would
    // append a synthesized TurnAborted to the rollout of a session that is still running, so
    // attaching is destructive and must be refused instead.
    for status in [
        Status::Working,
        Status::NeedsInput {
            reason: Some("awaiting approval".to_string()),
        },
    ] {
        let session = routed_session(status.clone(), Some(4242), false);
        // The refusal does not depend on whether a daemon happens to be up: this thread is
        // not hosted by it either way.
        for available in [Some(&daemon()), None] {
            match attach_route(&session, available) {
                AttachRoute::Refuse(reason) => assert!(
                    reason.to_lowercase().contains("interrupt"),
                    "the refusal must name the interrupt it is avoiding, got {reason:?}"
                ),
                other => panic!("mid-turn foreign thread must be refused, got {other:?}"),
            }
        }
    }
}

#[test]
fn attach_route_resumes_locally_when_there_is_no_live_turn_to_protect() {
    // Idle means the fd is held but the last turn completed, so a replay ends cleanly and
    // there is no interrupt to fabricate. Done/Error/Unknown have nothing running at all,
    // and a mid-turn row with NO pid means the scan found no process to join or protect.
    let cases = [
        (Status::Idle, Some(4242)),
        (Status::Working, None),
        (Status::needs_input(), None),
        (Status::Done, None),
        (Status::Error, None),
        (Status::Unknown, None),
    ];
    for (status, pid) in cases {
        assert_eq!(
            attach_route(&routed_session(status.clone(), pid, false), Some(&daemon())),
            AttachRoute::Local,
            "foreign {status:?} pid={pid:?} must take the plain resume"
        );
    }
}

#[test]
fn attach_route_opens_a_daemon_hosted_row_locally_when_no_daemon_answers() {
    // The row was hosted by a daemon that is now gone, so there is no live thread left to
    // join and none to interrupt either. Opening it read-only beats refusing the row.
    for status in [Status::Working, Status::Idle, Status::Done] {
        assert_eq!(
            attach_route(&routed_session(status.clone(), None, true), None),
            AttachRoute::Local,
            "daemon-hosted {status:?} with no daemon must still open"
        );
    }
}

#[test]
fn attach_command_refuses_a_live_foreign_turn_with_the_reason_verbatim() {
    // The wiring the TUI actually hits: a refused route surfaces as Err(AttachRefusal) whose
    // reason is shown verbatim in the footer, never as a command that quietly corrupts the
    // running session's rollout.
    let backend = CodexBackend::new(PathBuf::from("/tmp/does-not-matter"));
    let refusal = backend
        .attach_command(&routed_session(Status::Working, Some(4242), false))
        .expect_err("a live foreign turn must not be attachable");
    assert!(
        refusal.reason.to_lowercase().contains("interrupt"),
        "the footer notice must name the interrupt, got {:?}",
        refusal.reason
    );
}

#[test]
fn stop_route_never_signals_a_daemon_held_pid() {
    // THE GUARD. The daemon holds the rollout fd for every thread it hosts, so the
    // /proc/<pid>/fd scan reports the DAEMON's pid for those rows. SIGTERM there would kill
    // the daemon and every other session it hosts, so a daemon-hosted row is always stopped
    // with turn/interrupt over the socket -- including the belt-and-braces case where a pid
    // somehow rode along on the row.
    assert_eq!(
        stop_route(&routed_session(Status::Working, None, true)),
        StopRoute::Interrupt
    );
    assert_eq!(
        stop_route(&routed_session(Status::Working, Some(4242), true)),
        StopRoute::Interrupt,
        "daemon_hosted outranks any pid on the row"
    );
}

#[test]
fn stop_route_signals_only_a_pid_this_session_owns() {
    assert_eq!(
        stop_route(&routed_session(Status::Working, Some(4242), false)),
        StopRoute::Signal(4242)
    );
    // No pid and no daemon host: nothing to signal, nothing to interrupt.
    assert_eq!(
        stop_route(&routed_session(Status::Working, None, false)),
        StopRoute::Unsupported
    );
    assert_eq!(
        stop_route(&routed_session(Status::Done, None, false)),
        StopRoute::Unsupported
    );
}

#[test]
fn codex_advertises_stop_for_a_daemon_hosted_row_that_has_no_pid() {
    // A daemon-hosted row carries no pid BY DESIGN, so the old `stop = pid.is_some()` gate
    // would gray Ctrl+C out on exactly the rows that are interruptible. Stop stays advertised
    // for them (turn/interrupt), and a pidless foreign row stays unsupported.
    let backend = CodexBackend::new(PathBuf::from("/tmp/does-not-matter"));
    assert!(
        backend
            .capabilities_for(&routed_session(Status::Working, None, true))
            .stop,
        "a daemon-hosted row is stoppable over the socket"
    );
    assert!(
        !backend
            .capabilities_for(&routed_session(Status::Working, None, false))
            .stop,
        "no pid and no daemon host means nothing to stop"
    );
    assert!(
        backend
            .capabilities_for(&routed_session(Status::Working, Some(4242), false))
            .stop
    );
}

#[test]
fn codex_list_flags_daemon_hosted_only_when_the_daemon_holds_the_fd() {
    // Listing rows whose rollouts nobody has open: every row must come back with
    // daemon_hosted false and no pid.
    //
    // This half only pins the false direction, which a hardcoded `false` would also satisfy.
    // The true direction cannot be forced from here (it needs a real process holding a real
    // fd), so it is pinned by the pure resolver tests in `status_tests.rs`
    // (`resolver_withholds_the_daemon_pid_and_flags_the_row_daemon_hosted`) and end to end by
    // `e2e_live::codex_daemon_spawn_is_joinable_and_interrupt_spares_the_daemon`.
    let schema = common::read_fixture("threads_schema.sql");
    let rows = [
        thread_row("t_cli", "cli", 3000),
        thread_row("t_vscode", "vscode", 2000),
    ];
    let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    let (dir, path) = common::temp_db(&schema, &refs);
    std::fs::rename(&path, dir.path().join("state_1.sqlite")).expect("rename temp db");

    let mut backend = CodexBackend::new(dir.path().to_path_buf());
    let sessions = backend.list().expect("list codex sessions");
    assert_eq!(sessions.len(), 2);
    for session in &sessions {
        assert!(
            !session.daemon_hosted,
            "{} has no open fd at all, so nothing hosts it",
            session.id
        );
        assert_eq!(session.pid, None, "{} has no live process", session.id);
    }
}
