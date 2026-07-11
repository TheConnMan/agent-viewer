//! Live end-to-end tests. Both are #[ignore] — they need real codex auth + network
//! (and, for the smoke, whatever backends exist on the box), so plain `cargo test` skips
//! them. Run explicitly:
//!   cargo test -p agent-viewer-core --test e2e_live -- --ignored --nocapture

use agent_viewer_core::backend::{Backend, Status, all_backends};
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::default_codex_home;
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
        .spawn(&repo, "Reply with exactly the word DONE.")
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

    // 4. Observe Running (live /proc/fd correlation) within 20s of spawn.
    let running = poll_until(&mut backend, Duration::from_secs(20), |s| {
        s.id == id && s.status == Status::Running
    });
    assert!(
        running.is_some(),
        "never observed Running (the /proc/fd proof)"
    );
    println!("[e2e] observed Running after {:?}", spawn_at.elapsed());

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
