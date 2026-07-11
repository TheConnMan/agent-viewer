//! Live end-to-end tests. Both are #[ignore] — they need real codex auth + network
//! (and, for the smoke, whatever backends exist on the box), so plain `cargo test` skips
//! them. Run explicitly:
//!   cargo test -p agent-viewer-core --test e2e_live -- --ignored --nocapture

use agent_viewer_core::backend::{Backend, Status, all_backends};
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
    assert!(!pty.is_exited(), "child died before drop (should survive detach)");
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
            println!("[smoke]   {} [{}] {}", kind.tag(), s.source_label, s.title);
        }
    }
}
