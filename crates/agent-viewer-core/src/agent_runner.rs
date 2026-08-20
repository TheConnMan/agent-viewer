use crate::backend::{Backend, BackendKind, Capabilities, Session, SessionOrigin, SpawnResult};
use crate::error::{AttachRefusal, Error, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const COMPATIBLE_CODEX_VERSION: &str = "0.146.0";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_ID_BYTES: usize = 256;
const MAX_ENDPOINT_BYTES: usize = 4096;
const MAX_TIMESTAMP_BYTES: usize = 128;
const MAX_CURSOR_BYTES: usize = 4096;
const LIST_PAGE_LIMIT: &str = "200";

pub struct AgentRunnerBackend {
    binary: PathBuf,
}

pub struct PreparedAttach {
    command: Option<Command>,
    binary: PathBuf,
    lease_id: String,
    release_on_drop: bool,
}

impl PreparedAttach {
    pub fn into_commands(mut self) -> (Command, Command) {
        self.release_on_drop = false;
        let command = self.command.take().expect("prepared attach command");
        let release = release_command(&self.binary, &self.lease_id);
        (command, release)
    }
}

impl Drop for PreparedAttach {
    fn drop(&mut self) {
        if self.release_on_drop {
            let _ = release_lease(&self.binary, &self.lease_id);
        }
    }
}

impl AgentRunnerBackend {
    pub fn new() -> AgentRunnerBackend {
        AgentRunnerBackend::with_binary("agent-runner")
    }

    pub fn with_binary(binary: impl Into<PathBuf>) -> AgentRunnerBackend {
        AgentRunnerBackend {
            binary: binary.into(),
        }
    }

    pub fn prepare_attach(
        &self,
        session: &Session,
    ) -> std::result::Result<PreparedAttach, AttachRefusal> {
        if session.backend != BackendKind::AgentRunner {
            return Err(AttachRefusal::new("Agent Runner attach target is invalid"));
        }
        let output = Command::new(&self.binary)
            .args(["--json", "run", "attach", &session.id])
            .output()
            .map_err(|error| AttachRefusal::new(command_start_refusal(error)))?;
        if !output.status.success() {
            return Err(AttachRefusal::new(api_error(&output)));
        }
        let stdout = bounded_output(&output.stdout)
            .ok_or_else(|| AttachRefusal::new("Agent Runner attach metadata is invalid"))?;
        let value: serde_json::Value = serde_json::from_slice(stdout)
            .map_err(|_| AttachRefusal::new("Agent Runner attach metadata is invalid"))?;
        let lease_candidate = value
            .get("data")
            .and_then(|data| data.get("lease_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|lease_id| valid_id(lease_id))
            .map(str::to_string);
        let metadata = match serde_json::from_value::<AttachEnvelope>(value) {
            Ok(envelope) if envelope.schema_version == 1 && envelope.ok => envelope.data,
            _ => {
                if let Some(lease_id) = lease_candidate {
                    let _ = release_lease(&self.binary, &lease_id);
                }
                return Err(AttachRefusal::new(
                    "Agent Runner attach metadata is invalid",
                ));
            }
        };
        let invalid = !valid_id(&metadata.lease_id)
            || !valid_id(&metadata.native_thread_id)
            || metadata.endpoint.len() > MAX_ENDPOINT_BYTES
            || metadata.expires_at.is_empty()
            || metadata.expires_at.len() > MAX_TIMESTAMP_BYTES
            || crate::rfc3339_millis(&metadata.expires_at).is_none()
            || metadata.compatible_codex_version.is_empty()
            || metadata.compatible_codex_version.len() > 32
            || !valid_unix_endpoint(&metadata.endpoint);
        if invalid {
            let _ = release_lease(&self.binary, &metadata.lease_id);
            return Err(AttachRefusal::new(
                "Agent Runner attach metadata is invalid for the supported Codex transport",
            ));
        }
        if metadata.compatible_codex_version != COMPATIBLE_CODEX_VERSION {
            let _ = release_lease(&self.binary, &metadata.lease_id);
            return Err(AttachRefusal::new(format!(
                "local Codex {COMPATIBLE_CODEX_VERSION} is incompatible with Agent Runner Codex {}",
                metadata.compatible_codex_version
            )));
        }
        let local_version = local_codex_version().map_err(|reason| {
            let _ = release_lease(&self.binary, &metadata.lease_id);
            AttachRefusal::new(reason)
        })?;
        if local_version != metadata.compatible_codex_version {
            let _ = release_lease(&self.binary, &metadata.lease_id);
            return Err(AttachRefusal::new(format!(
                "local Codex version {local_version} is incompatible with Agent Runner Codex {}",
                metadata.compatible_codex_version
            )));
        }
        let command = crate::codex::cli::resume_remote_command(
            &metadata.endpoint,
            &metadata.native_thread_id,
        );
        Ok(PreparedAttach {
            command: Some(command),
            binary: self.binary.clone(),
            lease_id: metadata.lease_id,
            release_on_drop: true,
        })
    }
}

impl Default for AgentRunnerBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for AgentRunnerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::AgentRunner
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            attach: cfg!(target_os = "linux"),
            ..Capabilities::none()
        }
    }

    fn listing_scope(&self) -> Option<crate::backend::ListingCacheScope> {
        let binary = self.binary.to_string_lossy();
        crate::backend::ListingCacheScope::new(
            self.kind(),
            crate::backend::listing_scope_executable(&binary),
        )
        .ok()
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        let mut sessions = Vec::new();
        let mut seen_runs = HashSet::new();
        let mut seen_cursors = HashSet::new();
        let mut before: Option<String> = None;

        loop {
            let mut command = Command::new(&self.binary);
            command.args([
                "--json",
                "run",
                "list",
                "--status",
                "reviewable",
                "--limit",
                LIST_PAGE_LIMIT,
            ]);
            if let Some(cursor) = &before {
                command.args(["--before", cursor]);
            }
            let output = match command.output() {
                Ok(output) => output,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(error) => return Err(Error::Command(command_start_refusal(error))),
            };
            if !output.status.success() {
                return Err(Error::Command(api_error(&output)));
            }
            let stdout = bounded_output(&output.stdout).ok_or_else(invalid_list_metadata)?;
            let envelope: ListEnvelope =
                serde_json::from_slice(stdout).map_err(|_| invalid_list_metadata())?;
            if envelope.schema_version != 1 || !envelope.ok {
                return Err(invalid_list_metadata());
            }

            for run in envelope.data.runs {
                if run.runner == "kubernetes"
                    && run.provider == "codex"
                    && run.status == "reviewable"
                    && run.native_session_id.as_deref().is_some_and(valid_id)
                    && valid_id(&run.run_id)
                    && seen_runs.insert(run.run_id.clone())
                {
                    sessions.push(Session {
                        backend: BackendKind::AgentRunner,
                        id: run.run_id.clone(),
                        short_id: None,
                        origin: SessionOrigin::Background,
                        title: run.run_id,
                        cwd: PathBuf::new(),
                        git_branch: None,
                        status: crate::Status::Done,
                        created_at_ms: crate::rfc3339_millis(&run.submitted_at).unwrap_or(0),
                        updated_at_ms: crate::rfc3339_millis(&run.updated_at).unwrap_or(0),
                        hidden: false,
                        companion: false,
                        subagent: false,
                        summary: String::new(),
                        pid: None,
                        rollout_path: None,
                        pr_refs: Vec::new(),
                        daemon_hosted: false,
                    });
                }
            }

            let Some(cursor) = envelope.data.next_before else {
                break;
            };
            if cursor.is_empty()
                || cursor.len() > MAX_CURSOR_BYTES
                || cursor.contains(['\0', '\n', '\r'])
                || !seen_cursors.insert(cursor.clone())
            {
                return Err(invalid_list_metadata());
            }
            before = Some(cursor);
        }
        Ok(sessions)
    }

    fn available_models(&self) -> Vec<String> {
        Vec::new()
    }

    fn spawn(
        &self,
        _dir: &Path,
        _task: &str,
        _model: Option<&str>,
        _effort: Option<&str>,
    ) -> Result<SpawnResult> {
        Err(Error::Unsupported(self.kind().name()))
    }

    fn attach_command(&self, _session: &Session) -> std::result::Result<Command, AttachRefusal> {
        Err(AttachRefusal::new(
            "Agent Runner attach requires a private lease",
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListEnvelope {
    schema_version: u32,
    ok: bool,
    data: ListData,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListData {
    runs: Vec<RunItem>,
    next_before: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunItem {
    run_id: String,
    #[serde(rename = "request_id")]
    _request_id: serde::de::IgnoredAny,
    runner: String,
    provider: String,
    status: String,
    native_session_id: Option<String>,
    #[serde(rename = "native_turn_id")]
    _native_turn_id: serde::de::IgnoredAny,
    #[serde(rename = "snapshot_id")]
    _snapshot_id: serde::de::IgnoredAny,
    #[serde(rename = "outcome")]
    _outcome: serde::de::IgnoredAny,
    submitted_at: String,
    #[serde(rename = "started_at")]
    _started_at: serde::de::IgnoredAny,
    #[serde(rename = "finished_at")]
    _finished_at: serde::de::IgnoredAny,
    updated_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachEnvelope {
    schema_version: u32,
    ok: bool,
    data: AttachMetadata,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachMetadata {
    lease_id: String,
    endpoint: String,
    expires_at: String,
    native_thread_id: String,
    compatible_codex_version: String,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_unix_endpoint(endpoint: &str) -> bool {
    endpoint
        .strip_prefix("unix://")
        .is_some_and(|path| Path::new(path).is_absolute())
}

fn local_codex_version() -> std::result::Result<String, String> {
    let output = Command::new("codex")
        .arg("--version")
        .output()
        .map_err(|_| "local Codex 0.146.0 is unavailable".to_string())?;
    if !output.status.success() {
        return Err("local Codex 0.146.0 is unavailable".to_string());
    }
    let stdout = bounded_output(&output.stdout)
        .ok_or_else(|| "local Codex version output is invalid".to_string())?;
    let text = std::str::from_utf8(stdout)
        .map_err(|_| "local Codex version output is invalid".to_string())?
        .trim();
    let version = text
        .strip_prefix("codex-cli ")
        .ok_or_else(|| "local Codex version output is invalid".to_string())?;
    if version != COMPATIBLE_CODEX_VERSION {
        return Err(format!(
            "local Codex version {version} does not match required {COMPATIBLE_CODEX_VERSION}"
        ));
    }
    Ok(version.to_string())
}

fn release_command(binary: &Path, lease_id: &str) -> Command {
    let mut command = Command::new(binary);
    command.args(["--json", "run", "attach-release", lease_id]);
    command
}

fn release_lease(binary: &Path, lease_id: &str) -> Result<()> {
    let mut command = release_command(binary, lease_id);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Command(
            "Agent Runner attach lease release failed".to_string(),
        ))
    }
}

fn bounded_output(bytes: &[u8]) -> Option<&[u8]> {
    (bytes.len() <= MAX_RESPONSE_BYTES).then_some(bytes)
}

fn invalid_list_metadata() -> Error {
    Error::Command("agent-runner list metadata is invalid".to_string())
}

fn command_start_refusal(error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        "Agent Runner controller is unavailable".to_string()
    } else {
        "Agent Runner command could not be started".to_string()
    }
}

fn api_error(output: &std::process::Output) -> String {
    let bytes = if output.stderr.is_empty() {
        output.stdout.as_slice()
    } else {
        output.stderr.as_slice()
    };
    let Some(bytes) = bounded_output(bytes) else {
        return "Agent Runner request failed".to_string();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return "Agent Runner request failed".to_string();
    };
    let code = value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| {
            value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .unwrap_or("agent_runner_error");
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|value| value.len() <= 320)
        .unwrap_or_else(|| "request failed".to_string());
    format!("{code}: {message}")
}
