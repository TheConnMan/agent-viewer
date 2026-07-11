//! Real-PTY integration tests (tests 27-30). A real kernel pty + real child processes,
//! no network, deliberately NOT #[ignore] — they must stay fast. The winsize-at-open
//! gotcha is load-bearing: a 0x0 pty renders nothing (see memory
//! pty-tui-testing-needs-winsize), so test 27 fails if the winsize is not set.

use agent_viewer_core::pty::{PtySession, PtySpec};
use std::time::{Duration, Instant};

fn spec(program: &str, args: &[&str], rows: u16, cols: u16) -> PtySpec {
    PtySpec {
        program: program.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: None,
        rows,
        cols,
    }
}

/// Poll a screen predicate for up to `timeout`.
fn wait_for_screen(session: &PtySession, timeout: Duration, needle: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if session.with_screen(|s| s.contents()).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn pty_captures_output() {
    let mut session =
        PtySession::spawn(spec("sh", &["-c", "printf hello-pty; sleep 30"], 24, 80))
            .expect("spawn pty");
    assert!(
        wait_for_screen(&session, Duration::from_secs(5), "hello-pty"),
        "expected 'hello-pty' to render through the vt100 parser"
    );
    session.kill();
}

#[test]
fn pty_write_input_echoes() {
    let mut session = PtySession::spawn(spec("cat", &[], 24, 80)).expect("spawn pty");
    session.write_input(b"abc").expect("write input");
    assert!(
        wait_for_screen(&session, Duration::from_secs(5), "abc"),
        "expected the pty echo of 'abc'"
    );
    session.kill();
}

#[test]
fn pty_resize_applies() {
    let mut session =
        PtySession::spawn(spec("sh", &["-c", "sleep 30"], 24, 80)).expect("spawn pty");
    session.resize(30, 100).expect("resize");
    assert_eq!(session.with_screen(|s| s.size()), (30, 100));
    session.kill();
}

#[test]
fn pty_kill_returns_when_grandchild_holds_slave() {
    // A grandchild that escapes the session (setsid) keeps the pty slave open after the
    // direct child is reaped and survives the controlling-process SIGHUP, so the master
    // reader never EOFs and a naive join() hangs forever. kill() must SIGKILL the whole
    // process group so the slave closes and the reader unblocks. Run kill() on a helper
    // thread so a regression surfaces as a timeout, not a hang.
    let mut session = PtySession::spawn(spec(
        "sh",
        &["-c", "setsid sleep 30 & exec sleep 30"],
        24,
        80,
    ))
    .expect("spawn pty");
    // Let the shell fork the escaping grandchild and exec into the foreground sleep.
    std::thread::sleep(Duration::from_millis(300));

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        session.kill();
        let _ = tx.send(());
    });
    // The invariant is "returns promptly, not never"; a loose bound avoids flaking under
    // parallel workspace runs while still catching a genuine hang (which never returns).
    assert!(
        rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "kill() did not return within 5s — reader join hung on a grandchild holding the pty slave"
    );
}

#[test]
fn pty_kill_after_reap_never_signals() {
    // is_exited() reaps the child via try_wait; a later kill() must NOT re-signal the
    // (now potentially recycled) numeric pid. The `exited` latch guards every signal
    // path in kill() by construction — see pty.rs::kill, whose group-SIGKILL and
    // child.kill() both sit behind `if !self.exited`. Here we assert the observable half:
    // once is_exited() latches, kill() returns promptly without panicking.
    let mut session =
        PtySession::spawn(spec("sh", &["-c", "exit 0"], 24, 80)).expect("spawn pty");

    let start = Instant::now();
    while !session.is_exited() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "child `exit 0` never observed exited"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // kill() on a reaped session takes the no-signal path; run it on a helper thread so a
    // regression (a hang) surfaces as a timeout rather than blocking the test forever.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        session.kill();
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "kill() after reap did not return promptly"
    );
}

#[test]
fn pty_child_survives_detach_dies_on_drop() {
    let mut session =
        PtySession::spawn(spec("sleep", &["30"], 24, 80)).expect("spawn pty");
    let pid = session.pid().expect("child pid");
    let proc_path = format!("/proc/{pid}");

    // Detach semantics = ownership without I/O: the child stays alive while held.
    std::thread::sleep(Duration::from_millis(500));
    assert!(!session.is_exited(), "child should survive detach");
    assert!(
        std::path::Path::new(&proc_path).exists(),
        "child pid must still be alive after detach"
    );

    // Dropping the session kills + reaps the child.
    drop(session);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if !std::path::Path::new(&proc_path).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("child pid {pid} still alive 2s after drop");
}
