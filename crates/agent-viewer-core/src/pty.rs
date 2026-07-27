//! The embedded-attach engine: a `PtySession` owning a real PTY + child + vt100 parser,
//! UI-free so it can be headless-integration-tested (spawn `sh`/`cat`) and reused by a
//! Phase-3 daemon. Stream B owns this module in full.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalPalette {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
}

#[derive(Debug, Clone, PartialEq)]
pub struct PtySpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    /// Extra env vars to set on the child (e.g. CLAUDE_AGENTS_SELECT). Applied ON TOP of the
    /// inherited environment — the child still sees the viewer's env plus these.
    pub envs: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    pub palette: Option<TerminalPalette>,
}

/// Read program/args/cwd/envs off a built `Command` via the stable getters
/// (get_program/get_args/get_current_dir/get_envs). rows/cols clamped to >= 1. Env entries
/// with a None value (an explicit unset) are dropped — only set-values carry over.
pub fn spec_from_command(cmd: &std::process::Command, rows: u16, cols: u16) -> PtySpec {
    PtySpec {
        program: cmd.get_program().to_string_lossy().into_owned(),
        args: cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        cwd: cmd.get_current_dir().map(|p| p.to_path_buf()),
        envs: cmd
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect(),
        rows: rows.max(1),
        cols: cols.max(1),
        palette: None,
    }
}

const PALETTE_QUERIES: [(&[u8], u8); 4] = [
    (b"\x1b]10;?\x07", 10),
    (b"\x1b]10;?\x1b\\", 10),
    (b"\x1b]11;?\x07", 11),
    (b"\x1b]11;?\x1b\\", 11),
];

#[derive(Default)]
struct PaletteQueryDetector {
    pending: Vec<u8>,
}

impl PaletteQueryDetector {
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut slots = Vec::new();
        for &byte in bytes {
            self.pending.push(byte);
            if let Some((_, slot)) = PALETTE_QUERIES
                .iter()
                .find(|(query, _)| *query == self.pending.as_slice())
            {
                slots.push(*slot);
                self.pending.clear();
                continue;
            }

            while !self.pending.is_empty()
                && !PALETTE_QUERIES
                    .iter()
                    .any(|(query, _)| query.starts_with(self.pending.as_slice()))
            {
                self.pending.remove(0);
            }
        }
        slots
    }
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

fn write_frame(writer: &SharedWriter, bytes: &[u8]) -> std::io::Result<()> {
    let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
    writer.write_all(bytes)?;
    writer.flush()
}

#[cfg(test)]
fn write_frame_after_lock_boundary(
    writer: &SharedWriter,
    bytes: &[u8],
    at_lock_boundary: impl FnOnce(),
) -> std::io::Result<()> {
    at_lock_boundary();
    write_frame(writer, bytes)
}

fn palette_reply(slot: u8, color: [u8; 3]) -> String {
    let [red, green, blue] = color;
    format!("\x1b]{slot};rgb:{red:02X}{red:02X}/{green:02X}{green:02X}/{blue:02X}{blue:02X}\x1b\\")
}

/// A live PTY session: master pty, child, writer, vt100 parser fed by a reader thread.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: SharedWriter,
    parser: Arc<Mutex<vt100::Parser>>,
    reader_thread: Option<JoinHandle<()>>,
    /// True once the child has been reaped (via is_exited or kill). After reap the
    /// numeric pid may be recycled by an unrelated process, so NO signal path (group
    /// SIGKILL or child.kill) may run once this is set — it would hit a stranger.
    exited: bool,
}

impl PtySession {
    /// Open a portable-pty at rows x cols (winsize set AT OPEN — a 0x0 pty renders
    /// nothing; see memory pty-tui-testing-needs-winsize), spawn the command on the
    /// slave, detach a reader thread feeding vt100::Parser::new(rows, cols, 0). The
    /// child gets the pty as its controlling terminal (portable-pty default).
    pub fn spawn(spec: PtySpec) -> Result<PtySession> {
        let rows = spec.rows.max(1);
        let cols = spec.cols.max(1);
        let palette = spec.palette;

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Command(format!("openpty: {e}")))?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        for (key, value) in &spec.envs {
            cmd.env(key, value);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| Error::Command(format!("spawn {}: {e}", spec.program)))?;

