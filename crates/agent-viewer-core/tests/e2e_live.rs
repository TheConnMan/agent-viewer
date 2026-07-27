//! Live end-to-end tests. Both are #[ignore] — they need real codex auth + network
//! (and, for the smoke, whatever backends exist on the box), so plain `cargo test` skips
//! them. Run explicitly:
//!   cargo test -p agent-viewer-core --test e2e_live -- --ignored --nocapture

use agent_viewer_core::backend::{Backend, Status, all_backends};
use agent_viewer_core::claude::ClaudeBackend;
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::codex::app_server::{
    ensure_daemon, interrupt_thread, probe_daemon, remote_endpoint, spawn_thread,
};
use agent_viewer_core::codex::status::open_rollout_paths;
use agent_viewer_core::default_codex_home;
use agent_viewer_core::pty::{PtySession, spec_from_command};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Poll `list()` until `pred` matches a session or the deadline passes.
fn poll_until<F>(backend: &mut CodexBackend, timeout: Duration, mut pred: F) -> Option<()>
where
    F: FnMut(&agent_viewer_core::Session) -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(sessions) = backend.list()
            && sessions.iter().any(&mut pred)
        {
            return Some(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// Poll `list()` until a session matches `pred`, returning a clone of it.
fn poll_session<F>(
    backend: &mut CodexBackend,
    timeout: Duration,
    mut pred: F,
) -> Option<agent_viewer_core::Session>
where
    F: FnMut(&agent_viewer_core::Session) -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(sessions) = backend.list()
            && let Some(found) = sessions.iter().find(|s| pred(s))
        {
            return Some(found.clone());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// The regression guard for the read-only-sandbox bug: a viewer-spawned session must be able
/// to write `.git`, which is what branching, worktrees, and commits all require. Under the old
/// `--sandbox workspace-write` this failed with "Read-only file system" while the run still
/// reported success, so the failure mode is a SILENT no-op — only a real spawn catches it.
/// Asserts on the git state the session leaves behind, never on what the agent says it did.
#[test]
#[ignore = "live: spawns a real codex exec (auth + network)"]
fn codex_spawned_session_can_write_git_metadata() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().to_path_buf();
    for args in [
        vec!["init"],
        vec!["config", "user.email", "e2e@example.com"],
        vec!["config", "user.name", "e2e"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .status()
                .expect("run git")
                .success(),
            "git {args:?} failed"
        );
    }
    // A commit so HEAD resolves and `git branch` has something to point at.
    std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
    for args in [vec!["add", "seed.txt"], vec!["commit", "-m", "seed"]] {
        assert!(
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .status()
                .expect("run git")
                .success(),
            "git {args:?} failed"
        );
    }

    let backend = CodexBackend::new(default_codex_home());
    backend
        .spawn(
            &repo,
            "Run exactly this one shell command and then stop: \
             git checkout -b sandbox-probe. Do not do anything else.",
            None,
        )
        .expect("spawn codex exec");

    // Poll git itself for the branch. The agent's own summary is not evidence — under the old
    // sandbox it cheerfully reported completion while having written nothing.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut created = false;
    while Instant::now() < deadline {
        let out = std::process::Command::new("git")
            .args(["branch", "--list", "sandbox-probe"])
            .current_dir(&repo)
            .output()
            .expect("run git branch");
        if !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
            created = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        created,
        "spawned session could not create a branch — the spawn path is sandboxed again \
         (check SANDBOX_ARGS / SPEC.md 'Spawn sandbox posture')"
    );
    println!("[e2e] viewer-spawned session created branch sandbox-probe");
}

#[test]
#[ignore = "live: spawns a real codex exec (auth + network)"]
fn codex_spawn_running_then_done() {
    // 1. Temp project dir with a git repo (codex exec expects one).
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().to_path_buf();
    let ok = std::process::Command::new("git")
        .arg("init")
        .current_dir(&repo)
        .status()
        .expect("run git init")
        .success();
    assert!(ok, "git init failed");

    let mut backend = CodexBackend::new(default_codex_home());

    // 2. Spawn a trivial background codex exec.
    let spawn_at = Instant::now();
    backend
        .spawn(&repo, "Reply with exactly the word DONE.", None)
        .expect("spawn codex exec");

    // 3. Session appears in the list (spec ~1s, done-when ~2s; assert within 15s).
    let canon = std::fs::canonicalize(&repo).unwrap_or(repo.clone());
    let appeared = poll_until(&mut backend, Duration::from_secs(15), |s| {
        std::fs::canonicalize(&s.cwd).unwrap_or_else(|_| s.cwd.clone()) == canon
    });
    assert!(
        appeared.is_some(),
        "spawned session never appeared in list()"
    );
    println!("[e2e] session appeared after {:?}", spawn_at.elapsed());

    let id = {
        let sessions = backend.list().unwrap();
        sessions
            .into_iter()
            .find(|s| std::fs::canonicalize(&s.cwd).unwrap_or_else(|_| s.cwd.clone()) == canon)
            .map(|s| s.id)
            .expect("session id")
    };

    // 4. Observe Working (live /proc/fd correlation) within 20s of spawn.
    let working = poll_until(&mut backend, Duration::from_secs(20), |s| {
        s.id == id && s.status == Status::Working
    });
    assert!(
        working.is_some(),
        "never observed Working (the /proc/fd proof)"
    );
    println!("[e2e] observed Working after {:?}", spawn_at.elapsed());

    // 5. After the process exits, status flips to Done (task_complete in tail).
    let done = poll_until(&mut backend, Duration::from_secs(180), |s| {
        s.id == id && s.status == Status::Done
    });
    assert!(done.is_some(), "never flipped to Done");
    println!("[e2e] observed Done after {:?}", spawn_at.elapsed());

    // 6. hide/unhide round-trip, then leave hidden.
    backend.hide(&id).expect("hide");
    let hidden = poll_until(&mut backend, Duration::from_secs(10), |s| {
        s.id == id && s.hidden
    });
    assert!(hidden.is_some(), "hide did not take");
    backend.unhide(&id).expect("unhide");
    let unhidden = poll_until(&mut backend, Duration::from_secs(10), |s| {
        s.id == id && !s.hidden
    });
    assert!(unhidden.is_some(), "unhide did not take");
    backend.hide(&id).expect("re-hide (tidy)");
}

#[test]
#[ignore = "live: spawns a real codex exec + attaches an embedded PTY (auth + network)"]
fn embedded_attach_live() {
    // 1. Spawn a real cheap codex exec and wait for Done (reuses the v1 helper shape).
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().to_path_buf();
    let ok = std::process::Command::new("git")
        .arg("init")
        .current_dir(&repo)
        .status()
        .expect("run git init")
        .success();
    assert!(ok, "git init failed");

    let mut backend = CodexBackend::new(default_codex_home());
    backend
        .spawn(&repo, "Reply with exactly the word DONE.", None)
        .expect("spawn codex exec");

    let canon = std::fs::canonicalize(&repo).unwrap_or(repo.clone());
    let session = poll_session(&mut backend, Duration::from_secs(180), |s| {
        std::fs::canonicalize(&s.cwd).unwrap_or_else(|_| s.cwd.clone()) == canon
            && s.status == Status::Done
    })
    .expect("session never reached Done");

    // 2. attach_command -> spec (24x80) -> PtySession::spawn (a real `codex resume`).
    let command = backend
        .attach_command(&session)
        .expect("codex supports attach");
    let spec = spec_from_command(&command, 24, 80);
    let mut pty = PtySession::spawn(spec).expect("spawn embedded pty");

    // 3. Poll for a non-blank screen through the vt100 parser; print the proof line.
    let start = Instant::now();
    let mut first_line = String::new();
    while start.elapsed() < Duration::from_secs(20) {
        let contents = pty.with_screen(|s| s.contents());
        if let Some(line) = contents.lines().find(|l| !l.trim().is_empty()) {
            first_line = line.to_string();
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !first_line.is_empty(),
        "embedded PTY screen stayed blank (no vt100 output observed)"
    );
    println!("[e2e] first screen line via vt100: {first_line:?}");

    // 4. Detach semantics: held, no I/O, 1s -> child still alive.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        !pty.is_exited(),
        "child died before drop (should survive detach)"
    );
    let pid = pty.pid().expect("child pid");

    // 5. Drop -> child gone within 2s.
    drop(pty);
    let start = Instant::now();
    let mut gone = false;
    while start.elapsed() < Duration::from_secs(2) {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(gone, "embedded PTY child {pid} outlived the session drop");
}

#[test]
#[ignore = "live: lists whatever backends exist on this box (non-gating smoke)"]
fn multi_backend_smoke() {
    // Non-gating: build the full roster, list each, print counts + sample rows.
    // Asserts only Ok(), never counts, so it stays cheap and non-flaky.
    for mut backend in all_backends() {
        let kind = backend.kind();
        let sessions = backend.list().expect("backend list must be Ok in smoke");
        println!("[smoke] {:?}: {} sessions", kind, sessions.len());
        for s in sessions.iter().take(3) {
            let _cwd: &PathBuf = &s.cwd;
            println!("[smoke]   {} [{:?}] {}", kind.tag(), s.origin, s.title);
        }
    }
}

/// Live proof of the real rename channel: renaming a claude bg job must write that job's own
/// `state.json`, and `claude agents` must echo the new name back on the next listing. This is
/// the load-bearing check that agent-viewer and `claude agents` cannot disagree about a name.
///
/// Picks a FINISHED job (never a live one, so no worker is racing the write), snapshots the
/// whole state file (bytes AND mode), renames, re-lists, then restores that snapshot before
/// asserting - so a failure leaves the box exactly as it found it. Restoring the raw bytes
/// rather than re-renaming matters: rename stamps `nameSource: "user"`, so a second rename
/// would permanently disable claude's auto-titler on a job that had been auto-named.
/// Skips cleanly when no finished claude job exists.
#[test]
#[ignore = "live: needs claude on PATH with at least one finished bg job"]
fn claude_live_rename_round_trips_through_the_agents_listing() {
    let mut backend = ClaudeBackend::new();
    let Ok(sessions) = backend.list() else {
        eprintln!("[skip] claude backend not listable on this box");
        return;
    };
    // Finished + has the short id that names its job dir + no live pid: nothing else is
    // writing this state.json while we do.
    let target = sessions
        .iter()
        .find(|s| {
            matches!(s.status, Status::Done | Status::Error)
                && s.pid.is_none()
                && s.short_id.as_deref().is_some_and(|id| !id.is_empty())
        })
        .cloned();
    let Some(session) = target else {
        eprintln!("[skip] no finished claude bg job to rename");
        return;
    };
    let original = session.title.clone();
    let probe = "e2e-live-rename-probe";
    let short = session.short_id.clone().expect("filtered on short id");
    let state_path = agent_viewer_core::claude::job_state_path_in(
        &agent_viewer_core::claude::default_jobs_root(),
        &short,
    );
    let snapshot = std::fs::read(&state_path).expect("state.json readable");
    let snapshot_mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&state_path)
            .expect("metadata")
            .permissions(),
    );
    println!("[rename] target job {short} title={original:?} mode={snapshot_mode:o}");

    let renamed = backend.rename(&session, probe);
    let seen_after_rename = renamed.as_ref().ok().and_then(|()| {
        backend
            .list()
            .ok()?
            .into_iter()
            .find(|s| s.id == session.id)
            .map(|s| s.title)
    });
    let mode_after_rename = std::fs::metadata(&state_path)
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()))
        .unwrap_or(0);
    // Restore the exact bytes AND mode FIRST, then assert, so an assertion failure never
    // leaves the probe name, a stamped nameSource, or a widened mode behind. `fs::write`
    // truncates in place and so keeps whatever mode the file currently has, which is exactly
    // the wrong thing if the implementation under test just widened it.
    std::fs::write(&state_path, &snapshot).expect("restore state.json");
    std::fs::set_permissions(
        &state_path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(snapshot_mode),
    )
    .expect("restore state.json mode");
    let seen_after_restore = backend
        .list()
        .ok()
        .and_then(|all| all.into_iter().find(|s| s.id == session.id))
        .map(|s| s.title);
    println!("[rename] after rename: {seen_after_rename:?}, after restore: {seen_after_restore:?}");

    renamed.expect("rename of a finished bg job must succeed");
    assert_eq!(
        seen_after_rename.as_deref(),
        Some(probe),
        "`claude agents` must report the name agent-viewer wrote"
    );
    assert_eq!(
        mode_after_rename & 0o777,
        snapshot_mode & 0o777,
        "rename must not widen the real state file's permissions"
    );
    assert_eq!(
        seen_after_restore.as_deref(),
        Some(original.as_str()),
        "the original name must be back"
    );
    assert_eq!(
        std::fs::read(&state_path).expect("state.json readable"),
        snapshot,
        "the state file must be byte-identical to how the test found it"
    );
}

