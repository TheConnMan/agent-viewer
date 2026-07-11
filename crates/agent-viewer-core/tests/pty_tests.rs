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
