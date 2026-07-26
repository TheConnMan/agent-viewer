use crate::backend::{Backend, BackendKind, Capabilities, Session, SessionOrigin, Status};
use crate::error::{AttachRefusal, Error, Result};
use std::sync::OnceLock;

pub struct OpencodeBackend {
    db_path: std::path::PathBuf,
    /// Discovered model list, computed once and reused (best-effort; degrades to just the
    /// default when the `opencode models` probe fails).
    models_cache: OnceLock<Vec<String>>,
}

impl OpencodeBackend {
    /// Default path: $HOME/.local/share/opencode/opencode.db
    pub fn new() -> OpencodeBackend {
        OpencodeBackend {
            db_path: crate::home_dir().join(".local/share/opencode/opencode.db"),
            models_cache: OnceLock::new(),
        }
    }
    pub fn with_db(db_path: std::path::PathBuf) -> OpencodeBackend {
        OpencodeBackend {
            db_path,
            models_cache: OnceLock::new(),
        }
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
            attach: true,
            rename: true,
            archive: false,
            delete: true,
            stop: true,
            needs_input: false,
            pr_refs: false,
            live_status: false,
        }
    }
    fn capabilities_for(&self, session: &Session) -> Capabilities {
        let mut capabilities = self.capabilities();
        capabilities.stop = session.pid.is_some();
        capabilities
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
            "SELECT id, parent_id, directory, title, time_created, time_updated, time_archived, \
             permission FROM session ORDER BY time_updated DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let parent_id: Option<String> = row.get(1)?;
            let time_updated: i64 = row.get(5)?;
            let time_archived: Option<i64> = row.get(6)?;
            let permission: Option<String> = row.get(7)?;
            Ok(Session {
                backend: BackendKind::Opencode,
                id: row.get(0)?,
                short_id: None,
                origin: SessionOrigin::Interactive,
                title: row.get(3)?,
                cwd: std::path::PathBuf::from(row.get::<_, String>(2)?),
                git_branch: None,
                status: opencode_status(live, time_updated, now),
                created_at_ms: row.get(4)?,
                updated_at_ms: time_updated,
                hidden: time_archived.is_some(),
                companion: parent_id.is_some() || is_run_mode_permission(permission.as_deref()),
                summary: String::new(),
                // Overlay fills the pid for viewer-spawned opencode sessions.
                pid: None,
                rollout_path: None,
                pr_refs: Vec::new(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    fn available_models(&self) -> Vec<String> {
        // default first, then whatever `opencode models` lists (provider/model ids).
        self.models_cache
            .get_or_init(|| {
                let mut models = vec!["default".to_string()];
                models.extend(opencode_models_via_cli());
                crate::backend::dedup_preserve(models)
            })
            .clone()
    }
    fn spawn(&self, dir: &std::path::Path, task: &str, model: Option<&str>) -> Result<Option<u32>> {
        let title = crate::spawn::truncated_title(task);
        let mut cmd = std::process::Command::new("opencode");
        cmd.arg("run")
            .arg("--dir")
            .arg(dir)
            .arg("--title")
            .arg(&title);
        if let Some(model) = model {
            cmd.arg("-m").arg(model);
        }
        cmd.arg(task);
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
    fn remove(&self, session: &Session) -> Result<()> {
        crate::spawn::run_checked(
            std::process::Command::new("opencode")
                .arg("session")
                .arg("delete")
                .arg(&session.id),
        )
    }
    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        // The official `opencode db` subcommand — a CLI mutation, not a raw DB write.
        crate::spawn::run_checked(
            std::process::Command::new("opencode")
                .arg("db")
                .arg(rename_sql(&session.id, name)),
        )
    }
    fn attach_command(
        &self,
        session: &Session,
    ) -> std::result::Result<std::process::Command, AttachRefusal> {
        // The TUI command accepts `-s <id>` to open a session; the old `run -s <id> -i
        // --dir` form was invalid (`run` has no -i flag). Pin the cwd only when it exists,
        // so a deleted dir does not fail the spawn.
        let mut cmd = std::process::Command::new("opencode");
        cmd.arg("-s").arg(&session.id);
        if session.cwd.is_dir() {
            cmd.current_dir(&session.cwd);
        }
        Ok(cmd)
    }
}

/// Default opencode DB path (mirrors OpencodeBackend::new()).
pub fn default_opencode_db() -> std::path::PathBuf {
    crate::home_dir().join(".local/share/opencode/opencode.db")
}

/// Run `opencode models` and parse stdout. Any failure (spawn error, non-zero exit) is a
/// quiet empty Vec — discovery is best-effort.
fn opencode_models_via_cli() -> Vec<String> {
    let mut cmd = std::process::Command::new("opencode");
    cmd.arg("models");
    match crate::spawn::run_with_timeout(cmd, std::time::Duration::from_secs(3)) {
        Some(stdout) => parse_opencode_models(&stdout),
        None => Vec::new(),
    }
}

/// PURE: each non-empty trimmed line of `opencode models` stdout is a model id
/// (format `provider/model`). Blank/whitespace-only lines are dropped.
pub fn parse_opencode_models(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect()
}

/// The most recent opencode message that has text, as a TranscriptItem (role + concatenated
/// text parts). Reads the `message`/`part` tables read-only. Ok(None) when the DB is missing
/// or the session has no text message. Never creates/writes the DB.
pub fn read_opencode_last_message(
    db_path: &std::path::Path,
    session_id: &str,
) -> Result<Option<crate::codex::rollout::TranscriptItem>> {
    use crate::codex::rollout::TranscriptItem;
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = crate::open_readonly(db_path)?;
    // Rows arrive grouped by message (most-recent message first), parts in creation order.
    let mut stmt = conn.prepare(
        "SELECT m.id, m.data, p.data FROM message m JOIN part p ON p.message_id = m.id \
         WHERE m.session_id = ?1 ORDER BY m.time_created DESC, m.id DESC, p.time_created ASC",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        let mid: String = row.get(0)?;
        let mdata: String = row.get(1)?;
        let pdata: String = row.get(2)?;
        Ok((mid, mdata, pdata))
    })?;

    // Walk the grouped rows, accumulating each message's text parts. Return the first
    // (most-recent) message whose accumulated text is non-empty.
    let mut cur_id: Option<String> = None;
    let mut cur_role = String::from("assistant");
    let mut cur_text = String::new();
    for row in rows {
        let (mid, mdata, pdata) = row?;
        if cur_id.as_deref() != Some(mid.as_str()) {
            // Boundary: the previous message is complete — return it if it had non-blank text
            // (a whitespace-only message, e.g. newline-only parts around tool transitions,
            // would otherwise hide the real prior message behind blank lines). Return the
            // original untrimmed text so the real message's internal formatting is preserved.
            if cur_id.is_some() && !cur_text.trim().is_empty() {
                return Ok(Some(TranscriptItem {
                    role: cur_role,
                    text: cur_text,
                }));
            }
            cur_id = Some(mid);
            cur_role = parsed_role(&mdata);
            cur_text = String::new();
        }
        if let Some(text) = parsed_text_part(&pdata) {
            cur_text.push_str(&text);
        }
    }
    // The final message (no boundary follows it).
    if cur_id.is_some() && !cur_text.trim().is_empty() {
        return Ok(Some(TranscriptItem {
            role: cur_role,
            text: cur_text,
        }));
    }
    Ok(None)
}

/// message.data JSON "role" (defaulting to "assistant" when absent/unparseable).
fn parsed_role(mdata: &str) -> String {
    serde_json::from_str::<serde_json::Value>(mdata)
        .ok()
        .as_ref()
        .and_then(|v| crate::json_str(v, "role").map(String::from))
        .unwrap_or_else(|| "assistant".to_string())
}

/// part.data JSON text, only for `{"type":"text","text":...}` parts (else None so
/// tool/step-start/step-finish parts are skipped). Returns an owned String.
fn parsed_text_part(pdata: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(pdata).ok()?;
    if crate::json_str(&value, "type") != Some("text") {
        return None;
    }
    crate::json_str(&value, "text").map(String::from)
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

/// Recency thresholds for the status heuristic below.
const WORKING_MAX_AGE_MS: i64 = 60_000; // 1 min
const IDLE_MAX_AGE_MS: i64 = 1_800_000; // 30 min

/// PURE three-tier status heuristic, unit-tested (the process check is injected, I-3):
/// Working: live && age <= 1 min; Idle: live && age <= 30 min; Done: otherwise.
/// Never NeedsInput or Error because the session table has no error column.
pub fn opencode_status(live_opencode_proc: bool, updated_at_ms: i64, now_ms: i64) -> Status {
    if !live_opencode_proc {
        return Status::Done;
    }
    let age = now_ms - updated_at_ms;
    if age <= WORKING_MAX_AGE_MS {
        Status::Working
    } else if age <= IDLE_MAX_AGE_MS {
        Status::Idle
    } else {
        Status::Done
    }
}

/// Companion filter for one-shot `opencode run` sessions — opencode's equivalent of the
/// codex `exec`/`subagent` rule in `codex::Source::is_companion`. A run started by a script
/// (an `/implement` review pass, a CI job) is not a fleet member, but it carries no
/// `parent_id`, so `parent_id` alone leaves it in the default list.
///
/// The signal is the `session.permission` column. `opencode run` denies the interactive
/// `question` tool (plus `plan_enter`/`plan_exit`) when it creates the session; the TUI
/// writes no session override at all. Verified live on this box 2026-07-26, both on
/// opencode 1.17.20: `opencode run --title ...` stored
/// `[{"permission":"question","pattern":"*","action":"deny"},{plan_enter deny},{plan_exit
/// deny}]`, a TUI session driven through a pty stored NULL.
///
/// Matched semantically (a denied `question` entry anywhere in the array), not by string
/// equality: the stored key order is not the source order, and the github-action path
/// writes the `question` deny without the plan pair. Never panics — anything that is not
/// a JSON array of objects is treated as interactive, the safe direction (a shown row a
/// keypress away from hidden beats a hidden row you cannot find).
pub fn is_run_mode_permission(permission: Option<&str>) -> bool {
    let Some(raw) = permission.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return false;
    };
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(raw) else {
        return false;
    };
    entries.iter().any(|entry| {
        entry.get("permission").and_then(|p| p.as_str()) == Some("question")
            && entry.get("action").and_then(|a| a.as_str()) == Some("deny")
    })
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
