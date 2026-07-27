use crate::backend::{Backend, BackendKind, Capabilities, Session, SessionOrigin, Status};
use crate::codex::rollout::TranscriptItem;
use crate::error::{AttachRefusal, Error, Result};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(20);

type Launcher = dyn Fn(Command) -> io::Result<()> + Send + Sync;

#[derive(Clone)]
pub struct OpencodeRuntime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    candidates: [SocketAddr; 2],
    startup_timeout: Duration,
    durable_cwd: PathBuf,
    launcher: Arc<Launcher>,
    agent: ureq::Agent,
    authorization: Option<String>,
    state: Mutex<RuntimeState>,
}

#[derive(Default)]
struct RuntimeState {
    endpoint: Option<SocketAddr>,
    last_failure: Option<String>,
}

struct HttpReply {
    status: u16,
    body: String,
}

#[derive(Deserialize)]
struct HealthResponse {
    healthy: bool,
    version: String,
}

impl OpencodeRuntime {
    pub fn new() -> Self {
        let home = crate::home_dir();
        let durable_cwd = if !home.as_os_str().is_empty() && home.is_dir() {
            home
        } else {
            PathBuf::from("/")
        };
        Self::from_parts(
            [
                SocketAddr::from(([127, 0, 0, 1], 4096)),
                SocketAddr::from(([127, 0, 0, 1], 4097)),
            ],
            SERVER_STARTUP_TIMEOUT,
            durable_cwd,
            Arc::new(|command| {
                let log_path = crate::spawn::viewer_log_path("opencode-server");
                crate::spawn::spawn_detached(command, &log_path)
                    .map(|_| ())
                    .map_err(|error| io::Error::other(error.to_string()))
            }),
        )
    }

