use crate::backend::{
    Backend, BackendKind, Capabilities, ListingCacheScope, Session, SessionOrigin, SpawnResult,
    Status, TailEvent,
};
use crate::error::{AttachRefusal, Error, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const GROK_METHODS: &[&str] = &[
    "initialize",
    "session/new",
    "session/prompt",
    "session/cancel",
    "x.ai/sessions/list",
    "x.ai/session/rename",
    "x.ai/session/delete",
    "x.ai/models/list",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokDiagnostics {
    pub binary: PathBuf,
    pub home: PathBuf,
    pub binary_available: bool,
    pub leader_count: usize,
    pub registered: bool,
    pub methods: Vec<String>,
}

/// The only public entry point for Grok storage and lifecycle operations.
pub struct GrokLifecycle {
    binary: PathBuf,
    home: PathBuf,
}

impl GrokLifecycle {
    pub fn new(binary: impl Into<PathBuf>, home: impl Into<PathBuf>) -> GrokLifecycle {
        GrokLifecycle {
            binary: binary.into(),
            home: home.into(),
        }
    }

    pub fn diagnostics(&self) -> Result<GrokDiagnostics> {
        let binary_available = binary_available(&self.binary);
        #[cfg(target_os = "linux")]
        let (leader_count, registered) = {
            let candidates = leader_candidates(&self.home)?;
            let count = candidates.len();
            let mut registered = false;
            for candidate in candidates {
                match LeaderClient::connect(&candidate.socket, None)
                    .and_then(|mut client| client.probe())
                {
                    Ok(()) => registered = true,
                    Err(error) if is_frame_cap_error(&error) => return Err(error),
                    Err(_) => {}
                }
            }
            (count, registered)
        };
        #[cfg(not(target_os = "linux"))]
        let (leader_count, registered) = (0, false);
        Ok(GrokDiagnostics {
            binary: self.binary.clone(),
            home: self.home.clone(),
            binary_available,
            leader_count,
            registered,
            methods: if registered {
                GROK_METHODS
                    .iter()
                    .map(|method| (*method).to_string())
                    .collect()
            } else {
                Vec::new()
            },
        })
    }

    pub fn list(&self) -> Result<Vec<Session>> {
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::Unsupported(BackendKind::Grok.name()))
        }
        #[cfg(target_os = "linux")]
        {
            let mut sessions = read_durable_sessions(&self.home)?;
            if let Ok((_, mut clients)) = self.connect_existing(None, true) {
                let mut roster_statuses = std::collections::HashMap::new();
                for client in &mut clients {
                    if let Ok(body) = client.ext_request("x.ai/sessions/list", json!({}))
                        && let Some(roster) = body.get("result")
                    {
                        merge_roster(&mut sessions, roster, &mut roster_statuses);
                    }
                }
            }
            sessions.sort_by(|left, right| {
                left.updated_at_ms
                    .cmp(&right.updated_at_ms)
                    .then_with(|| left.id.cmp(&right.id))
            });
            Ok(sessions)
        }
    }

    pub fn spawn(&self, cwd: &Path, task: &str, model: Option<&str>) -> Result<SpawnResult> {
        #[cfg(target_os = "linux")]
        {
            let cwd_text = cwd
                .to_str()
                .filter(|cwd| is_terminal_safe(cwd))
                .ok_or_else(|| {
                    Error::Command("Grok working directory contains unsafe characters".into())
                })?;
            if model.is_some_and(|model| !is_terminal_safe(model)) {
                return Err(Error::Command(
                    "Grok model identity contains unsafe characters".into(),
                ));
            }
            let mut client = self.connect_or_start(model)?;
            let response =
                client.request("session/new", json!({"cwd": cwd_text, "mcpServers": []}))?;
            let session_id = response
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty() && is_terminal_safe(id))
                .ok_or_else(|| Error::Command("Grok session/new returned no identity".into()))?
                .to_string();
            let prompt_id = client.send_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": task}],
                }),
            )?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let roster_id = client.send_request("_x.ai/sessions/list", json!({}))?;
                let roster = client.response(
                    roster_id,
                    "_x.ai/sessions/list",
                    Some((prompt_id, "session/prompt")),
                )?;
                let working = roster
                    .pointer("/result/sessions")
                    .and_then(Value::as_array)
                    .is_some_and(|sessions| {
                        sessions.iter().any(|session| {
                            session.get("sessionId").and_then(Value::as_str)
                                == Some(session_id.as_str())
                                && session.get("activity").and_then(Value::as_str)
                                    == Some("working")
                                && session.get("resident").and_then(Value::as_bool) == Some(true)
                        })
                    });
                if working {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(Error::Command(
                        "Grok roster did not confirm the spawned session was working".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(SpawnResult {
                pid: None,
                session_id: Some(session_id),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (cwd, task, model);
            Err(Error::Unsupported(BackendKind::Grok.name()))
        }
    }

    pub fn cancel(&self, session_id: &str) -> Result<()> {
        require_session_id(session_id)?;
        #[cfg(target_os = "linux")]
        {
            let (candidate_count, mut clients) = match self.connect_existing(None, true) {
                Ok(connected) => connected,
                Err(error) if is_definitively_unreachable_error(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            if candidate_count == 0 {
                return Ok(());
            }
            let mut owner = None;
            for mut client in clients.drain(..) {
                let Ok(body) = client.ext_request("x.ai/sessions/list", json!({})) else {
                    continue;
                };
                let owns_session = body
                    .pointer("/result/sessions")
                    .and_then(Value::as_array)
                    .is_some_and(|sessions| {
                        sessions.iter().any(|session| {
                            session.get("sessionId").and_then(Value::as_str) == Some(session_id)
                        })
                    });
                if owns_session {
                    if owner.is_some() {
                        return Err(Error::Command(format!(
                            "multiple Grok leaders claim session {session_id}"
                        )));
                    }
                    owner = Some(client);
                }
            }
            let mut owner = owner.ok_or_else(|| {
                Error::Command(format!(
                    "no reachable Grok leader owns session {session_id}"
                ))
            })?;
            owner.notify("session/cancel", json!({"sessionId": session_id}))?;
            let _ = owner.ext_request("x.ai/sessions/list", json!({}));
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        Err(Error::Unsupported(BackendKind::Grok.name()))
    }

    pub fn rename(&self, session_id: &str, title: &str) -> Result<()> {
        require_session_id(session_id)?;
        #[cfg(target_os = "linux")]
        {
            let mut client = self.connect_or_start(None)?;
            let response = client.ext_request(
                "x.ai/session/rename",
                json!({"sessionId": session_id, "title": title}),
            )?;
            if response.get("success").and_then(Value::as_bool) != Some(true) {
                return Err(Error::Command("Grok session rename did not succeed".into()));
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = title;
            Err(Error::Unsupported(BackendKind::Grok.name()))
        }
    }

    pub fn delete(&self, session_id: &str) -> Result<()> {
        require_session_id(session_id)?;
        #[cfg(target_os = "linux")]
        {
            let cwd = durable_session_cwd(&self.home, session_id)?;
            let mut client = self.connect_or_start(None)?;
            let response = client.ext_request(
                "x.ai/session/delete",
                json!({"sessionId": session_id, "cwd": cwd}),
            )?;
            if response.get("success").and_then(Value::as_bool) != Some(true) {
                return Err(Error::Command("Grok session delete did not succeed".into()));
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        Err(Error::Unsupported(BackendKind::Grok.name()))
    }

    pub fn models(&self) -> Result<Vec<String>> {
        let fallback = || vec![BackendKind::Grok.default_model().to_string()];
        #[cfg(target_os = "linux")]
        {
            let Ok((_, mut clients)) = self.connect_existing(None, true) else {
                return Ok(fallback());
            };
            for client in &mut clients {
                let Ok(body) = client.ext_request("x.ai/models/list", json!({})) else {
                    continue;
                };
                let Some(value) = body.get("result") else {
                    continue;
                };
                let mut models = fallback();
                if let Some(current) = value.get("currentModelId").and_then(Value::as_str)
                    && is_terminal_safe(current)
                {
                    models.push(current.to_string());
                }
                if let Some(available) = value.get("availableModels").and_then(Value::as_array) {
                    models.extend(available.iter().filter_map(|entry| {
                        entry
                            .get("modelId")
                            .and_then(Value::as_str)
                            .filter(|model| is_terminal_safe(model))
                            .map(str::to_string)
                    }));
                }
                return Ok(crate::backend::dedup_preserve(models));
            }
        }
        Ok(fallback())
    }

    #[cfg(target_os = "linux")]
    fn connect_existing(
        &self,
        model: Option<&str>,
        initialize: bool,
    ) -> Result<(usize, Vec<LeaderClient>)> {
        let candidates = leader_candidates(&self.home)?;
        let count = candidates.len();
        let mut clients = Vec::new();
        let mut first_unreachable_error = None;
        let mut first_substantive_error = None;
        for candidate in candidates {
            let connected =
                LeaderClient::connect(&candidate.socket, model).and_then(|mut client| {
                    client.probe()?;
                    if initialize {
                        client.initialize()?;
                    }
                    Ok(client)
                });
            match connected {
                Ok(client) => clients.push(client),
                Err(error) if is_definitively_unreachable_error(&error) => {
                    if first_unreachable_error.is_none() {
                        first_unreachable_error = Some(error);
                    }
                }
                Err(error) => {
                    if first_substantive_error.is_none() {
                        first_substantive_error = Some(error);
                    }
                }
            }
        }
        if clients.is_empty() {
            if let Some(error) = first_substantive_error {
                return Err(error);
            }
            if let Some(error) = first_unreachable_error {
                return Err(error);
            }
        }
        Ok((count, clients))
    }

    #[cfg(target_os = "linux")]
    fn connect_or_start(&self, model: Option<&str>) -> Result<LeaderClient> {
        let mut last_error = match self.connect_existing(model, true) {
            Ok((_, mut clients)) if !clients.is_empty() => return Ok(clients.remove(0)),
            Err(error) => Some(error),
            Ok(_) => None,
        };
        self.start_leader()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match self.connect_existing(model, true) {
                Ok((_, mut clients)) if !clients.is_empty() => return Ok(clients.remove(0)),
                Err(error) => last_error = Some(error),
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(last_error.unwrap_or_else(|| {
                    Error::Command("official Grok leader did not become reachable".into())
                }));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(target_os = "linux")]
    fn start_leader(&self) -> Result<()> {
        if !binary_available(&self.binary) {
            return Err(Error::Command(
                "official grok binary is unavailable".to_string(),
            ));
        }
        let resolved_home = lexical_absolute_path(&self.home)?;
        let stable_cwd = if resolved_home.is_dir() {
            resolved_home.as_path()
        } else {
            resolved_home
                .parent()
                .filter(|parent| parent.is_dir())
                .unwrap_or_else(|| Path::new("/"))
        };
        let mut command = std::process::Command::new(&self.binary);
        command
            .arg("agent")
            .arg("leader")
            .arg("--no-exit-on-disconnect")
            .arg("--relay-on-demand")
            .env("GROK_HOME", &resolved_home)
            .current_dir(stable_cwd);
        crate::spawn::spawn_detached_silent(command)?;
        Ok(())
    }
}

pub struct GrokBackend {
    lifecycle: GrokLifecycle,
}

impl GrokBackend {
    pub fn new() -> GrokBackend {
        GrokBackend {
            lifecycle: GrokLifecycle::new("grok", default_grok_home()),
        }
    }
}

impl Default for GrokBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for GrokBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Grok
    }

    fn capabilities(&self) -> Capabilities {
        #[cfg(target_os = "linux")]
        {
            Capabilities {
                spawn: true,
                attach: true,
                rename: true,
                archive: false,
                delete: true,
                stop: true,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Capabilities::none()
        }
    }

    fn listing_scope(&self) -> Option<ListingCacheScope> {
        let key = crate::backend::listing_scope_key(&[
            crate::backend::listing_scope_path(&self.lifecycle.home),
            crate::backend::listing_scope_executable(&self.lifecycle.binary.to_string_lossy()),
        ]);
        ListingCacheScope::new(self.kind(), key).ok()
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        self.lifecycle.list()
    }

    fn tail(&self, session: &Session, max_events: usize) -> Result<Vec<TailEvent>> {
        let Some(path) = session.rollout_path.as_deref() else {
            return Ok(Vec::new());
        };
        read_grok_tail(path, max_events)
    }

    fn spawn(
        &self,
        dir: &Path,
        task: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<SpawnResult> {
        let _ = effort;
        self.lifecycle.spawn(dir, task, model)
    }

    fn available_models(&self) -> Vec<String> {
        self.lifecycle
            .models()
            .unwrap_or_else(|_| vec![BackendKind::Grok.default_model().to_string()])
    }

    fn stop(&self, session: &Session) -> Result<()> {
        self.lifecycle.cancel(&session.id)
    }

    fn remove(&self, session: &Session) -> Result<()> {
        self.lifecycle.delete(&session.id)
    }

    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        self.lifecycle.rename(&session.id, name)
    }

    fn attach_command(
        &self,
        session: &Session,
    ) -> std::result::Result<std::process::Command, AttachRefusal> {
        let mut command = std::process::Command::new(&self.lifecycle.binary);
        command
            .arg("--resume")
            .arg(&session.id)
            .current_dir(&session.cwd);
        Ok(command)
    }
}

#[derive(Deserialize)]
struct DurableInfo {
    id: String,
    cwd: String,
}

#[derive(Deserialize)]
struct DurableSummary {
    info: DurableInfo,
    session_summary: String,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    hidden: Option<bool>,
    #[serde(default)]
    session_kind: Option<String>,
    #[serde(default)]
    generated_title: Option<String>,
    #[serde(default)]
    last_turn_summary: Option<String>,
    #[serde(default)]
    last_active_at: Option<String>,
}

// Summaries are metadata records. One MiB leaves ample room for titles and turn summaries
// while preventing a corrupt record from forcing an unbounded allocation during refresh.
const GROK_SUMMARY_MAX_BYTES: u64 = 1024 * 1024;
const GROK_DURABLE_LIST_MAX_BYTES: u64 = 32 * 1024 * 1024;
const GROK_STATUS_TAIL_MAX_BYTES: u64 = 1024 * 1024;
const GROK_DURABLE_STATUS_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[cfg(target_os = "linux")]
fn read_durable_sessions(home: &Path) -> Result<Vec<Session>> {
    let home_directory = match SecureDirectory::open_root(home) {
        Ok(directory) => directory,
        Err(Error::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ELOOP) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let sessions_directory = match home_directory.open_directory(std::ffi::OsStr::new("sessions")) {
        Ok(directory) => directory,
        Err(Error::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ELOOP) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let cwd_names = sessions_directory.names()?;
    let mut sessions = Vec::new();
    let mut bytes_read = 0_u64;
    let mut status_bytes_read = 0_u64;
    for cwd_name in cwd_names {
        let Ok(cwd_directory) = sessions_directory.open_directory(&cwd_name) else {
            continue;
        };
        let Ok(session_names) = cwd_directory.names() else {
            continue;
        };
        for session_name in session_names {
            let Ok(session_directory) = cwd_directory.open_directory(&session_name) else {
                continue;
            };
            let Ok(file) = session_directory.open_regular(std::ffi::OsStr::new("summary.json"))
            else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            let summary_bytes = metadata.len();
            if summary_bytes > GROK_SUMMARY_MAX_BYTES
                || bytes_read.saturating_add(summary_bytes) > GROK_DURABLE_LIST_MAX_BYTES
            {
                continue;
            }
            bytes_read += summary_bytes;
            let mut body = Vec::new();
            use std::io::Read as _;
            if file
                .take(GROK_SUMMARY_MAX_BYTES + 1)
                .read_to_end(&mut body)
                .is_err()
                || body.len() as u64 > GROK_SUMMARY_MAX_BYTES
            {
                continue;
            }
            let Ok(summary) = serde_json::from_slice::<DurableSummary>(&body) else {
                continue;
            };
            if summary.info.id.trim().is_empty()
                || summary.info.cwd.trim().is_empty()
                || !is_terminal_safe(&summary.info.id)
                || !is_terminal_safe(&summary.info.cwd)
                || session_name.to_str() != Some(summary.info.id.as_str())
            {
                continue;
            }
            let default_hidden = summary
                .session_kind
                .as_deref()
                .is_some_and(|kind| kind.starts_with("subagent"));
            let hidden = summary.hidden.unwrap_or(default_hidden);
            let Some(created_at_ms) = crate::rfc3339_millis(&summary.created_at) else {
                continue;
            };
            let Some(updated_at_ms) = summary
                .last_active_at
                .as_deref()
                .and_then(crate::rfc3339_millis)
                .or_else(|| crate::rfc3339_millis(&summary.updated_at))
            else {
                continue;
            };
            let title = summary
                .generated_title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| summary.session_summary.trim());
            let title = sanitize_terminal_text(title);
            let fallback_title = sanitize_terminal_text(&summary.info.id);
            let summary_text = sanitize_terminal_text(
                summary
                    .last_turn_summary
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or(&summary.session_summary),
            );
            let history = session_directory
                .open_regular(std::ffi::OsStr::new("chat_history.jsonl"))
                .is_ok()
                .then(|| {
                    home.join("sessions")
                        .join(&cwd_name)
                        .join(&session_name)
                        .join("chat_history.jsonl")
                });
            let (status, status_bytes) = read_durable_status(
                &session_directory,
                &summary.info.id,
                GROK_DURABLE_STATUS_MAX_BYTES.saturating_sub(status_bytes_read),
            );
            status_bytes_read = status_bytes_read.saturating_add(status_bytes);
            sessions.push(Session {
                backend: BackendKind::Grok,
                id: summary.info.id.clone(),
                short_id: None,
                origin: SessionOrigin::Interactive,
                title: if title.is_empty() {
                    fallback_title
                } else {
                    title
                },
                cwd: PathBuf::from(summary.info.cwd),
                git_branch: None,
                status,
                created_at_ms,
                updated_at_ms,
                hidden,
                companion: false,
                subagent: default_hidden,
                summary: summary_text,
                pid: None,
                rollout_path: history,
                pr_refs: Vec::new(),
                daemon_hosted: false,
            });
        }
    }
    Ok(sessions)
}

#[cfg(target_os = "linux")]
fn read_durable_status(
    session_directory: &SecureDirectory,
    session_id: &str,
    remaining_bytes: u64,
) -> (Status, u64) {
    use std::io::{Read as _, Seek as _, SeekFrom};

    if remaining_bytes == 0 {
        return (Status::Unknown, 0);
    }
    let Ok(mut file) = session_directory.open_regular(std::ffi::OsStr::new("updates.jsonl")) else {
        return (Status::Unknown, 0);
    };
    let Ok(metadata) = file.metadata() else {
        return (Status::Unknown, 0);
    };
    let file_len = metadata.len();
    let read_len = file_len
        .min(GROK_STATUS_TAIL_MAX_BYTES)
        .min(remaining_bytes);
    if read_len == 0 {
        return (Status::Unknown, 0);
    }
    let offset = file_len.saturating_sub(read_len);
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return (Status::Unknown, 0);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(read_len).unwrap_or_default());
    if (&mut file).take(read_len).read_to_end(&mut bytes).is_err() || bytes.len() as u64 != read_len
    {
        return (Status::Unknown, bytes.len() as u64);
    }
    let stable_len = file
        .metadata()
        .is_ok_and(|current| current.len() == file_len);
    if !stable_len {
        return (Status::Unknown, read_len);
    }
    let complete_bytes = if offset > 0 {
        let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n') else {
            return (Status::Unknown, read_len);
        };
        &bytes[first_newline + 1..]
    } else {
        &bytes
    };
    let Ok(body) = std::str::from_utf8(complete_bytes) else {
        return (Status::Unknown, read_len);
    };

    let mut status = Status::Unknown;
    for line in body.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            status = Status::Unknown;
            continue;
        };
        let method = record.get("method").and_then(Value::as_str);
        if record.pointer("/params/sessionId").and_then(Value::as_str) != Some(session_id) {
            continue;
        }
        let update = record.pointer("/params/update");
        let update_kind = update
            .and_then(|update| update.get("sessionUpdate"))
            .and_then(Value::as_str);
        if update_kind == Some("user_message_chunk")
            && matches!(method, Some("session/update" | "_x.ai/session/update"))
        {
            status = Status::Unknown;
            continue;
        }
        if method != Some("_x.ai/session/update") || update_kind != Some("turn_completed") {
            continue;
        }
        let prompt_id = update
            .and_then(|update| update.get("prompt_id"))
            .and_then(Value::as_str)
            .filter(|prompt_id| !prompt_id.trim().is_empty());
        let stop_reason = update
            .and_then(|update| update.get("stop_reason"))
            .and_then(Value::as_str);
        status = match (prompt_id, stop_reason) {
            (Some(_), Some("end_turn" | "cancelled")) => Status::Done,
            (
                Some(_),
                Some("rate_limit" | "error" | "refusal" | "max_tokens" | "max_turn_requests"),
            ) => Status::Error,
            _ => Status::Unknown,
        };
    }
    (status, read_len)
}

#[cfg(target_os = "linux")]
fn durable_session_cwd(home: &Path, session_id: &str) -> Result<PathBuf> {
    let home_directory = SecureDirectory::open_root(home).map_err(|error| {
        Error::Command(format!(
            "Grok delete failed to read durable storage: {error}"
        ))
    })?;
    let sessions_directory = home_directory
        .open_directory(std::ffi::OsStr::new("sessions"))
        .map_err(|error| {
            Error::Command(format!(
                "Grok delete failed to read durable storage: {error}"
            ))
        })?;
    let cwd_names = sessions_directory.names().map_err(|error| {
        Error::Command(format!(
            "Grok delete failed to read durable storage: {error}"
        ))
    })?;
    let mut matched_cwd = None;
    for cwd_name in cwd_names {
        let Ok(cwd_directory) = sessions_directory.open_directory(&cwd_name) else {
            continue;
        };
        let Ok(session_names) = cwd_directory.names() else {
            continue;
        };
        for session_name in session_names {
            if session_name != std::ffi::OsStr::new(session_id) {
                continue;
            }
            if matched_cwd.is_some() {
                return Err(Error::Command(format!(
                    "Grok delete failed because durable session {session_id} is ambiguous"
                )));
            }
            let session_directory =
                cwd_directory
                    .open_directory(&session_name)
                    .map_err(|error| {
                        Error::Command(format!(
                            "Grok delete failed to open durable session {session_id}: {error}"
                        ))
                    })?;
            let file = session_directory
                .open_regular(std::ffi::OsStr::new("summary.json"))
                .map_err(|error| {
                    Error::Command(format!(
                        "Grok delete failed to read durable session {session_id}: {error}"
                    ))
                })?;
            if file.metadata()?.len() > GROK_SUMMARY_MAX_BYTES {
                return Err(Error::Command(format!(
                    "Grok delete failed because durable session {session_id} is oversized"
                )));
            }
            let mut body = Vec::new();
            use std::io::Read as _;
            file.take(GROK_SUMMARY_MAX_BYTES + 1)
                .read_to_end(&mut body)?;
            if body.len() as u64 > GROK_SUMMARY_MAX_BYTES {
                return Err(Error::Command(format!(
                    "Grok delete failed because durable session {session_id} grew too large"
                )));
            }
            let summary: DurableSummary = serde_json::from_slice(&body).map_err(|error| {
                Error::Command(format!(
                    "Grok delete failed to parse durable session {session_id}: {error}"
                ))
            })?;
            if summary.info.id != session_id
                || summary.info.cwd.trim().is_empty()
                || !is_terminal_safe(&summary.info.cwd)
            {
                return Err(Error::Command(format!(
                    "Grok delete failed because durable session {session_id} has invalid identity"
                )));
            }
            matched_cwd = Some(PathBuf::from(summary.info.cwd));
        }
    }
    matched_cwd.ok_or_else(|| {
        Error::Command(format!(
            "Grok delete failed because durable session {session_id} was not found"
        ))
    })
}

fn open_owned_regular(path: &Path) -> Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    let file = open_path_no_symlinks(path, libc::O_RDONLY | libc::O_CLOEXEC)?;
    #[cfg(all(unix, not(target_os = "linux")))]
    let file = {
        use std::os::unix::fs::OpenOptionsExt as _;

        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path)?;

    validate_owned_regular(file)
}

fn validate_owned_regular(file: std::fs::File) -> Result<std::fs::File> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::Command("Grok file is not a regular file".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(Error::Command(
                "Grok file is not owned by the current user".into(),
            ));
        }
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn validate_owned_directory(file: std::fs::File) -> Result<std::fs::File> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        return Err(Error::Command("Grok path is not a directory".into()));
    }
    use std::os::unix::fs::MetadataExt as _;

    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Command(
            "Grok directory is not owned by the current user".into(),
        ));
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
struct SecureDirectory {
    file: std::fs::File,
}

#[cfg(target_os = "linux")]
impl SecureDirectory {
    fn open_root(path: &Path) -> Result<SecureDirectory> {
        let file =
            open_path_no_symlinks(path, libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY)?;
        Ok(SecureDirectory {
            file: validate_owned_directory(file)?,
        })
    }

    fn open_directory(&self, name: &std::ffi::OsStr) -> Result<SecureDirectory> {
        let file = open_at_no_symlinks(
            &self.file,
            name,
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
        )?;
        Ok(SecureDirectory {
            file: validate_owned_directory(file)?,
        })
    }

    fn open_regular(&self, name: &std::ffi::OsStr) -> Result<std::fs::File> {
        validate_owned_regular(open_at_no_symlinks(
            &self.file,
            name,
            libc::O_RDONLY | libc::O_CLOEXEC,
        )?)
    }

    fn names(&self) -> Result<Vec<std::ffi::OsString>> {
        use std::os::fd::AsRawFd as _;

        let descriptor_path = format!("/proc/self/fd/{}", self.file.as_raw_fd());
        let mut names = std::fs::read_dir(descriptor_path)?
            .flatten()
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        names.sort();
        Ok(names)
    }
}

#[cfg(target_os = "linux")]
fn open_path_no_symlinks(path: &Path, flags: libc::c_int) -> Result<std::fs::File> {
    let absolute = lexical_absolute_path(path)?;
    let relative = absolute.strip_prefix("/").unwrap_or(&absolute);
    let anchor = std::fs::File::open("/")?;
    open_at_no_symlinks(&anchor, relative.as_os_str(), flags)
}

#[cfg(target_os = "linux")]
fn lexical_absolute_path(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::from("/");
    for component in joined.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(component) => normalized.push(component),
            std::path::Component::Prefix(_) => {
                return Err(Error::Command("Grok path has an unsupported prefix".into()));
            }
        }
    }
    Ok(normalized)
}

