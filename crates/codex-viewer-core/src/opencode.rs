use crate::backend::{Backend, BackendKind, Capabilities, Session, Status};
use crate::error::Result;

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
        }
    }
    fn list(&mut self) -> Result<Vec<Session>> {
        // Missing DB file is a quiet empty backend, not an error.
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        // Read-only open (same discipline as the codex registry): never create or write
        // the tool's own DB; WAL is fine to read while opencode holds it open.
        let conn = rusqlite::Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        // ONE process check per list() call: no per-session /proc signal exists.
        let live = live_opencode_proc();
        let now = now_ms();
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
                rollout_path: None,
            })
        })?;
        let mut sessions = Vec::new();
        for session in rows {
            sessions.push(session?);
        }
        Ok(sessions)
    }
    fn spawn(&self, dir: &std::path::Path, task: &str) -> Result<()> {
        let title: String = task.chars().take(40).collect();
        let mut cmd = std::process::Command::new("opencode");
        cmd.arg("run")
            .arg("--dir")
            .arg(dir)
            .arg("--title")
            .arg(&title)
            .arg(task);
        // Viewer-owned log dir; we do NOT write under ~/.local/share/opencode/.
        let home = std::env::var("HOME").unwrap_or_default();
        let log_path = std::path::PathBuf::from(home)
            .join(".local/state/codex-agent-viewer/logs")
            .join(format!("opencode-{}.log", now_ms()));
        crate::spawn::spawn_detached(cmd, &log_path)?;
        Ok(())
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// PURE status heuristic, unit-tested (the process check is injected):
/// Running iff live_opencode_proc && (now_ms - updated_at_ms) <= 60_000; else Done.
/// Never returns Errored.
pub fn opencode_status(live_opencode_proc: bool, updated_at_ms: i64, now_ms: i64) -> Status {
    if live_opencode_proc && (now_ms - updated_at_ms) <= 60_000 {
        Status::Running
    } else {
        Status::Done
    }
}