    #[doc(hidden)]
    pub fn for_test<L>(
        candidates: [SocketAddr; 2],
        startup_timeout: Duration,
        durable_cwd: PathBuf,
        launcher: L,
    ) -> Self
    where
        L: Fn(Command) -> io::Result<()> + Send + Sync + 'static,
    {
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.ip().is_loopback())
        );
        Self::from_parts(candidates, startup_timeout, durable_cwd, Arc::new(launcher))
    }

    fn from_parts(
        candidates: [SocketAddr; 2],
        startup_timeout: Duration,
        durable_cwd: PathBuf,
        launcher: Arc<Launcher>,
    ) -> Self {
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .redirects(0)
            .timeout_connect(HTTP_REQUEST_TIMEOUT)
            .timeout_read(HTTP_REQUEST_TIMEOUT)
            .timeout_write(HTTP_REQUEST_TIMEOUT)
            .build();
        let authorization = std::env::var("OPENCODE_SERVER_PASSWORD")
            .ok()
            .map(|password| {
                let username = std::env::var("OPENCODE_SERVER_USERNAME")
                    .unwrap_or_else(|_| "opencode".to_string());
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(format!("{username}:{password}").as_bytes());
                format!("Basic {encoded}")
            });
        Self {
            inner: Arc::new(RuntimeInner {
                candidates,
                startup_timeout,
                durable_cwd,
                launcher,
                agent,
                authorization,
                state: Mutex::new(RuntimeState::default()),
            }),
        }
    }

    fn compatibility_only() -> Self {
        Self::for_test(
            [
                SocketAddr::from(([127, 0, 0, 1], 0)),
                SocketAddr::from(([127, 0, 0, 1], 0)),
            ],
            Duration::from_millis(20),
            PathBuf::from("/"),
            |_| Err(io::Error::other("compatibility runtime cannot launch")),
        )
    }

    fn state(&self) -> MutexGuard<'_, RuntimeState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn health_timeout(&self) -> Duration {
        self.inner
            .startup_timeout
            .min(HEALTH_PROBE_TIMEOUT)
            .max(Duration::from_millis(1))
    }

    fn request(
        &self,
        endpoint: SocketAddr,
        method: &str,
        target: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> std::result::Result<HttpReply, String> {
        let url = format!("http://{endpoint}{target}");
        let mut request = self
            .inner
            .agent
            .request(method, &url)
            .timeout(timeout.max(Duration::from_millis(1)));
        if let Some(authorization) = &self.inner.authorization {
            request = request.set("Authorization", authorization);
        }
        let response = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        };
        match response {
            Ok(response) | Err(ureq::Error::Status(_, response)) => {
                let status = response.status();
                let body = response
                    .into_string()
                    .map_err(|error| format!("{method} {target} response unreadable: {error}"))?;
                Ok(HttpReply { status, body })
            }
            Err(error) => Err(format!("{method} {target} failed: {error}")),
        }
    }

    fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: SocketAddr,
        method: &str,
        target: &str,
        body: Option<&Value>,
        expected_status: u16,
    ) -> Result<T> {
        let reply = self
            .request(endpoint, method, target, body, HTTP_REQUEST_TIMEOUT)
            .map_err(Error::Command)?;
        if reply.status != expected_status {
            return Err(Error::Command(http_status_error(
                method,
                target,
                reply.status,
                &reply.body,
            )));
        }
        serde_json::from_str(&reply.body).map_err(|error| {
            Error::Command(format!("{method} {target} returned invalid JSON: {error}"))
        })
    }

    fn expect_status(
        &self,
        endpoint: SocketAddr,
        method: &str,
        target: &str,
        body: Option<&Value>,
        expected_status: u16,
    ) -> Result<()> {
        let reply = self
            .request(endpoint, method, target, body, HTTP_REQUEST_TIMEOUT)
            .map_err(Error::Command)?;
        if reply.status == expected_status {
            Ok(())
        } else {
            Err(Error::Command(http_status_error(
                method,
                target,
                reply.status,
                &reply.body,
            )))
        }
    }

    fn probe_health(
        &self,
        candidate: SocketAddr,
        timeout: Duration,
    ) -> std::result::Result<(), String> {
        let reply = self.request(
            candidate,
            "GET",
            "/global/health",
            None,
            timeout.min(self.health_timeout()),
        )?;
        if reply.status != 200 {
            return Err(http_status_error(
                "GET",
                "/global/health",
                reply.status,
                &reply.body,
            ));
        }
        let health: HealthResponse = serde_json::from_str(&reply.body)
            .map_err(|error| format!("invalid health JSON: {error}"))?;
        if health.healthy && !health.version.is_empty() {
            Ok(())
        } else {
            Err("health response did not contain healthy true and a version".to_string())
        }
    }

    fn probe_candidates(
        &self,
        deadline: Option<Instant>,
    ) -> (Option<SocketAddr>, Vec<(SocketAddr, String)>) {
        let mut failures = Vec::new();
        for (index, candidate) in self.inner.candidates.iter().copied().enumerate() {
            let timeout = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        failures.push((candidate, "startup deadline elapsed".to_string()));
                        continue;
                    }
                    let slots = (self.inner.candidates.len() - index) as u32;
                    (remaining / slots)
                        .min(self.health_timeout())
                        .max(Duration::from_millis(1))
                }
                None => self.health_timeout(),
            };
            match self.probe_health(candidate, timeout) {
                Ok(()) => return (Some(candidate), failures),
                Err(error) => failures.push((candidate, error)),
            }
        }
        (None, failures)
    }

    fn probe(&self) -> Option<SocketAddr> {
        let mut state = self.state();
        if let Some(endpoint) = state.endpoint {
            state.last_failure = self
                .probe_health(endpoint, self.health_timeout())
                .err()
                .map(|error| format!("port {}: {error}", endpoint.port()));
            return Some(endpoint);
        }
        let (endpoint, failures) = self.probe_candidates(None);
        state.endpoint = endpoint;
        state.last_failure = endpoint
            .is_none()
            .then(|| format_candidate_failures(&failures));
        endpoint
    }

    fn ensure_server(&self) -> std::result::Result<SocketAddr, String> {
        let mut state = self.state();
        if let Some(endpoint) = state.endpoint {
            state.last_failure = self
                .probe_health(endpoint, self.health_timeout())
                .err()
                .map(|error| format!("port {}: {error}", endpoint.port()));
            return Ok(endpoint);
        }
        let deadline = Instant::now() + self.inner.startup_timeout;
        let (endpoint, mut failures) = self.probe_candidates(Some(deadline));
        if let Some(endpoint) = endpoint {
            state.endpoint = Some(endpoint);
            state.last_failure = None;
            return Ok(endpoint);
        }

        for (index, candidate) in self.inner.candidates.iter().copied().enumerate() {
            if Instant::now() >= deadline {
                note_candidate_failure(
                    &mut failures,
                    candidate,
                    "startup deadline elapsed".to_string(),
                );
                break;
            }
            match TcpListener::bind(candidate) {
                Ok(listener) => drop(listener),
                Err(error) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let candidates_left = (self.inner.candidates.len() - index) as u32;
                    let candidate_deadline = Instant::now() + remaining / candidates_left;
                    let mut health_error = "server did not become healthy".to_string();
                    while Instant::now() < candidate_deadline {
                        let remaining =
                            candidate_deadline.saturating_duration_since(Instant::now());
                        match self.probe_health(candidate, remaining.min(self.health_timeout())) {
                            Ok(()) => {
                                state.endpoint = Some(candidate);
                                state.last_failure = None;
                                return Ok(candidate);
                            }
                            Err(error) => health_error = error,
                        }
                        std::thread::sleep(
                            READINESS_POLL_INTERVAL
                                .min(candidate_deadline.saturating_duration_since(Instant::now())),
                        );
                    }
                    note_candidate_failure(
                        &mut failures,
                        candidate,
                        format!("port is occupied ({error}); health failed: {health_error}"),
                    );
                    continue;
                }
            }

            let mut command = Command::new("opencode");
            command
                .arg("serve")
                .arg("--hostname")
                .arg("127.0.0.1")
                .arg("--port")
                .arg(candidate.port().to_string())
                .current_dir(&self.inner.durable_cwd);
            if let Err(error) = (self.inner.launcher)(command) {
                note_candidate_failure(
                    &mut failures,
                    candidate,
                    format!("could not start server: {error}"),
                );
                continue;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            let candidates_left = (self.inner.candidates.len() - index) as u32;
            let candidate_deadline = Instant::now() + remaining / candidates_left;
            let mut last_health_error = "server did not become healthy".to_string();
            while Instant::now() < candidate_deadline {
                let remaining = candidate_deadline.saturating_duration_since(Instant::now());
                match self.probe_health(candidate, remaining.min(self.health_timeout())) {
                    Ok(()) => {
                        state.endpoint = Some(candidate);
                        state.last_failure = None;
                        return Ok(candidate);
                    }
                    Err(error) => last_health_error = error,
                }
                std::thread::sleep(
                    READINESS_POLL_INTERVAL
                        .min(candidate_deadline.saturating_duration_since(Instant::now())),
                );
            }
            note_candidate_failure(
                &mut failures,
                candidate,
                format!("readiness timed out: {last_health_error}"),
            );
        }

        let failure = format_candidate_failures(&failures);
        state.endpoint = None;
        state.last_failure = Some(failure.clone());
        Err(failure)
    }

    fn healthy_tier(&self) -> bool {
        self.state().endpoint.is_some()
    }

    fn require_server(&self) -> Result<SocketAddr> {
        self.probe()
            .ok_or(Error::Unsupported(BackendKind::Opencode.name()))
    }

    pub fn read_last_message(
        &self,
        db_path: &Path,
        session_id: &str,
    ) -> Result<Option<TranscriptItem>> {
        let Some(endpoint) = self.probe() else {
            return read_opencode_last_message(db_path, session_id);
        };
        let target = format!(
            "/session/{}/message?limit=200",
            percent_encode(session_id.as_bytes())
        );
        let messages: Vec<Value> = self.request_json(endpoint, "GET", &target, None, 200)?;
        Ok(last_server_message(messages))
    }
}