#[cfg(target_os = "linux")]
fn open_at_no_symlinks(
    directory: &std::fs::File,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
) -> Result<std::fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| Error::Command("Grok path contains an invalid null character".into()))?;
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory.as_raw_fd(),
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM)) {
            return Err(Error::Unsupported(
                "secure Grok storage requires openat2 on Linux kernel 5.6 or newer",
            ));
        }
        return Err(error.into());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor as libc::c_int) })
}

fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .filter_map(|character| {
            let unsafe_character = matches!(
                character,
                '\u{0000}'..='\u{001f}'
                    | '\u{007f}'..='\u{009f}'
                    | '\u{061c}'
                    | '\u{200e}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            );
            if unsafe_character {
                character.is_whitespace().then_some(' ')
            } else {
                Some(character)
            }
        })
        .collect()
}

fn is_terminal_safe(text: &str) -> bool {
    !text.chars().any(|character| {
        matches!(
            character,
            '\u{0000}'..='\u{001f}'
                | '\u{007f}'..='\u{009f}'
                | '\u{061c}'
                | '\u{200e}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RosterEntry {
    session_id: String,
    #[serde(default)]
    title: Option<String>,
    cwd: String,
    activity: String,
    resident: bool,
    last_change_unix_ms: i64,
    #[serde(default)]
    last_turn_summary: Option<String>,
}

fn merge_roster(
    sessions: &mut Vec<Session>,
    response: &Value,
    roster_statuses: &mut std::collections::HashMap<String, Status>,
) {
    let Some(entries) = response.get("sessions").and_then(Value::as_array) else {
        return;
    };
    for value in entries {
        let Ok(entry) = serde_json::from_value::<RosterEntry>(value.clone()) else {
            continue;
        };
        if entry.session_id.trim().is_empty() || entry.cwd.trim().is_empty() {
            continue;
        }
        if !is_terminal_safe(&entry.session_id) || !is_terminal_safe(&entry.cwd) {
            continue;
        }
        let status = match entry.activity.as_str() {
            "working" => Status::Working,
            "idle" => Status::Idle,
            "needs_input" => Status::NeedsInput { reason: None },
            "completed" => Status::Done,
            "dead" => Status::Error,
            "dormant" => Status::Unknown,
            _ => Status::Unknown,
        };
        let (status, roster_conflict) = match roster_statuses.get(&entry.session_id) {
            Some(previous) if previous != &status => (Status::Unknown, true),
            Some(previous) => (previous.clone(), false),
            None => (status, false),
        };
        roster_statuses.insert(entry.session_id.clone(), status.clone());
        if let Some(session) = sessions.iter_mut().find(|row| row.id == entry.session_id) {
            if let Some(title) = entry.title.filter(|title| !title.trim().is_empty()) {
                session.title = sanitize_terminal_text(&title);
            }
            session.cwd = PathBuf::from(entry.cwd);
            if !matches!(entry.activity.as_str(), "dormant" | "idle")
                || roster_conflict
                || !matches!(session.status, Status::Done | Status::Error)
            {
                session.status = status;
            }
            session.updated_at_ms = entry.last_change_unix_ms;
            if let Some(summary) = entry
                .last_turn_summary
                .filter(|summary| !summary.trim().is_empty())
            {
                session.summary = sanitize_terminal_text(&summary);
            }
            session.pid = None;
            session.daemon_hosted = entry.resident;
            continue;
        }
        let title = entry
            .title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| entry.session_id.clone());
        sessions.push(Session {
            backend: BackendKind::Grok,
            id: entry.session_id,
            short_id: None,
            origin: SessionOrigin::Interactive,
            title: sanitize_terminal_text(&title),
            cwd: PathBuf::from(entry.cwd),
            git_branch: None,
            status,
            created_at_ms: entry.last_change_unix_ms,
            updated_at_ms: entry.last_change_unix_ms,
            hidden: false,
            companion: false,
            subagent: false,
            summary: sanitize_terminal_text(entry.last_turn_summary.as_deref().unwrap_or_default()),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: entry.resident,
        });
    }
}

fn read_grok_tail(path: &Path, max_events: usize) -> Result<Vec<TailEvent>> {
    if max_events == 0 {
        return Ok(Vec::new());
    }
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut file = open_owned_regular(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(
        len.saturating_sub(crate::TRANSCRIPT_TAIL_BYTES),
    ))?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(len.min(crate::TRANSCRIPT_TAIL_BYTES)).unwrap_or_default(),
    );
    file.take(crate::TRANSCRIPT_TAIL_BYTES)
        .read_to_end(&mut bytes)?;
    let body = String::from_utf8_lossy(&bytes);
    let mut events = Vec::new();
    for line in body.lines() {
        let Some(value) = crate::parse_json_line(line) else {
            continue;
        };
        match crate::json_str(&value, "type") {
            Some("user") => {
                let text = match value.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter(|part| crate::json_str(part, "type") == Some("text"))
                        .filter_map(|part| crate::json_str(part, "text"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                let text = crate::backend::squash(&sanitize_terminal_text(&text));
                if !text.is_empty() {
                    events.push(TailEvent::User(text));
                }
            }
            Some("assistant") => {
                if let Some(text) = value.get("content").and_then(Value::as_str) {
                    let text = crate::backend::squash(&sanitize_terminal_text(text));
                    if !text.is_empty() {
                        events.push(TailEvent::Agent(text));
                    }
                }
                if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let Some(name) =
                            crate::json_str(call, "name").filter(|name| !name.is_empty())
                        else {
                            continue;
                        };
                        let arguments = call
                            .get("arguments")
                            .and_then(Value::as_str)
                            .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
                        events.push(TailEvent::Tool {
                            name: sanitize_terminal_text(name),
                            detail: sanitize_terminal_text(
                                &arguments
                                    .as_ref()
                                    .map(crate::backend::tool_detail)
                                    .unwrap_or_default(),
                            ),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    if events.len() > max_events {
        events = events.split_off(events.len() - max_events);
    }
    Ok(events)
}

fn require_session_id(session_id: &str) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(Error::Command("Grok session identity is empty".into()));
    }
    if !is_terminal_safe(session_id) {
        return Err(Error::Command(
            "Grok session identity contains unsafe characters".into(),
        ));
    }
    Ok(())
}

fn binary_available(binary: &Path) -> bool {
    let available = |path: &Path| {
        if !path.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        true
    };
    if binary.is_absolute() || binary.components().count() > 1 {
        return available(binary);
    }
    std::env::var_os("PATH")
        .as_deref()
        .into_iter()
        .flat_map(std::env::split_paths)
        .any(|directory| available(&directory.join(binary)))
}

fn default_grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home_dir().join(".grok"))
}

#[cfg(target_os = "linux")]
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[cfg(target_os = "linux")]
#[derive(Default)]
struct PartialLeaderCandidate {
    socket: Option<PathBuf>,
    lock: Option<PathBuf>,
}

#[cfg(target_os = "linux")]
struct LeaderCandidate {
    socket: PathBuf,
}

#[cfg(target_os = "linux")]
fn leader_candidates(home: &Path) -> Result<Vec<LeaderCandidate>> {
    use std::collections::BTreeMap;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let entries = match std::fs::read_dir(home) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut candidates: BTreeMap<String, PartialLeaderCandidate> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if let Some(suffix) = name
            .strip_prefix("leader")
            .and_then(|name| name.strip_suffix(".sock"))
        {
            candidates.entry(suffix.to_string()).or_default().socket = Some(path);
        } else if let Some(suffix) = name
            .strip_prefix("leader")
            .and_then(|name| name.strip_suffix(".lock"))
        {
            candidates.entry(suffix.to_string()).or_default().lock = Some(path);
        }
    }
    let effective_uid = unsafe { libc::geteuid() };
    Ok(candidates
        .into_values()
        .filter_map(|candidate| {
            let socket = candidate.socket?;
            let lock = candidate.lock?;
            let socket_metadata = std::fs::symlink_metadata(&socket).ok()?;
            let lock_metadata = std::fs::symlink_metadata(&lock).ok()?;
            if !socket_metadata.file_type().is_socket()
                || !lock_metadata.file_type().is_file()
                || socket_metadata.uid() != effective_uid
                || lock_metadata.uid() != effective_uid
            {
                return None;
            }
            Some(LeaderCandidate { socket })
        })
        .collect())
}

#[cfg(target_os = "linux")]
fn is_frame_cap_error(error: &Error) -> bool {
    matches!(error, Error::Command(message) if message.contains("64 MiB"))
}

#[cfg(target_os = "linux")]
fn is_definitively_unreachable_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
    )
}

#[cfg(target_os = "linux")]
struct LeaderClient {
    stream: std::os::unix::net::UnixStream,
}

#[cfg(target_os = "linux")]
impl LeaderClient {
    fn connect(socket: &Path, model: Option<&str>) -> Result<LeaderClient> {
        let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
        verify_peer_uid(&stream)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(3)))?;
        write_frame(
            &mut stream,
            &json!({
                "type": "register",
                "client_type": "agent-viewer",
                "mode": "stdio",
                "capabilities": {"default_model": model, "yolo_mode": true},
            }),
        )?;
        let registered = read_frame(&mut stream)?;
        if registered.get("type").and_then(Value::as_str) != Some("registered") {
            return Err(Error::Command("Grok leader registration failed".into()));
        }
        let protocol = registered
            .get("leader_protocol_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Command("Grok leader omitted its protocol version".into()))?;
        let control = registered
            .pointer("/leader_capabilities/control_v1")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if protocol < 1 || !control {
            return Err(Error::Command(
                "Grok leader does not support control protocol version 1".into(),
            ));
        }
        if registered.get("ready").and_then(Value::as_bool) == Some(false) {
            stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
            let ready = read_frame(&mut stream)?;
            if ready.get("type").and_then(Value::as_str) != Some("leader_ready") {
                return Err(Error::Command("Grok leader did not become ready".into()));
            }
            stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        }
        Ok(LeaderClient { stream })
    }

    fn probe(&mut self) -> Result<()> {
        let request_id = format!(
            "agent-viewer-control-{}",
            NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        write_frame(
            &mut self.stream,
            &json!({
                "type": "control",
                "request_id": request_id,
                "command": {"type": "get_leader_info"},
            }),
        )?;
        for _ in 0..1024 {
            let response = read_frame(&mut self.stream)?;
            if response.get("type").and_then(Value::as_str) != Some("control_result")
                || response.get("request_id").and_then(Value::as_str) != Some(request_id.as_str())
            {
                continue;
            }
            if response.pointer("/result/Ok/type").and_then(Value::as_str) != Some("leader_info") {
                return Err(Error::Command(
                    "Grok leader rejected its identity probe".into(),
                ));
            }
            return Ok(());
        }
        Err(Error::Command(
            "Grok leader sent too many unrelated control messages".into(),
        ))
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
                "_meta": {
                    "clientType": "agent-viewer",
                    "clientVersion": env!("CARGO_PKG_VERSION"),
                },
            }),
        )?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send_request(method, params)?;
        self.response(id, method, None)
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        write_frame(
            &mut self.stream,
            &json!({
                "type": "acp",
                "payload": json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }).to_string(),
            }),
        )?;
        Ok(id)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        write_frame(
            &mut self.stream,
            &json!({
                "type": "acp",
                "payload": json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params,
                }).to_string(),
            }),
        )
    }

    fn ext_request(&mut self, method: &str, params: Value) -> Result<Value> {
        let response = self.request(&format!("_{method}"), params)?;
        if response.get("error").is_some_and(|error| !error.is_null()) {
            return Err(Error::Command(format!("Grok {method} failed")));
        }
        Ok(response)
    }

    fn response(
        &mut self,
        id: u64,
        method: &str,
        pending_error: Option<(u64, &str)>,
    ) -> Result<Value> {
        for _ in 0..4096 {
            let outer = read_frame(&mut self.stream)?;
            if outer.get("type").and_then(Value::as_str) != Some("acp") {
                continue;
            }
            let payload = outer
                .get("payload")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Command("Grok leader returned malformed ACP".into()))?;
            let message: Value = serde_json::from_str(payload)?;
            if message.get("method").and_then(Value::as_str) == Some("session/request_permission")
                && let Some(reverse_id) = message.get("id").cloned()
            {
                write_frame(
                    &mut self.stream,
                    &json!({
                        "type": "acp",
                        "payload": json!({
                            "jsonrpc": "2.0",
                            "id": reverse_id,
                            "result": {"outcome": {"outcome": "cancelled"}},
                        }).to_string(),
                    }),
                )?;
                continue;
            }
            if let Some((pending_id, pending_method)) = pending_error
                && message.get("id").and_then(Value::as_u64) == Some(pending_id)
            {
                if let Some(error) = message.get("error").filter(|error| !error.is_null()) {
                    return Err(request_error(pending_method, error));
                }
                continue;
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error").filter(|error| !error.is_null()) {
                return Err(request_error(method, error));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| Error::Command(format!("Grok {method} returned no result")));
        }
        Err(Error::Command(format!(
            "Grok {method} received too many unrelated messages"
        )))
    }
}

