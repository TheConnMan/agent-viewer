//! The embedded-attach engine: a `PtySession` owning a real PTY + child + vt100 parser,
//! UI-free so it can be headless-integration-tested (spawn `sh`/`cat`) and reused by a
//! Phase-3 daemon. Stream B owns this module in full.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct PtySpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub rows: u16,
    pub cols: u16,
}

/// Read program/args/cwd off a built `Command` via the stable getters
/// (get_program/get_args/get_current_dir). rows/cols clamped to >= 1.
pub fn spec_from_command(cmd: &std::process::Command, rows: u16, cols: u16) -> PtySpec {
    PtySpec {
        program: cmd.get_program().to_string_lossy().into_owned(),
        args: cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        cwd: cmd.get_current_dir().map(|p| p.to_path_buf()),
        rows: rows.max(1),
        cols: cols.max(1),
    }
}

/// A live PTY session: master pty, child, writer, vt100 parser fed by a reader thread.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    reader_thread: Option<JoinHandle<()>>,
}

impl PtySession {
    /// Open a portable-pty at rows x cols (winsize set AT OPEN — a 0x0 pty renders
    /// nothing; see memory pty-tui-testing-needs-winsize), spawn the command on the
    /// slave, detach a reader thread feeding vt100::Parser::new(rows, cols, 0). The
    /// child gets the pty as its controlling terminal (portable-pty default).
    pub fn spawn(spec: PtySpec) -> Result<PtySession> {
        let rows = spec.rows.max(1);
        let cols = spec.cols.max(1);

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

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| Error::Command(format!("spawn {}: {e}", spec.program)))?;

        // Drop our slave handle so the child is the sole owner: when it exits the pty
        // slave closes and the master reader unblocks (EOF/EIO), letting the reader
        // thread — and thus Drop — finish without hanging.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| Error::Command(format!("clone reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| Error::Command(format!("take writer: {e}")))?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let reader_parser = Arc::clone(&parser);
        let reader_thread = std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: slave closed
                    Ok(n) => {
                        if let Ok(mut p) = reader_parser.lock() {
                            p.process(&buf[..n]);
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
        })
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
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
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_size(rows, cols);
        }
        Ok(())
    }

    /// Run f against the current screen under the parser lock (render path).
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        let parser = self.parser.lock().expect("pty parser lock poisoned");
        f(parser.screen())
    }

    pub fn is_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Kill + reap the child, join the reader. Idempotent.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait(); // reap the zombie
        if let Some(handle) = self.reader_thread.take() {
            // The child is dead, so the slave is closed and the reader has unblocked.
            let _ = handle.join();
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