/// Live proof of the daemon spawn-and-join path, and of the guard that keeps it safe.
///
/// `codex exec` runs its app-server IN PROCESS, so an exec-spawned thread can never be joined
/// from outside: a foreign `codex resume` replays the rollout, finds it ends mid-turn, and
/// appends a synthesized `TurnAborted{Interrupted}` while the real session keeps running. Only
/// a thread hosted by the shared daemon can be joined, which is why spawn goes through it.
///
/// Asserts the four things that make that path safe, none of which can be checked without a
/// real daemon: the spawned thread shows up in `list()` and reaches Working; its row is
/// `daemon_hosted` with NO pid (the fd holder is the daemon, and a SIGTERM there would kill
/// every session it hosts); attach routes to `codex resume --remote unix://<sock>`; and
/// `interrupt_thread` leaves the daemon process alive. Skips cleanly when no daemon answers.
#[test]
#[ignore = "live: needs a reachable codex app-server daemon (auth + network)"]
fn codex_daemon_spawn_is_joinable_and_interrupt_spares_the_daemon() {
    let Some(daemon) = ensure_daemon() else {
        eprintln!("[skip] no codex app-server daemon reachable on this box");
        return;
    };
    println!("[daemon] socket {}", daemon.socket_path.display());

    // 1. Temp git repo (codex expects one).
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().to_path_buf();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .current_dir(&repo)
            .status()
            .expect("run git init")
            .success(),
        "git init failed"
    );

    // 2. Spawn a thread ON THE DAEMON. The task is deliberately slow enough that the turn is
    //    still in progress while the assertions below run.
    let thread_id = spawn_thread(
        &daemon,
        &repo,
        "Count from 1 to 40, printing one number per line, and nothing else.",
        None,
    )
    .expect("spawn a thread through the daemon");
    println!("[daemon] thread {thread_id}");

    // 3. The row appears in the ordinary listing and reaches Working.
    let mut backend = CodexBackend::new(default_codex_home());
    let session = poll_session(&mut backend, Duration::from_secs(30), |s| {
        s.id == thread_id && s.status == Status::Working
    })
    .expect("daemon-spawned thread never reached Working in list()");

    // 4. THE GUARD: the daemon holds this rollout's fd, so the row must carry no pid at all.
    assert!(
        session.daemon_hosted,
        "a thread spawned on the daemon must be flagged daemon_hosted"
    );
    assert_eq!(
        session.pid, None,
        "the daemon's pid must never be handed back as the session's killable pid"
    );

    // 5. The daemon's own pid, straight from the fd scan for this rollout.
    let rollout = session
        .rollout_path
        .clone()
        .expect("codex rows carry a rollout path");
    let canonical = std::fs::canonicalize(&rollout).unwrap_or(rollout);
    let owner = open_rollout_paths()
        .get(&canonical)
        .copied()
        .expect("something holds the live rollout fd");
    assert!(
        owner.daemon,
        "the fd holder for a daemon-hosted thread must be recognized as the app-server"
    );
    println!("[daemon] rollout fd held by app-server pid {}", owner.pid);

    // 6. Attach joins through this daemon's socket instead of a plain resume.
    let command = backend
        .attach_command(&session)
        .expect("a daemon-hosted row is attachable");
    let args: Vec<String> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        args.iter().any(|a| a == "--remote"),
        "attach must join the daemon, not fork a foreign resume: {args:?}"
    );
    assert!(
        args.iter().any(|a| *a == remote_endpoint(&daemon)),
        "attach must point at this daemon's unix:// socket: {args:?}"
    );

    // 7. Interrupt the turn (this is `stop` for a daemon-hosted row), then prove the daemon
    //    itself survived it. Killing the daemon here would take down every hosted session.
    interrupt_thread(&daemon, &thread_id).expect("interrupt the daemon-hosted turn");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        std::path::Path::new(&format!("/proc/{}", owner.pid)).exists(),
        "interrupt killed the app-server daemon (pid {}) - stop must never signal a \
         daemon-held pid",
        owner.pid
    );
    assert!(
        probe_daemon().is_some(),
        "the daemon must still answer its version probe after an interrupt"
    );
    println!(
        "[daemon] app-server pid {} survived the interrupt",
        owner.pid
    );

    // 8. The row must LEAVE Working. An interrupted turn writes neither `task_complete` nor a
    //    new `task_started`, so a tail rule that only knows those two markers leaves the row
    //    Working forever and the stop looks like it did nothing to the user.
    let stopped = poll_session(&mut backend, Duration::from_secs(30), |s| {
        s.id == thread_id && s.status != Status::Working
    })
    .expect("interrupted daemon-hosted row never left Working");
    println!(
        "[daemon] row settled at {:?} after the interrupt (daemon_hosted={}, pid={:?})",
        stopped.status, stopped.daemon_hosted, stopped.pid
    );
    assert!(
        stopped.daemon_hosted && stopped.pid.is_none(),
        "the stopped row is still daemon-hosted and still pidless"
    );

    // Tidy: keep the box's session list clean.
    let _ = backend.hide(&thread_id);
}

