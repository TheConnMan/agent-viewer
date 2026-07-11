use crate::backend::{Backend, BackendKind, Capabilities, Session, Status};
use crate::error::{Error, Result};

pub struct OpencodeBackend {
    db_path: std::path::PathBuf,
}

impl OpencodeBackend {
    /// Default path: $HOME/.local/share/opencode/opencode.db
    pub fn new() -> OpencodeBackend {
        let home = std::env::var("HOME").unwrap_or_default();
        OpencodeBackend {
            db_path: std::path::PathBuf::from(home).join(".local/share/opencode/opencode.db"),
        }
    }
    pub fn with_db(db_path: std::path::PathBuf) -> OpencodeBackend {
        OpencodeBackend { db_path }
    }
}

impl Default for OpencodeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for OpencodeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Opencode
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            spawn: true,
            hide: false,
            attach: true,
            stop: true,
            remove: true,
            rename: true,
        }
    }
    fn list(&mut self) -> Result<Vec<Session>> {
        // Missing DB file is a quiet empty backend, not an error.
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        // Read-only open (same discipline as the codex registry): never create or write
        // the tool's own DB; WAL is fine to read while opencode holds it open.
        let conn = crate::open_readonly(&self.db_path)?;
        // ONE process check per list() call: no per-session /proc signal exists.
        let live = live_opencode_proc();
        let now = crate::spawn::now_ms();
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, directory, title, time_created, time_updated, time_archived \
             FROM session ORDER BY time_updated DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let parent_id: Option<String> = row.get(1)?;
            let time_updated: i64 = row.get(5)?;
            let time_archived: Option<i64> = row.get(6)?;
            Ok(Session {
                backend: BackendKind::Opencode,
                id: row.get(0)?,
                title: row.get(3)?,
                cwd: std::path::PathBuf::from(row.get::<_, String>(2)?),
                created_at_ms: row.get(4)?,
                updated_at_ms: time_updated,
                status: opencode_status(live, time_updated, now),
                hidden: time_archived.is_some(),
                source_label: if parent_id.is_some() {
                    "subagent".to_string()
                } else {
                    "opencode".to_string()
                },
                summary: String::new(),
                companion: parent_id.is_some(),
                // Overlay fills the pid for viewer-spawned opencode sessions.
                pid: None,
                rollout_path: None,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    fn spawn(&self, dir: &std::path::Path, task: &str) -> Result<Option<u32>> {
        let title: String = task.chars().take(40).collect();
        let mut cmd = std::process::Command::new("opencode");
        cmd.arg("run")
            .arg("--dir")
            .arg(dir)
            .arg("--title")
            .arg(&title)
            .arg(task);
        // Viewer-owned log dir; we do NOT write under ~/.local/share/opencode/.
        let log_path = crate::spawn::viewer_log_path("opencode");
        let pid = crate::spawn::spawn_detached(cmd, &log_path)?;
        Ok(Some(pid))
    }
    fn stop(&self, session: &Session) -> Result<()> {
        // Only viewer-spawned opencode sessions carry a pid (filled by the overlay).
        match session.pid {
            Some(pid) => crate::spawn::terminate(pid, "opencode"),
            None => Err(Error::Unsupported(self.kind().name())),
        }
    }
    fn remove(&self, id: &str) -> Result<()> {
        let output = std::process::Command::new("opencode")
            .arg("session")
            .arg("delete")
            .arg(id)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::Command(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ))
        }
    }
    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        // The official `opencode db` subcommand — a CLI mutation, not a raw DB write.
        let output = std::process::Command::new("opencode")
            .arg("db")
            .arg(rename_sql(&session.id, name))
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::Command(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ))
        }
    }
    fn attach_command(&self, session: &Session) -> Option<std::process::Command> {
        let mut cmd = std::process::Command::new("opencode");
        cmd.arg("run")
            .arg("-s")
            .arg(&session.id)
            .arg("-i")
            .arg("--dir")
            .arg(&session.cwd);
        Some(cmd)
    }
}

/// IMPURE process check (live-verified only): does any process named `opencode*` exist.
/// All opencode sessions share one process, so this is a single best-effort signal.
fn live_opencode_proc() -> bool {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.processes()
        .values()
        .any(|p| p.name().to_string_lossy().starts_with("opencode"))
}

/// PURE three-tier status heuristic, unit-tested (the process check is injected, I-3):
/// Working: live && age <= 60_000; Idle: live && age <= 1_800_000 (30 min);
/// Done: otherwise. Never NeedsInput/Failed/Stopped (no signal exists — the session
/// table has no error column; verified live 2026-07-11).
pub fn opencode_status(live_opencode_proc: bool, updated_at_ms: i64, now_ms: i64) -> Status {
    if !live_opencode_proc {
        return Status::Done;
    }
    let age = now_ms - updated_at_ms;
    if age <= 60_000 {
        Status::Working
    } else if age <= 1_800_000 {
        Status::Idle
    } else {
        Status::Done
    }
}

/// PURE, unit-tested: `UPDATE session SET title='<t>' WHERE id='<id>'` with single
/// quotes doubled in both values (SQL-92 escaping).
pub fn rename_sql(id: &str, title: &str) -> String {
    let escape = |value: &str| value.replace('\'', "''");
    format!(
        "UPDATE session SET title='{}' WHERE id='{}'",
        escape(title),
        escape(id)
    )
}
