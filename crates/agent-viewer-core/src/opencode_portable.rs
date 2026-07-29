use crate::backend::{Backend, BackendKind, Capabilities, Session, SessionOrigin, Status};
use crate::error::{AttachRefusal, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Shared compatibility runtime handle. Portable builds have no managed server tier, but the
/// handle remains cloneable so all backend sets in the TUI keep the same public construction
/// contract as Linux.
#[derive(Clone, Default)]
pub struct OpencodeRuntime;

impl OpencodeRuntime {
    pub fn new() -> Self {
        Self
    }
}

pub struct OpencodeBackend {
    db_path: PathBuf,
    _runtime: OpencodeRuntime,
    models_cache: OnceLock<Vec<String>>,
}

impl OpencodeBackend {
    pub fn new() -> Self {
        Self::with_runtime(OpencodeRuntime::new())
    }

    pub fn with_runtime(runtime: OpencodeRuntime) -> Self {
        Self {
            db_path: default_opencode_db(),
            _runtime: runtime,
            models_cache: OnceLock::new(),
        }
    }

    fn list_from_sqlite(&self) -> Result<Vec<Session>> {
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        let conn = crate::open_readonly(&self.db_path)?;
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, directory, title, time_created, time_updated, time_archived, \
             permission FROM session ORDER BY time_updated DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let parent_id: Option<String> = row.get(1)?;
            let time_archived: Option<i64> = row.get(6)?;
            let permission: Option<String> = row.get(7)?;
            Ok(Session {
                backend: BackendKind::Opencode,
                id: row.get(0)?,
                short_id: None,
                origin: SessionOrigin::Interactive,
                title: row.get(3)?,
                cwd: PathBuf::from(row.get::<_, String>(2)?),
                git_branch: None,
                status: Status::Unknown,
                created_at_ms: row.get(4)?,
                updated_at_ms: row.get(5)?,
                hidden: time_archived.is_some(),
                companion: parent_id.is_some() || is_run_mode_permission(permission.as_deref()),
                summary: String::new(),
                pid: None,
                rollout_path: None,
                pr_refs: Vec::new(),
                daemon_hosted: false,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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

    fn listing_scope(&self) -> Option<crate::backend::ListingCacheScope> {
        let key = crate::backend::listing_scope_key(&[
            crate::backend::listing_scope_path(&self.db_path),
            "compatibility".to_string(),
        ]);
        crate::backend::ListingCacheScope::new(self.kind(), key).ok()
    }

    fn capabilities(&self) -> Capabilities {
        crate::backend::opencode_capabilities_for_platform(crate::platform::current_platform())
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        self.list_from_sqlite()
    }

    fn available_models(&self) -> Vec<String> {
        self.models_cache
            .get_or_init(|| {
                let mut models = vec![self.kind().default_model().to_string()];
                models.extend(opencode_models_via_cli());
                crate::backend::dedup_preserve(models)
            })
            .clone()
    }

    fn spawn(&self, dir: &Path, task: &str, model: Option<&str>) -> Result<crate::SpawnResult> {
        let title = crate::spawn::truncated_title(task);
        let mut command = Command::new("opencode");
        command
            .arg("run")
            .arg("--dir")
            .arg(dir)
            .arg("--title")
            .arg(&title);
        if let Some(model) = model {
            command.arg("-m").arg(model);
        }
        command.arg(task);
        let pid =
            crate::spawn::spawn_detached(command, &crate::spawn::viewer_log_path("opencode"))?;
        Ok(crate::SpawnResult {
            pid: Some(pid),
            session_id: None,
        })
    }

    fn remove(&self, _session: &Session) -> Result<()> {
        Err(crate::error::Error::Unsupported(self.kind().name()))
    }

    fn attach_command(&self, session: &Session) -> std::result::Result<Command, AttachRefusal> {
        let mut command = Command::new("opencode");
        command.arg("-s").arg(&session.id);
        if session.cwd.is_dir() {
            command.current_dir(&session.cwd);
        }
        Ok(command)
    }
}

/// Default OpenCode database path used by the read only compatibility backend.
fn default_opencode_db() -> PathBuf {
    crate::platform::opencode_db_from(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        &crate::home_dir(),
    )
}

fn opencode_models_via_cli() -> Vec<String> {
    let mut command = Command::new("opencode");
    command.arg("models");
    match crate::spawn::run_with_timeout(command, crate::spawn::MODEL_PROBE_TIMEOUT) {
        Some(stdout) => parse_opencode_models(&stdout),
        None => Vec::new(),
    }
}

/// PURE parser for the line oriented `opencode models` output.
fn parse_opencode_models(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Detect one shot `opencode run` sessions from their stored permission rule.
fn is_run_mode_permission(permission: Option<&str>) -> bool {
    let Some(raw) = permission.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return false;
    };
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(raw) else {
        return false;
    };
    entries.iter().any(|entry| {
        entry.get("permission").and_then(|value| value.as_str()) == Some("question")
            && entry.get("action").and_then(|value| value.as_str()) == Some("deny")
    })
}
