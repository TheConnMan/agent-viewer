//! The embedded-attach engine: a `PtySession` owning a real PTY + child + vt100 parser,
//! UI-free so it can be headless-integration-tested (spawn `sh`/`cat`) and reused by a
//! Phase-3 daemon. Stream B owns this module in full.

use crate::error::Result;

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
    let _ = (cmd, rows, cols);
    todo!("Stream B: read Command getters, clamp rows/cols >= 1")
}

/// A live PTY session: master pty, child, writer, vt100 parser fed by a reader thread.
pub struct PtySession {}

impl PtySession {
    /// Open a portable-pty at rows x cols (winsize set AT OPEN — a 0x0 pty renders
    /// nothing; see memory pty-tui-testing-needs-winsize), spawn the command on the
    /// slave, detach a reader thread feeding vt100::Parser::new(rows, cols, 0). The
    /// child gets the pty as its controlling terminal (portable-pty default).
    pub fn spawn(spec: PtySpec) -> Result<PtySession> {
        let _ = spec;
        todo!("Stream B: open pty at rows x cols, spawn child, detach reader thread")
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        let _ = bytes;
        todo!("Stream B: write to the pty master")
    }

    /// PTY (TIOCSWINSZ via MasterPty::resize) and parser (set_size) in lockstep.
    /// Kicks SIGWINCH to the child for free.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let _ = (rows, cols);
        todo!("Stream B: MasterPty::resize + parser set_size in lockstep")
    }

    /// Run f against the current screen under the parser lock (render path).
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        let _ = f;
        todo!("Stream B: lock parser, call f(parser.screen())")
    }

    pub fn is_exited(&mut self) -> bool {
        todo!("Stream B: child try_wait")
    }

    pub fn pid(&self) -> Option<u32> {
        todo!("Stream B: child process id")
    }

    /// Kill + reap the child, join the reader. Idempotent.
    pub fn kill(&mut self) {
        todo!("Stream B: kill + reap child, join reader thread")
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Stream B: self.kill() — viewer quit kills owned children; backends resume by
        // ID, so conversation state survives (documented). Empty until spawn() exists.
    }
}