impl Default for OpencodeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

pub struct OpencodeBackend {
    db_path: PathBuf,
    runtime: OpencodeRuntime,
    models_cache: OnceLock<Vec<String>>,
}

impl OpencodeBackend {
    pub fn new() -> Self {
        Self::with_runtime(OpencodeRuntime::new())
    }

    pub fn with_runtime(runtime: OpencodeRuntime) -> Self {
        Self::with_db_and_runtime(default_opencode_db(), runtime)
    }

    pub fn with_db(db_path: PathBuf) -> Self {
        Self::with_db_and_runtime(db_path, OpencodeRuntime::compatibility_only())
    }

    pub fn with_db_and_runtime(db_path: PathBuf, runtime: OpencodeRuntime) -> Self {
        Self {
            db_path,
            runtime,
            models_cache: OnceLock::new(),
        }
    }

    fn list_from_sqlite(&self) -> Result<Vec<Session>> {
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }
        let conn = crate::open_readonly(&self.db_path)?;
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
                cwd: PathBuf::from(row.get::<_, String>(2)?),
                git_branch: None,
                status: opencode_status(live, time_updated, now),
                created_at_ms: row.get(4)?,
                updated_at_ms: time_updated,
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

    fn list_from_server(&self, endpoint: SocketAddr) -> Result<Vec<Session>> {
        let sessions: Vec<ServerSession> =
            self.runtime
                .request_json(endpoint, "GET", "/session?limit=10000", None, 200)?;
        let directories = sessions
            .iter()
            .filter(|session| session.is_active_managed())
            .map(|session| session.directory.as_str())
            .collect::<BTreeSet<_>>();
        let mut statuses = HashMap::new();
        let mut permissions = Vec::new();
        let mut questions = Vec::new();
        for directory in directories {
            let directory = percent_encode(directory.as_bytes());
            statuses.extend(self.runtime.request_json::<HashMap<String, ServerStatus>>(
                endpoint,
                "GET",
                &format!("/session/status?directory={directory}"),
                None,
                200,
            )?);
            permissions.extend(self.runtime.request_json::<Vec<PermissionRequest>>(
                endpoint,
                "GET",
                &format!("/permission?directory={directory}"),
                None,
                200,
            )?);
            questions.extend(self.runtime.request_json::<Vec<QuestionRequest>>(
                endpoint,
                "GET",
                &format!("/question?directory={directory}"),
                None,
                200,
            )?);
        }

        let mut input_reasons = HashMap::new();
        for permission in permissions {
            let reason = permission
                .patterns
                .first()
                .map(|pattern| format!("{}: {pattern}", permission.permission))
                .unwrap_or(permission.permission);
            input_reasons.insert(permission.session_id, reason);
        }
        for question in questions {
            if let Some(reason) = question
                .questions
                .first()
                .map(|question| question.question.clone())
            {
                input_reasons.insert(question.session_id, reason);
            }
        }

        let mut listed = sessions
            .into_iter()
            .map(|session| {
                let status = if session.is_active_managed() {
                    input_reasons
                        .get(&session.id)
                        .cloned()
                        .map(|reason| Status::NeedsInput {
                            reason: Some(reason),
                        })
                        .unwrap_or_else(|| {
                            match statuses.get(&session.id).map(|status| status.kind.as_str()) {
                                Some("busy" | "retry") => Status::Working,
                                _ => Status::Idle,
                            }
                        })
                } else {
                    Status::Idle
                };
                Session {
                    backend: BackendKind::Opencode,
                    id: session.id,
                    short_id: None,
                    origin: SessionOrigin::Interactive,
                    title: session.title,
                    cwd: PathBuf::from(session.directory),
                    git_branch: None,
                    status,
                    created_at_ms: session.time.created,
                    updated_at_ms: session.time.updated,
                    hidden: session.time.archived.unwrap_or(0.0) > 0.0,
                    companion: session.parent_id.is_some()
                        || session
                            .permission
                            .as_ref()
                            .is_some_and(server_permission_is_run_mode),
                    summary: String::new(),
                    pid: None,
                    rollout_path: None,
                    pr_refs: Vec::new(),
                    daemon_hosted: true,
                }
            })
            .collect::<Vec<_>>();
        listed.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
        Ok(listed)
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
        if self.runtime.healthy_tier() {
            server_capabilities()
        } else {
            compatibility_capabilities()
        }
    }

