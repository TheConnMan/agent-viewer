//! Live end-to-end tests. Both are #[ignore] — they need real codex auth + network
//! (and, for the smoke, whatever backends exist on the box), so plain `cargo test` skips
//! them. Run explicitly:
//!   cargo test -p agent-viewer-core --test e2e_live -- --ignored --nocapture

use agent_viewer_core::backend::{Backend, Status, all_backends};
use agent_viewer_core::claude::ClaudeBackend;
use agent_viewer_core::codex::CodexBackend;
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