/// A real `codex exec` session must be classified as unjoinable on live data, and its attach
/// must come back as a refusal that asks for the read-only tail.
///
/// This is the row shape the classification defect produced. `codex exec` hosts its app-server
/// IN PROCESS, so it holds its own rollout fd and nothing outside can subscribe to its turn.
/// The old daemon test scored "holds more than one rollout" as the daemon, and an exec session
/// that spawns a subagent holds exactly two, which would have routed this row's attach to a
/// `--remote` join on a daemon that does not host it: the fabricated interrupt, from the very
/// path built to prevent it.
///
/// Deliberately spawned through `codex exec` directly rather than `backend.spawn`, because
/// spawn now refuses to create an unjoinable session at all.
#[test]
#[ignore]
fn codex_exec_row_is_unjoinable_and_falls_back_to_the_read_only_tail() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().to_path_buf();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .current_dir(&repo)
            .status()
            .expect("run git init")
            .success(),
        "git init failed"
    );

    // A turn long enough to still be running when the assertions land.
    let mut child = std::process::Command::new("codex")
        .arg("exec")
        .arg("--json")
        .arg("-C")
        .arg(&repo)
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg("Run the shell command `sleep 60` and then reply with the word DONE.")
        // stdin MUST be closed: with an inherited stdin, `codex exec` prints "Reading
        // additional input from stdin..." and blocks forever instead of running the prompt.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn codex exec");

    let mut backend = CodexBackend::new(default_codex_home());
    let canon = std::fs::canonicalize(&repo).unwrap_or_else(|_| repo.clone());
    let row = poll_session(&mut backend, Duration::from_secs(45), |s| {
        std::fs::canonicalize(&s.cwd).unwrap_or_else(|_| s.cwd.clone()) == canon
            && s.status == Status::Working
    });
    let row = match row {
        Some(row) => row,
        None => {
            let _ = child.kill();
            panic!("the exec session never showed up as Working");
        }
    };
    println!(
        "[exec] row: status={:?} daemon_hosted={} pid={:?}",
        row.status, row.daemon_hosted, row.pid
    );

    // The classification that everything downstream depends on: an exec session holds its OWN
    // rollout, so it is a signalable pid and NOT a daemon host, even with a daemon running.
    assert!(
        !row.daemon_hosted,
        "a codex exec session hosts itself and must never read as the daemon"
    );
    assert!(
        row.pid.is_some(),
        "an exec session's own pid is what makes it stoppable"
    );

    // And the answer to Enter on that row: refused (no fork), flagged for the live tail.
    let refusal = backend
        .attach_command(&row)
        .expect_err("a live exec turn must not be joined");
    println!("[exec] attach refusal: {}", refusal.reason);
    assert!(
        refusal.tail,
        "the refusal must ask for the read-only tail rather than dead-end the user"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = backend.hide(&row.id);
}