    fn capabilities_for(&self, session: &Session) -> Capabilities {
        if session.daemon_hosted && self.runtime.healthy_tier() {
            server_capabilities()
        } else {
            compatibility_capabilities()
        }
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        match self.runtime.probe() {
            Some(endpoint) => self.list_from_server(endpoint),
            None => self.list_from_sqlite(),
        }
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
        let model = selected_model(model)?;
        let endpoint = self
            .runtime
            .ensure_server()
            .map_err(|error| Error::Command(format!("spawn failed: {error}")))?;
        let directory = percent_encode(dir.as_os_str().as_bytes());
        let mut create = json!({
            "title": crate::spawn::truncated_title(task),
            "metadata": {"agent-viewer": {"background": true}}
        });
        if let Some(model) = &model {
            create["model"] = json!({"providerID": model.provider, "id": model.id});
        }
        let create_target = format!("/session?directory={directory}");
        let created: CreatedSession =
            self.runtime
                .request_json(endpoint, "POST", &create_target, Some(&create), 200)?;
        if created.id.is_empty() {
            return Err(Error::Command(
                "POST /session returned an empty session id".to_string(),
            ));
        }

        let mut prompt = json!({"parts": [{"type": "text", "text": task}]});
        if let Some(model) = &model {
            prompt["model"] = json!({"providerID": model.provider, "modelID": model.id});
        }
        let prompt_target = format!(
            "/session/{}/prompt_async?directory={directory}",
            percent_encode(created.id.as_bytes())
        );
        if let Err(error) =
            self.runtime
                .expect_status(endpoint, "POST", &prompt_target, Some(&prompt), 204)
        {
            return Err(Error::Command(format!(
                "session {} was created but prompt acceptance failed: {error}",
                created.id
            )));
        }
        Ok(crate::SpawnResult {
            pid: None,
            session_id: Some(created.id),
        })
    }

