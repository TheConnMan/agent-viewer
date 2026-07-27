//! Real-PTY integration tests (tests 27-30). A real kernel pty + real child processes,
//! no network, deliberately NOT #[ignore] — they must stay fast. The winsize-at-open
//! gotcha is load-bearing: a 0x0 pty renders nothing (see memory
//! pty-tui-testing-needs-winsize), so test 27 fails if the winsize is not set.

use agent_viewer_core::pty::{PtySession, PtySpec, TerminalPalette};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn spec(program: &str, args: &[&str], rows: u16, cols: u16) -> PtySpec {
    PtySpec {
        program: program.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: None,
        envs: Vec::new(),
        rows,
        cols,
        palette: None,
    }
}

fn palette(foreground: [u8; 3], background: [u8; 3]) -> TerminalPalette {
    TerminalPalette {
        foreground,
        background,
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

fn sync_file(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "agent-viewer-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::write(&path, "").expect("create palette synchronization file");
    path
}

fn wait_for_sync(path: &std::path::Path, timeout: Duration, expected: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if std::fs::read_to_string(path).ok().as_deref() == Some(expected) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn pty_captures_output() {
    let mut session = PtySession::spawn(spec("sh", &["-c", "printf hello-pty; sleep 30"], 24, 80))
        .expect("spawn pty");
    assert!(
        wait_for_screen(&session, Duration::from_secs(5), "hello-pty"),
        "expected 'hello-pty' to render through the vt100 parser"
    );
    session.kill();
}

#[test]
fn pty_spawn_passes_env_from_command() {
    // The env set on a std Command must reach the PTY child (regression: spec_from_command
    // dropped envs, so CLAUDE_AGENTS_SELECT never reached the embedded `claude agents`).
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg("printf %s \"$SMOKE_VAR\"; sleep 30");
    cmd.env("SMOKE_VAR", "env-reached-child");
    let mut session = PtySession::spawn(agent_viewer_core::pty::spec_from_command(&cmd, 24, 80))
        .expect("spawn pty");
    assert!(
        wait_for_screen(&session, Duration::from_secs(5), "env-reached-child"),
        "expected the child to see SMOKE_VAR through the PTY"
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
    let mut session = PtySession::spawn(spec("sh", &["-c", "exit 0"], 24, 80)).expect("spawn pty");

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
    let mut session = PtySession::spawn(spec("sleep", &["30"], 24, 80)).expect("spawn pty");
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

#[test]
fn pty_answers_fragmented_palette_queries_with_configured_colors() {
    // This is deliberately a real child over a real kernel PTY. An out of band file plus
    // acknowledgement handshakes hold each partial query at a controlled boundary before
    // the child writes its suffix. Every boundary of every supported query form is covered.
    let sync_file = sync_file("palette-fragments");
    let script = r#"
        stty raw -echo

        foreground_reply=$'\033]10;rgb:1212/3434/5656\033\\'
        background_reply=$'\033]11;rgb:ABAB/CDCD/EFEF\033\\'

        check_splits() {
            query=$1
            expected=$2
            label=$3
            for split in $(seq 1 $((${#query} - 1))); do
                printf '%s' "${query:0:split}"
                printf "palette-split-%s-%s" "$label" "$split" > "$PALETTE_SYNC_FILE"
                IFS= read -r -n 1 acknowledgement
                [ "$acknowledgement" = a ] || exit 1
                printf '%s' "${query:split}"
                reply=$(dd bs=1 count=25 status=none)
                [ "$reply" = "$expected" ] || {
                    printf 'palette-split-bad-%s-%s\n' "$label" "$split"
                    exit 1
                }
            done
        }

        check_splits $'\033]10;?\a' "$foreground_reply" '10-bel'
        check_splits $'\033]10;?\033\\' "$foreground_reply" '10-st'
        check_splits $'\033]11;?\a' "$background_reply" '11-bel'
        check_splits $'\033]11;?\033\\' "$background_reply" '11-st'
        printf 'palette-splits-ok\n'

        query=$'\033]10;?\a'
        for byte in $(seq 0 $((${#query} - 2))); do
            printf '%s' "${query:byte:1}"
            printf "palette-byte-%s" "$byte" > "$PALETTE_SYNC_FILE"
            IFS= read -r -n 1 acknowledgement
            [ "$acknowledgement" = b ] || exit 1
        done
        printf '%s' "${query:$((${#query} - 1)):1}"
        reply=$(dd bs=1 count=25 status=none)
        [ "$reply" = "$foreground_reply" ] || {
            printf 'palette-byte-bad\n'
            exit 1
        }
        printf 'palette-bytes-ok\n'

        # Adjacent queries need distinct, ordered replies rather than a duplicated or lost slot.
        printf '%s%s' $'\033]10;?\a' $'\033]11;?\033\\'
        replies=$(dd bs=1 count=50 status=none)
        [ "$replies" = "$foreground_reply$background_reply" ] || {
            printf 'palette-adjacent-bad\n'
            exit 1
        }
        printf 'palette-adjacent-ok\n'

        # A false prefix must retain the later ESC suffix, producing exactly one reply.
        printf '%s' $'\033]10;?\033]10;?\a'
        reply=$(dd bs=1 count=25 status=none)
        [ "$reply" = "$foreground_reply" ] || {
            printf 'palette-retained-suffix-bad\n'
            exit 1
        }
        if IFS= read -r -t 0.25 -n 1 unexpected; then
            printf 'palette-duplicate-reply\n'
            exit 1
        fi
        printf 'palette-retained-suffix-ok\n'

        # Unsupported OSC, malformed input, and partial prefixes must not result in bytes
        # written to the child.
        printf '\033]12;?\a'
        if IFS= read -r -t 0.25 -n 1 unexpected; then
            printf 'palette-unrelated-reply\n'
            exit 1
        fi
        printf '\033]10;rgb:ffff/ffff/ffff\a'
        if IFS= read -r -t 0.25 -n 1 unexpected; then
            printf 'palette-malformed-reply\n'
            exit 1
        fi
        printf '\033]10;?'
        if IFS= read -r -t 0.25 -n 1 unexpected; then
            printf 'palette-partial-prefix-reply\n'
            exit 1
        fi
        # CAN aborts the unfinished OSC in the terminal parser without completing it.
        printf '\030'
        printf 'palette-ok\n'
    "#;
    let mut session = PtySession::spawn(PtySpec {
        palette: Some(palette([0x12, 0x34, 0x56], [0xab, 0xcd, 0xef])),
        envs: vec![(
            ("PALETTE_SYNC_FILE").to_string(),
            sync_file.to_string_lossy().into_owned(),
        )],
        ..spec("bash", &["-c", script], 24, 80)
    })
    .expect("spawn palette probe");

    for split in ["10-bel", "10-st", "11-bel", "11-st"] {
        let query_len = if split.ends_with("bel") { 7 } else { 8 };
        for position in 1..query_len {
            let ready = format!("palette-split-{split}-{position}");
            assert!(
                wait_for_sync(&sync_file, Duration::from_secs(5), &ready),
                "the child never held the {split} query at byte boundary {position}"
            );
            session
                .write_input(b"a")
                .expect("acknowledge palette query split");
        }
    }
    assert!(
        wait_for_screen(&session, Duration::from_secs(5), "palette-splits-ok"),
        "the child did not receive an exact reply for every supported query split"
    );
    for byte in 0..6 {
        let ready = format!("palette-byte-{byte}");
        assert!(
            wait_for_sync(&sync_file, Duration::from_secs(5), &ready),
            "the child never held one byte palette chunk {byte}"
        );
        session
            .write_input(b"b")
            .expect("acknowledge one byte palette chunk");
    }
    assert!(
        wait_for_screen(&session, Duration::from_secs(5), "palette-bytes-ok"),
        "the child did not receive an exact reply after one byte query chunks"
    );
    assert!(
        wait_for_screen(&session, Duration::from_secs(5), "palette-adjacent-ok"),
        "adjacent palette queries did not receive two exact ordered replies"
    );
    assert!(
        wait_for_screen(
            &session,
            Duration::from_secs(5),
            "palette-retained-suffix-ok"
        ),
        "a false prefix did not retain the next palette query suffix exactly once"
    );
    assert!(
        wait_for_screen(&session, Duration::from_secs(5), "palette-ok"),
        "the child did not receive exact palette replies or observed an unexpected reply: {}",
        session.with_screen(|screen| screen.contents())
    );
    assert!(
        !session
            .with_screen(|screen| screen.contents())
            .contains("palette-unrelated-reply"),
        "an unrelated or malformed OSC query received a reply"
    );
    assert!(
        !session
            .with_screen(|screen| screen.contents())
            .contains("palette-malformed-reply"),
        "a malformed palette OSC query received a reply"
    );
    assert!(
        !session
            .with_screen(|screen| screen.contents())
            .contains("palette-partial-prefix-reply"),
        "a partial palette prefix received a reply"
    );
    session.kill();
    std::fs::remove_file(sync_file).expect("remove palette synchronization file");
}

#[test]
fn pty_keeps_palette_replies_and_concurrent_input_frames_intact() {
    // A separate parent writer starts each input frame immediately after the acknowledgement
    // that lets the child complete its fragmented query. The child accepts either whole-frame
    // ordering but rejects any byte interleaving between the reply and an input frame.
    let sync_file = sync_file("concurrent-palette");
    let script = r#"
        stty raw -echo
        for index in 0 1 2 3 4 5 6 7; do
            frame="I$index"
            printf '\033]10;'
            printf 'concurrent-palette-ready-%s' "$index" > "$PALETTE_SYNC_FILE"
            IFS= read -r -n 1 acknowledgement
            [ "$acknowledgement" = a ] || exit 1
            printf '?\a'
            received=$(dd bs=1 count=27 status=none)
            reply=$'\033]10;rgb:0101/2323/4545\033\\'
            case "$received" in
                "$reply$frame"|"$frame$reply") ;;
                *)
                    printf 'concurrent-palette-bad-%s\n' "$index"
                    exit 1
                    ;;
            esac
            printf 'concurrent-palette-ok-%s\n' "$index"
        done
        printf 'concurrent-palette-complete-after-8-frames\n'
    "#;
    let session = Arc::new(Mutex::new(
        PtySession::spawn(PtySpec {
            palette: Some(palette([0x01, 0x23, 0x45], [0x67, 0x89, 0xab])),
            envs: vec![(
                ("PALETTE_SYNC_FILE").to_string(),
                sync_file.to_string_lossy().into_owned(),
            )],
            ..spec("bash", &["-c", script], 24, 80)
        })
        .expect("spawn concurrent palette probe"),
    ));
    let (frame_tx, frame_rx) = mpsc::channel::<String>();
    let writer_session = Arc::clone(&session);
    let writer = std::thread::spawn(move || {
        for frame in frame_rx {
            writer_session
                .lock()
                .expect("lock concurrent pty session")
                .write_input(frame.as_bytes())
                .expect("write concurrent input frame");
        }
    });

    for index in 0..8 {
        let ready = format!("concurrent-palette-ready-{index}");
        assert!(
            wait_for_sync(&sync_file, Duration::from_secs(5), &ready),
            "the child never emitted palette query {index}"
        );
        session
            .lock()
            .expect("lock concurrent pty session")
            .write_input(b"a")
            .expect("acknowledge concurrent palette query");
        frame_tx
            .send(format!("I{index}"))
            .expect("queue concurrent input frame");
    }
    drop(frame_tx);
    writer.join().expect("join concurrent input writer");

    assert!(
        wait_for_screen(
            &session.lock().expect("lock concurrent pty session"),
            Duration::from_secs(5),
            "concurrent-palette-complete-after-8-frames",
        ),
        "a palette reply or concurrent input frame was interleaved before all eight frames completed"
    );
    assert!(
        !session
            .lock()
            .expect("lock concurrent pty session")
            .with_screen(|screen| screen.contents())
            .contains("concurrent-palette-bad-"),
        "the child observed an interleaved palette reply and input frame"
    );
    session.lock().expect("lock concurrent pty session").kill();
    std::fs::remove_file(sync_file).expect("remove concurrent palette synchronization file");
}
