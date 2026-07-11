//! The embedded-attach engine: a `PtySession` owning a real PTY + child + vt100 parser,
//! UI-free so it can be headless-integration-tested (spawn `sh`/`cat`) and reused by a
//! Phase-3 daemon. Stream B owns this module in full.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::error::{Error, Result};

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
    }
}

/// A live PTY session: master pty, child, writer, vt100 parser fed by a reader thread.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
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
            Ok(writer) => writer,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Command(format!("take writer: {e}")));
            }
        };

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
            exited: false,
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
    use super::spec_from_command;

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
}