    fn stop(&self, session: &Session) -> Result<()> {
        if !session.daemon_hosted {
            return Err(Error::Unsupported(self.kind().name()));
        }
        let endpoint = self.runtime.require_server()?;
        let target = format!("/session/{}/abort", percent_encode(session.id.as_bytes()));
        let _: bool = self
            .runtime
            .request_json(endpoint, "POST", &target, None, 200)?;
        Ok(())
    }

    fn remove(&self, session: &Session) -> Result<()> {
        if let Some(endpoint) = self.runtime.probe() {
            let target = format!("/session/{}", percent_encode(session.id.as_bytes()));
            let _: Value = self
                .runtime
                .request_json(endpoint, "DELETE", &target, None, 200)?;
            return Ok(());
        }
        crate::spawn::run_checked(
            Command::new("opencode")
                .arg("session")
                .arg("delete")
                .arg(&session.id),
        )
    }

    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        let endpoint = self.runtime.require_server()?;
        let target = format!("/session/{}", percent_encode(session.id.as_bytes()));
        let body = json!({"title": name});
        self.runtime
            .expect_status(endpoint, "PATCH", &target, Some(&body), 200)
    }

    fn hide(&self, id: &str) -> Result<()> {
        let endpoint = self.runtime.require_server()?;
        let target = format!("/session/{}", percent_encode(id.as_bytes()));
        let body = json!({"time": {"archived": crate::spawn::now_ms()}});
        self.runtime
            .expect_status(endpoint, "PATCH", &target, Some(&body), 200)
    }

    fn unhide(&self, id: &str) -> Result<()> {
        let endpoint = self.runtime.require_server()?;
        let target = format!("/session/{}", percent_encode(id.as_bytes()));
        let body = json!({"time": {"archived": 0}});
        self.runtime
            .expect_status(endpoint, "PATCH", &target, Some(&body), 200)
    }

    fn attach_command(&self, session: &Session) -> std::result::Result<Command, AttachRefusal> {
        let mut command = Command::new("opencode");
        if session.daemon_hosted {
            let endpoint = self.runtime.probe().ok_or_else(|| {
                AttachRefusal::new("the OpenCode server that hosted this session is unavailable")
            })?;
            command
                .arg("attach")
                .arg(format!("http://{endpoint}"))
                .arg("-s")
                .arg(&session.id);
        } else {
            command.arg("-s").arg(&session.id);
        }
        if session.cwd.is_dir() {
            command.current_dir(&session.cwd);
        }
        Ok(command)
    }
}