        // Drop our slave handle so the child is the sole owner: when it exits the pty
        // slave closes and the master reader unblocks (EOF/EIO), letting the reader
        // thread — and thus Drop — finish without hanging.
        drop(pair.slave);

        // If wiring up the master fails after the child is already running, kill+reap it
        // on the way out so a bailed spawn does not leak a live child.
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Command(format!("clone reader: {e}")));
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => Arc::new(Mutex::new(writer)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Command(format!("take writer: {e}")));
            }
        };

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let reader_parser = Arc::clone(&parser);
        let reader_writer = Arc::clone(&writer);
        let reader_thread = std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            let mut palette_queries = PaletteQueryDetector::default();
            let mut replies_enabled = palette.is_some();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: slave closed
                    Ok(n) => {
                        if let Ok(mut p) = reader_parser.lock() {
                            p.process(&buf[..n]);
                        }
                        if replies_enabled {
                            for slot in palette_queries.feed(&buf[..n]) {
                                let Some(palette) = palette else {
                                    replies_enabled = false;
                                    break;
                                };
                                let color = match slot {
                                    10 => palette.foreground,
                                    11 => palette.background,
                                    _ => continue,
                                };
                                if write_frame(
                                    &reader_writer,
                                    palette_reply(slot, color).as_bytes(),
                                )
                                .is_err()
                                {
                                    replies_enabled = false;
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => break, // EIO on slave close, or a real read error
                }
            }
        });

        Ok(PtySession {
            master: pair.master,
            child,
            writer,
            parser,
            reader_thread: Some(reader_thread),
            exited: false,
        })
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        write_frame(&self.writer, bytes)?;
        Ok(())
    }

    /// PTY (TIOCSWINSZ via MasterPty::resize) and parser (set_size) in lockstep.
    /// Kicks SIGWINCH to the child for free.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Command(format!("resize: {e}")))?;
        // Recover from a poisoned lock (a panicked reader thread) instead of propagating
        // the panic — the vt100 screen is advisory render state, not an invariant.
        let mut p = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        p.screen_mut().set_size(rows, cols);
        Ok(())
    }

    /// Run f against the current screen under the parser lock (render path).
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        // Recover from a poisoned lock rather than crash the render loop mid-alt-screen.
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        f(parser.screen())
    }

    pub fn is_exited(&mut self) -> bool {
        // try_wait reaps on Ok(Some): once observed exited, latch it so kill() never
        // signals the (now potentially recycled) pid.
        if self.exited {
            return true;
        }
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            self.exited = true;
        }
        self.exited
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Kill + reap the child, join the reader. Idempotent.
    pub fn kill(&mut self) {
        // Signal ONLY while the child is still ours to signal. Once reaped (self.exited),
        // its pid may have been recycled by an unrelated process, so both the group SIGKILL
        // and child.kill() below would target a stranger — skip all of it and just clean up
        // the reader thread.
        if !self.exited {
            // SIGKILL the child's whole process group first: a backgrounded grandchild in
            // the same group can keep the pty slave open after the direct child is reaped,
            // leaving the master reader blocked on read() forever. portable-pty spawns the
            // child as a session/group leader (setsid), so its pid is the pgid.
            if let Some(pid) = self.child.process_id() {
                let pid = pid as libc::pid_t;
                // Safety: getpgid/kill are simple libc calls; a negative pgid targets the group.
                unsafe {
                    let pgid = libc::getpgid(pid);
                    if pgid > 0 {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                }
            }
            let _ = self.child.kill();
            let _ = self.child.wait(); // reap the zombie
            self.exited = true;
        }
        if let Some(handle) = self.reader_thread.take() {
            // The group kill closes the slave for the common case, so join returns at once.
            // A grandchild that escaped the group (its own setsid session) can still hold
            // the slave open, so bound the wait: hand the join to a watcher and detach it
            // rather than hang the render loop. The detached reader unblocks on its own
            // once that grandchild finally exits.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = handle.join();
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Viewer quit kills owned children; backends resume by ID, so conversation
        // state survives (documented). Idempotent with an explicit kill().
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Barrier, Mutex};

    use super::{
        PALETTE_QUERIES, PaletteQueryDetector, SharedWriter, palette_reply, spec_from_command,
        write_frame_after_lock_boundary,
    };

    struct PartialWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        max_chunk_len: usize,
    }

    impl Write for PartialWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let len = buf.len().min(self.max_chunk_len);
            self.bytes.lock().unwrap().extend_from_slice(&buf[..len]);
            Ok(len)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn spec_from_command_copies_env_pairs() {
        let mut cmd = std::process::Command::new("claude");
        cmd.arg("agents").env("CLAUDE_AGENTS_SELECT", "work0001");
        let spec = spec_from_command(&cmd, 24, 80);
        assert_eq!(spec.program, "claude");
        assert_eq!(spec.args, vec!["agents".to_string()]);
        assert!(
            spec.envs
                .contains(&("CLAUDE_AGENTS_SELECT".to_string(), "work0001".to_string())),
            "spec_from_command must carry the env pair, got {:?}",
            spec.envs
        );
    }

    #[test]
    fn palette_query_detector_recognizes_every_feed_boundary() {
        for &(query, slot) in &PALETTE_QUERIES {
            for split in 1..query.len() {
                let mut detector = PaletteQueryDetector::default();
                assert!(
                    detector.feed(&query[..split]).is_empty(),
                    "split {split} of {query:?}"
                );
                assert_eq!(
                    detector.feed(&query[split..]),
                    vec![slot],
                    "split {split} of {query:?}"
                );
            }
        }
    }

    #[test]
    fn palette_query_detector_recognizes_one_byte_chunks() {
        for &(query, slot) in &PALETTE_QUERIES {
            let mut detector = PaletteQueryDetector::default();
            let slots = query
                .iter()
                .flat_map(|byte| detector.feed(std::slice::from_ref(byte)))
                .collect::<Vec<_>>();
            assert_eq!(slots, vec![slot], "one byte chunks for {query:?}");
        }
    }

    #[test]
    fn palette_query_detector_handles_adjacent_malformed_and_partial_sequences() {
        let bel_foreground = PALETTE_QUERIES[0].0;
        let st_foreground = PALETTE_QUERIES[1].0;
        let bel_background = PALETTE_QUERIES[2].0;

        let mut detector = PaletteQueryDetector::default();
        assert_eq!(
            detector.feed(&[bel_foreground, st_foreground, bel_background].concat()),
            vec![10, 10, 11]
        );

        let mut detector = PaletteQueryDetector::default();
        assert!(
            detector
                .feed(b"\x1b]10;x\x07unrelated\x1b]12;?\x07")
                .is_empty()
        );
        assert_eq!(detector.feed(bel_background), vec![11]);

        let mut detector = PaletteQueryDetector::default();
        assert!(detector.feed(b"\x1b").is_empty());
        assert!(detector.feed(b"]1").is_empty());
        assert!(detector.feed(b"0;").is_empty());
        assert_eq!(detector.feed(b"?\x07"), vec![10]);

        let mut detector = PaletteQueryDetector::default();
        assert_eq!(
            detector.feed(b"\x1b]10;?\x1b]10;?\x07"),
            vec![10],
            "the retained escape suffix starts exactly one valid query"
        );
    }

    #[test]
    fn write_frame_serializes_partial_palette_reply_and_user_input() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(PartialWriter {
            bytes: Arc::clone(&written),
            max_chunk_len: 3,
        })));
        let palette = palette_reply(10, [0x12, 0x34, 0x56]).into_bytes();
        let input = b"user input that must remain one complete frame\r".to_vec();
        let lock_boundary = Arc::new(Barrier::new(2));

        let palette_writer = Arc::clone(&writer);
        let palette_lock_boundary = Arc::clone(&lock_boundary);
        let palette_task = std::thread::spawn(move || {
            write_frame_after_lock_boundary(&palette_writer, &palette, || {
                palette_lock_boundary.wait();
            })
            .unwrap();
            palette
        });

        let input_writer = Arc::clone(&writer);
        let input_lock_boundary = Arc::clone(&lock_boundary);
        let input_task = std::thread::spawn(move || {
            write_frame_after_lock_boundary(&input_writer, &input, || {
                input_lock_boundary.wait();
            })
            .unwrap();
            input
        });

        let palette = palette_task.join().unwrap();
        let input = input_task.join().unwrap();
        let actual = written.lock().unwrap().clone();
        let palette_then_input = [palette.as_slice(), input.as_slice()].concat();
        let input_then_palette = [input.as_slice(), palette.as_slice()].concat();

        assert!(
            actual == palette_then_input || actual == input_then_palette,
            "partial writes interleaved frames: {actual:?}"
        );
    }
}
