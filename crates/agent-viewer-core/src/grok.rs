use crate::backend::{
    Backend, BackendKind, Capabilities, Session, SessionOrigin, SpawnResult, Status, TailEvent,
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
        #[cfg(unix)]
        let (leader_count, registered) = {
            let (count, clients) = self.connect_existing(None, false)?;
            (count, !clients.is_empty())
        };
        #[cfg(not(unix))]
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
        let mut sessions = read_durable_sessions(&self.home)?;
        #[cfg(unix)]
        if let Ok((_, mut clients)) = self.connect_existing(None, true) {
            for client in &mut clients {
                if let Ok(roster) = client.ext_request("x.ai/sessions/list", json!({})) {
                    merge_roster(&mut sessions, &roster);
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

    pub fn spawn(&self, cwd: &Path, task: &str, model: Option<&str>) -> Result<SpawnResult> {
        #[cfg(unix)]
        {
            let mut client = self.connect_or_start(model)?;
            let response = client.request("session/new", json!({"cwd": cwd, "mcpServers": []}))?;
            let session_id = response
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| Error::Command("Grok session/new returned no identity".into()))?
                .to_string();
            client.send_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": task}],
                }),
            )?;
            let _ = client.ext_request("x.ai/sessions/list", json!({}));
            Ok(SpawnResult {
                pid: None,
                session_id: Some(session_id),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (cwd, task, model);
            Err(Error::Unsupported(BackendKind::Grok.name()))
        }
    }

    pub fn cancel(&self, session_id: &str) -> Result<()> {
        require_session_id(session_id)?;
        #[cfg(unix)]
        {
            let mut client = self.connect_or_start(None)?;
            client.notify("session/cancel", json!({"sessionId": session_id}))?;
            let _ = client.ext_request("x.ai/sessions/list", json!({}));
            Ok(())
        }
        #[cfg(not(unix))]
        Err(Error::Unsupported(BackendKind::Grok.name()))
    }

    pub fn rename(&self, session_id: &str, title: &str) -> Result<()> {
        require_session_id(session_id)?;
        #[cfg(unix)]
        {
            let mut client = self.connect_or_start(None)?;
            client.ext_request(
                "x.ai/session/rename",
                json!({"sessionId": session_id, "title": title}),
            )?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = title;
            Err(Error::Unsupported(BackendKind::Grok.name()))
        }
    }

    pub fn delete(&self, session_id: &str) -> Result<()> {
        require_session_id(session_id)?;
        #[cfg(unix)]
        {
            let mut client = self.connect_or_start(None)?;
            client.ext_request("x.ai/session/delete", json!({"sessionId": session_id}))?;
            Ok(())
        }
        #[cfg(not(unix))]
        Err(Error::Unsupported(BackendKind::Grok.name()))
    }

    pub fn models(&self) -> Result<Vec<String>> {
        let fallback = || vec![BackendKind::Grok.default_model().to_string()];
        #[cfg(unix)]
        {
            let Ok((_, mut clients)) = self.connect_existing(None, true) else {
                return Ok(fallback());
            };
            for client in &mut clients {
                let Ok(value) = client.ext_request("x.ai/models/list", json!({})) else {
                    continue;
                };
                let mut models = fallback();
                if let Some(current) = value.get("currentModelId").and_then(Value::as_str) {
                    models.push(current.to_string());
                }
                if let Some(available) = value.get("availableModels").and_then(Value::as_array) {
                    models.extend(available.iter().filter_map(|entry| {
                        entry
                            .get("modelId")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    }));
                }
                return Ok(crate::backend::dedup_preserve(models));
            }
        }
        Ok(fallback())
    }

    #[cfg(unix)]
    fn connect_existing(
        &self,
        model: Option<&str>,
        initialize: bool,
    ) -> Result<(usize, Vec<LeaderClient>)> {
        let candidates = leader_candidates(&self.home)?;
        let count = candidates.len();
        let mut clients = Vec::new();
        let mut first_error = None;
        for candidate in candidates {
            let Some(socket) = candidate.socket else {
                continue;
            };
            let connected = LeaderClient::connect(&socket, model).and_then(|mut client| {
                client.probe()?;
                if initialize {
                    client.initialize()?;
                }
                Ok(client)
            });
            match connected {
                Ok(client) => clients.push(client),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if clients.is_empty()
            && let Some(error) = first_error
        {
            return Err(error);
        }
        Ok((count, clients))
    }

    #[cfg(unix)]
    fn connect_or_start(&self, model: Option<&str>) -> Result<LeaderClient> {
        match self.connect_existing(model, true) {
            Ok((_, mut clients)) if !clients.is_empty() => return Ok(clients.remove(0)),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        self.start_leader()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match self.connect_existing(model, true) {
                Ok((_, mut clients)) if !clients.is_empty() => return Ok(clients.remove(0)),
                Err(error) if std::time::Instant::now() >= deadline => return Err(error),
                _ if std::time::Instant::now() >= deadline => {
                    return Err(Error::Command(
                        "official Grok leader did not become reachable".into(),
                    ));
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    }

    #[cfg(unix)]
    fn start_leader(&self) -> Result<()> {
        if !binary_available(&self.binary) {
            return Err(Error::Command(
                "official grok binary is unavailable".to_string(),
            ));
        }
        let stable_cwd = if self.home.is_dir() {
            self.home.as_path()
        } else {
            self.home
                .parent()
                .filter(|parent| parent.is_dir())
                .unwrap_or_else(|| Path::new("/"))
        };
        std::process::Command::new(&self.binary)
            .arg("agent")
            .arg("leader")
            .arg("--no-exit-on-disconnect")
            .arg("--relay-on-demand")
            .env("GROK_HOME", &self.home)
            .current_dir(stable_cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
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
        Capabilities {
            spawn: true,
            attach: true,
            rename: true,
            archive: false,
            delete: true,
            stop: true,
        }
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
        command.arg("--resume").arg(&session.id);
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
    #[serde(rename = "num_messages")]
    _num_messages: usize,
    #[serde(rename = "current_model_id")]
    _current_model_id: String,
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
    #[serde(default)]
    head_branch: Option<String>,
}

fn read_durable_sessions(home: &Path) -> Result<Vec<Session>> {
    let root = home.join("sessions");
    let cwd_entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut sessions = Vec::new();
    for cwd_entry in cwd_entries.flatten() {
        let cwd_path = cwd_entry.path();
        if !cwd_path.is_dir() {
            continue;
        }
        let Ok(session_entries) = std::fs::read_dir(&cwd_path) else {
            continue;
        };
        for session_entry in session_entries.flatten() {
            let session_dir = session_entry.path();
            if !session_dir.is_dir() {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(session_dir.join("summary.json")) else {
                continue;
            };
            let Ok(summary) = serde_json::from_str::<DurableSummary>(&body) else {
                continue;
            };
            if summary.info.id.trim().is_empty()
                || summary.info.cwd.trim().is_empty()
                || session_dir.file_name().and_then(|name| name.to_str())
                    != Some(summary.info.id.as_str())
            {
                continue;
            }
            let default_hidden = summary
                .session_kind
                .as_deref()
                .is_some_and(|kind| kind.starts_with("subagent"));
            if summary.hidden.unwrap_or(default_hidden) {
                continue;
            }
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
            let history = session_dir.join("chat_history.jsonl");
            sessions.push(Session {
                backend: BackendKind::Grok,
                id: summary.info.id.clone(),
                short_id: None,
                origin: SessionOrigin::Interactive,
                title: if title.is_empty() {
                    summary.info.id
                } else {
                    title.to_string()
                },
                cwd: PathBuf::from(summary.info.cwd),
                git_branch: summary.head_branch.filter(|branch| !branch.is_empty()),
                status: Status::Unknown,
                created_at_ms,
                updated_at_ms,
                hidden: false,
                companion: false,
                subagent: default_hidden,
                summary: summary
                    .last_turn_summary
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or(summary.session_summary),
                pid: None,
                rollout_path: history.is_file().then_some(history),
                pr_refs: Vec::new(),
                daemon_hosted: false,
            });
        }
    }
    Ok(sessions)
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

fn merge_roster(sessions: &mut Vec<Session>, response: &Value) {
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
        let status = match entry.activity.as_str() {
            "working" => Status::Working,
            "idle" => Status::Idle,
            "needs_input" => Status::NeedsInput { reason: None },
            "completed" => Status::Done,
            "dead" => Status::Error,
            "dormant" => Status::Unknown,
            _ => Status::Unknown,
        };
        if let Some(session) = sessions.iter_mut().find(|row| row.id == entry.session_id) {
            if let Some(title) = entry.title.filter(|title| !title.trim().is_empty()) {
                session.title = title;
            }
            session.cwd = PathBuf::from(entry.cwd);
            session.status = status;
            session.updated_at_ms = entry.last_change_unix_ms;
            if let Some(summary) = entry
                .last_turn_summary
                .filter(|summary| !summary.trim().is_empty())
            {
                session.summary = summary;
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
            title,
            cwd: PathBuf::from(entry.cwd),
            git_branch: None,
            status,
            created_at_ms: entry.last_change_unix_ms,
            updated_at_ms: entry.last_change_unix_ms,
            hidden: false,
            companion: false,
            subagent: false,
            summary: entry.last_turn_summary.unwrap_or_default(),
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
    let body = crate::read_tail_window(path, crate::TRANSCRIPT_TAIL_BYTES)?;
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
                let text = crate::backend::squash(&text);
                if !text.is_empty() {
                    events.push(TailEvent::User(text));
                }
            }
            Some("assistant") => {
                if let Some(text) = value.get("content").and_then(Value::as_str) {
                    let text = crate::backend::squash(text);
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
                            name: name.to_string(),
                            detail: arguments
                                .as_ref()
                                .map(crate::backend::tool_detail)
                                .unwrap_or_default(),
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

#[cfg(unix)]
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[cfg(unix)]
#[derive(Default)]
struct LeaderCandidate {
    socket: Option<PathBuf>,
    _lock: Option<PathBuf>,
}

#[cfg(unix)]
fn leader_candidates(home: &Path) -> Result<Vec<LeaderCandidate>> {
    use std::collections::BTreeMap;

    let entries = match std::fs::read_dir(home) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut candidates: BTreeMap<String, LeaderCandidate> = BTreeMap::new();
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
            candidates.entry(suffix.to_string()).or_default()._lock = Some(path);
        }
    }
    Ok(candidates.into_values().collect())
}

#[cfg(unix)]
struct LeaderClient {
    stream: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl LeaderClient {
    fn connect(socket: &Path, model: Option<&str>) -> Result<LeaderClient> {
        let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(3)))?;
        write_frame(
            &mut stream,
            &json!({
                "type": "register",
                "client_type": "agent-viewer",
                "mode": "stdio",
                "capabilities": {"default_model": model},
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
                    "startupHints": {
                        "nonInteractive": true,
                        "skipGitStatus": true,
                        "skipProjectLayout": true,
                    },
                },
            }),
        )?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
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
        self.response(id, method)
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<()> {
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
        )
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
        let response = self.request(
            &format!("_{method}"),
            json!({"method": method, "params": params}),
        )?;
        if response.get("error").is_some_and(|error| !error.is_null()) {
            return Err(Error::Command(format!("Grok {method} failed")));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| Error::Command(format!("Grok {method} returned no result")))
    }

    fn response(&mut self, id: u64, method: &str) -> Result<Value> {
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
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if message.get("error").is_some() {
                return Err(Error::Command(format!("Grok {method} request failed")));
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

#[cfg(unix)]
static NEXT_REQUEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(unix)]
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

#[cfg(unix)]
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