#[derive(Deserialize)]
struct ServerSession {
    id: String,
    #[serde(default, rename = "parentID")]
    parent_id: Option<String>,
    directory: String,
    title: String,
    #[serde(default)]
    permission: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
    time: ServerSessionTime,
}

impl ServerSession {
    fn is_active_managed(&self) -> bool {
        self.time.archived.unwrap_or(0.0) <= 0.0
            && self
                .metadata
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|metadata| metadata.get("agent-viewer"))
                .and_then(Value::as_object)
                .and_then(|marker| marker.get("background"))
                .and_then(Value::as_bool)
                == Some(true)
    }
}

#[derive(Deserialize)]
struct ServerSessionTime {
    created: i64,
    updated: i64,
    #[serde(default)]
    archived: Option<f64>,
}

#[derive(Deserialize)]
struct ServerStatus {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct PermissionRequest {
    #[serde(rename = "sessionID")]
    session_id: String,
    permission: String,
    #[serde(default)]
    patterns: Vec<String>,
}

#[derive(Deserialize)]
struct QuestionRequest {
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(default)]
    questions: Vec<Question>,
}

#[derive(Deserialize)]
struct Question {
    question: String,
}

#[derive(Deserialize)]
struct CreatedSession {
    id: String,
}

struct SelectedModel {
    provider: String,
    id: String,
}

fn selected_model(model: Option<&str>) -> Result<Option<SelectedModel>> {
    let Some(model) = model.filter(|model| *model != "default") else {
        return Ok(None);
    };
    let Some((provider, id)) = model.split_once('/') else {
        return Err(Error::Command(
            "selected OpenCode model must include a provider and model id".to_string(),
        ));
    };
    if provider.is_empty() || id.is_empty() {
        return Err(Error::Command(
            "selected OpenCode model must include a provider and model id".to_string(),
        ));
    }
    Ok(Some(SelectedModel {
        provider: provider.to_string(),
        id: id.to_string(),
    }))
}

fn server_capabilities() -> Capabilities {
    Capabilities {
        spawn: true,
        attach: true,
        rename: true,
        archive: true,
        delete: true,
        stop: true,
        needs_input: true,
        pr_refs: false,
        live_status: true,
    }
}

fn compatibility_capabilities() -> Capabilities {
    Capabilities {
        spawn: true,
        attach: true,
        rename: false,
        archive: false,
        delete: true,
        stop: false,
        needs_input: false,
        pr_refs: false,
        live_status: false,
    }
}

fn note_candidate_failure(
    failures: &mut Vec<(SocketAddr, String)>,
    candidate: SocketAddr,
    reason: String,
) {
    if let Some(entry) = failures
        .iter_mut()
        .find(|(address, _)| *address == candidate)
    {
        entry.1 = reason;
    } else {
        failures.push((candidate, reason));
    }
}

fn format_candidate_failures(failures: &[(SocketAddr, String)]) -> String {
    failures
        .iter()
        .map(|(candidate, reason)| format!("port {}: {reason}", candidate.port()))
        .collect::<Vec<_>>()
        .join("; ")
}