#[cfg(target_os = "linux")]
fn request_error(method: &str, error: &Value) -> Error {
    let detail = error
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty());
    Error::Command(match detail {
        Some(detail) => format!(
            "Grok {method} request failed: {}",
            sanitize_terminal_text(detail)
        ),
        None => format!("Grok {method} request failed"),
    })
}

#[cfg(target_os = "linux")]
fn verify_peer_uid(stream: &std::os::unix::net::UnixStream) -> Result<()> {
    use std::os::fd::AsRawFd as _;

    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(Error::Command(
            "Grok leader returned malformed peer credentials".into(),
        ));
    }
    let credentials = unsafe { credentials.assume_init() };
    if !peer_uid_matches(credentials.uid, unsafe { libc::geteuid() }) {
        return Err(Error::Command(
            "Grok leader peer is not owned by the current user".into(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn peer_uid_matches(peer_uid: libc::uid_t, effective_uid: libc::uid_t) -> bool {
    peer_uid == effective_uid
}

#[cfg(target_os = "linux")]
static NEXT_REQUEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(target_os = "linux")]
fn read_frame(stream: &mut std::os::unix::net::UnixStream) -> Result<Value> {
    use std::io::Read;

    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(Error::Command(
            "Grok leader frame exceeds the official 64 MiB limit".into(),
        ));
    }
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(target_os = "linux")]
fn write_frame(stream: &mut std::os::unix::net::UnixStream, value: &Value) -> Result<()> {
    use std::io::Write;

    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(Error::Command(
            "Grok leader frame exceeds the official 64 MiB limit".into(),
        ));
    }
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn peer_uid_equality_accepts_current_and_rejects_foreign_safely() {
        let current = unsafe { libc::geteuid() };
        let foreign = current.wrapping_add(1);
        assert_ne!(foreign, current);
        assert!(peer_uid_matches(current, current));
        assert!(!peer_uid_matches(foreign, current));

        let rejection = "Grok leader peer is not owned by the current user";
        assert_eq!(sanitize_terminal_text(rejection), rejection);
    }
}