fn http_status_error(method: &str, target: &str, status: u16, body: &str) -> String {
    let excerpt = body
        .chars()
        .take(512)
        .map(|character| {
            if character == '\r' || character == '\n' {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if excerpt.trim().is_empty() {
        format!("{method} {target} returned HTTP {status}")
    } else {
        format!("{method} {target} returned HTTP {status}: {excerpt}")
    }
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char);
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn server_permission_is_run_mode(permission: &Value) -> bool {
    let Value::Array(entries) = permission else {
        return false;
    };
    entries.iter().any(|entry| {
        entry.get("permission").and_then(Value::as_str) == Some("question")
            && entry.get("action").and_then(Value::as_str) == Some("deny")
    })
}

fn last_server_message(messages: Vec<Value>) -> Option<TranscriptItem> {
    messages
        .into_iter()
        .filter_map(|message| {
            let role = message
                .pointer("/info/role")
                .and_then(Value::as_str)
                .unwrap_or("assistant")
                .to_string();
            let created = message
                .pointer("/info/time/created")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let mut text = String::new();
            if let Some(parts) = message.get("parts").and_then(Value::as_array) {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(part_text) = part.get("text").and_then(Value::as_str)
                    {
                        text.push_str(part_text);
                    }
                }
            }
            if text.trim().is_empty() && role == "assistant" {
                text = message
                    .pointer("/info/error/data/message")
                    .or_else(|| message.pointer("/info/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            (!text.trim().is_empty()).then_some((created, TranscriptItem { role, text }))
        })
        .max_by_key(|(created, _)| *created)
        .map(|(_, item)| item)
}

pub fn default_opencode_db() -> PathBuf {
    crate::home_dir().join(".local/share/opencode/opencode.db")
}

fn opencode_models_via_cli() -> Vec<String> {
    let mut command = Command::new("opencode");
    command.arg("models");
    match crate::spawn::run_with_timeout(command, crate::spawn::MODEL_PROBE_TIMEOUT) {
        Some(stdout) => parse_opencode_models(&stdout),
        None => Vec::new(),
    }
}

pub fn parse_opencode_models(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn read_opencode_last_message(
    db_path: &Path,
    session_id: &str,
) -> Result<Option<TranscriptItem>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = crate::open_readonly(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT m.id, m.data, p.data FROM message m JOIN part p ON p.message_id = m.id \
         WHERE m.session_id = ?1 ORDER BY m.time_created DESC, m.id DESC, p.time_created ASC",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut current_id: Option<String> = None;
    let mut current_role = String::from("assistant");
    let mut current_text = String::new();
    for row in rows {
        let (message_id, message_data, part_data) = row?;
        if current_id.as_deref() != Some(message_id.as_str()) {
            if current_id.is_some() && !current_text.trim().is_empty() {
                return Ok(Some(TranscriptItem {
                    role: current_role,
                    text: current_text,
                }));
            }
            current_id = Some(message_id);
            current_role = parsed_role(&message_data);
            current_text.clear();
        }
        if let Some(text) = parsed_text_part(&part_data) {
            current_text.push_str(&text);
        }
    }
    if current_id.is_some() && !current_text.trim().is_empty() {
        return Ok(Some(TranscriptItem {
            role: current_role,
            text: current_text,
        }));
    }
    Ok(None)
}

fn parsed_role(message_data: &str) -> String {
    serde_json::from_str::<Value>(message_data)
        .ok()
        .as_ref()
        .and_then(|value| crate::json_str(value, "role").map(str::to_string))
        .unwrap_or_else(|| "assistant".to_string())
}

fn parsed_text_part(part_data: &str) -> Option<String> {
    let value: Value = serde_json::from_str(part_data).ok()?;
    if crate::json_str(&value, "type") != Some("text") {
        return None;
    }
    crate::json_str(&value, "text").map(str::to_string)
}

fn live_opencode_proc() -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system
        .processes()
        .values()
        .any(|process| process.name().to_string_lossy().starts_with("opencode"))
}

const WORKING_MAX_AGE_MS: i64 = 60_000;
const IDLE_MAX_AGE_MS: i64 = 1_800_000;

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

pub fn is_run_mode_permission(permission: Option<&str>) -> bool {
    let Some(raw) = permission.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    server_permission_is_run_mode(&value)
}
