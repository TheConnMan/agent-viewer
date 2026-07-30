//! The `agent-router` shell-out behind the composer's Auto entry.
//!
//! Auto is deliberately NOT a backend: it lists nothing, owns no sessions, and never
//! implements `Backend`. It exists only in the spawn flow, where the router classifies the
//! task, weighs weekly usage headroom, picks the provider plus model/effort, and dispatches
//! the job itself. The job then appears through the winning backend's normal listing path,
//! which is why the viewer only needs the decision, not a session of its own.
//!
//! Everything here is a shell-out plus JSON parsing, exactly how the viewer already treats
//! the `codex` and `claude` CLIs: no cargo dependency in either direction.

use crate::BackendKind;
use crate::platform::{Platform, current_platform};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The router CLI, resolved on PATH like every other backend binary.
pub const ROUTER_BIN: &str = "agent-router";

/// A routed spawn pays for a classifier call AND the winning backend's own spawn (a codex
/// app-server daemon start on a bad day), so the deadline is generous. It always runs on the
/// mutation worker, never the render loop.
pub const ROUTER_TIMEOUT: Duration = Duration::from_secs(180);

/// The single model entry offered while Auto is selected: the router owns the model and
/// effort choice, so the viewer must not send one.
pub const AUTO_MODEL: &str = "auto";

/// PURE: the first executable `binary` found in a PATH-style variable value. An absolute or
/// multi-component name is taken as-is. Kept pure (the PATH value and the platform are
/// arguments, not reads) so the Auto gate is testable without mutating the process environment,
/// and on a host that is not the platform under test.
pub fn find_on_path(platform: Platform, binary: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let path = Path::new(binary);
    if path.is_absolute() || path.components().count() > 1 {
        return executable_candidate(platform, path.to_path_buf());
    }
    path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        .find_map(|directory| executable_candidate(platform, directory.join(path)))
}

/// PURE: `candidate` when it is executable, else its windows `.exe` sibling. The windows release
/// installs `agent-router.exe`, so a gate that only ever probed the bare name would leave Auto
/// permanently missing on a supported platform. Only `.exe` is probed: full PATHEXT generality
/// buys nothing for one known binary name.
fn executable_candidate(platform: Platform, candidate: PathBuf) -> Option<PathBuf> {
    if is_executable_file(&candidate) {
        return Some(candidate);
    }
    if platform != Platform::Windows {
        return None;
    }
    let mut suffixed = candidate.into_os_string();
    suffixed.push(".exe");
    let suffixed = PathBuf::from(suffixed);
    is_executable_file(&suffixed).then_some(suffixed)
}

/// PURE: whether `path` is a regular file this process could actually exec. A file merely NAMED
/// `agent-router` (a half-finished download, a stray text file) must not open the Auto entry:
/// the entry would look installed and every submission would fail on exec instead.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// IMPURE: whether the Auto spawn entry is offered at all. A missing binary means the entry
/// never appears, matching the backends-appear-when-present posture: there is no error state
/// for a router that is simply not installed.
pub fn available() -> bool {
    find_on_path(
        current_platform(),
        ROUTER_BIN,
        std::env::var_os("PATH").as_deref(),
    )
    .is_some()
}

/// One routing decision as the CLI reports it. Only the fields the viewer shows or acts on
/// are kept; the full rubric and usage snapshot live in the router's own decision log.
#[derive(Debug, Clone, PartialEq)]
pub struct RouterOutcome {
    /// The backend that won, whose listing the new job appears in.
    pub provider: BackendKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The backend's own identity (codex thread id, claude short id, opencode session id) when
    /// the router resolved one. `None` still means dispatched: a `claude --bg` job whose short
    /// id had not surfaced yet is findable by name.
    pub job_id: Option<String>,
    pub job_name: Option<String>,
    /// The gate tags that fired, in order (`claude_signals`, `headroom_tiebreak`, ...).
    pub gates: Vec<String>,
    pub rationale: String,
    pub claude_weekly_pct: f64,
    pub codex_weekly_pct: f64,
}

impl RouterOutcome {
    /// PURE: the one-line footer notice for a routed spawn. The full rationale is a paragraph,
    /// so the footer carries the decision and the headroom that shaped it, not the prose.
    pub fn notice(&self) -> String {
        let mut line = format!("auto: {}", self.provider.name());
        if let Some(model) = &self.model {
            line.push(' ');
            line.push_str(model);
        }
        if let Some(effort) = &self.effort {
            line.push_str(&format!(" effort {effort}"));
        }
        if let Some(job) = self.job_id.as_deref().or(self.job_name.as_deref()) {
            line.push_str(&format!(" job {}", one_line(job)));
        }
        if !self.gates.is_empty() {
            line.push_str(&format!(" gates[{}]", self.gates.join(",")));
        }
        format!(
            "{line} (codex weekly {:.0}%, claude {:.0}%)",
            self.codex_weekly_pct, self.claude_weekly_pct
        )
    }
}

/// PURE: parse `agent-router run --json` stdout into a decision.
///
/// Every failure is reported as a message the user can act on rather than a default: a
/// decision the viewer cannot read must never become a silent spawn on a guessed provider.
pub fn parse_outcome(stdout: &str) -> std::result::Result<RouterOutcome, String> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| format!("`{ROUTER_BIN}` printed unreadable json: {e}"))?;
    let provider = required_str(&value, "provider")?;
    let provider = provider_kind(provider)?;
    let dry_run = required_bool(&value, "dry_run")?;
    if dry_run {
        return Err(format!("`{ROUTER_BIN}` json reports dry_run true"));
    }
    let dispatch = required_object(&value, "dispatch")?;
    let gates = required_string_array(&value, "gates")?;
    let rationale = required_str(&value, "rationale")?.to_string();
    let usage = required_object(&value, "usage")?;
    let job_id = nullable_string_from_object(dispatch, "job_id")
        .map_err(|error| format!("dispatch: {error}"))?;
    if job_id.as_deref() == Some("") {
        return Err(format!(
            "`{ROUTER_BIN}` json field dispatch.job_id is empty"
        ));
    }
    let job_name = nullable_string_from_object(dispatch, "job_name")
        .map_err(|error| format!("dispatch: {error}"))?;
    match job_name.as_deref() {
        None => {
            return Err(format!(
                "`{ROUTER_BIN}` json field dispatch.job_name is null"
            ));
        }
        Some("") => {
            return Err(format!(
                "`{ROUTER_BIN}` json field dispatch.job_name is empty"
            ));
        }
        Some(_) => {}
    }
    Ok(RouterOutcome {
        provider,
        model: nullable_string(&value, "model")?,
        effort: nullable_string(&value, "effort")?,
        job_id,
        job_name,
        gates,
        rationale,
        claude_weekly_pct: weekly_pct(usage, "claude")?,
        codex_weekly_pct: weekly_pct(usage, "codex")?,
    })
}

/// PURE: the argv for one routing run. `--json` only; no `--model`, since the router owns model
/// and effort selection.
///
/// The task is separated from the options by a literal `--`: it is user text and can begin with a
/// hyphen (`--help ...`, `-v ...`), which the router's own clap would otherwise parse as an option
/// and fail on instead of routing.
pub fn route_command(dir: &Path, task: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(ROUTER_BIN);
    cmd.arg("run")
        .arg("--json")
        .arg("--dir")
        .arg(dir)
        .arg("--provider")
        .arg("auto")
        .arg("--")
        .arg(task);
    cmd
}

/// IMPURE: route one task and let the router dispatch it. Every error is collapsed to one
/// line before it reaches the caller: it lands in the single-line footer, and both the
/// described argv (which embeds the raw task) and the router's stderr can carry newlines.
pub fn route(dir: &Path, task: &str) -> std::result::Result<RouterOutcome, String> {
    let stdout = crate::spawn::run_reporting_failure(route_command(dir, task), ROUTER_TIMEOUT)
        .map_err(|error| one_line(&error))?;
    parse_outcome(&stdout).map_err(|error| one_line(&error))
}

/// PURE: whitespace runs (newlines included) collapsed to single spaces. The router's job name
/// falls back to a prefix of the task, and a multi-line task would otherwise split or clip the
/// one line the footer notice gets.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// PURE: the backend a router provider name denotes. An unknown name is an error, not a
/// fallback, so a router that grows a fourth provider cannot land a job the viewer mislabels.
fn provider_kind(name: &str) -> std::result::Result<BackendKind, String> {
    match name {
        "codex" => Ok(BackendKind::Codex),
        "claude" => Ok(BackendKind::Claude),
        "opencode" => Ok(BackendKind::Opencode),
        other => Err(format!(
            "`{ROUTER_BIN}` routed to unknown provider {other:?}"
        )),
    }
}

fn required_str<'a>(
    value: &'a serde_json::Value,
    key: &str,
) -> std::result::Result<&'a str, String> {
    match value.get(key) {
        Some(serde_json::Value::String(text)) => Ok(text),
        Some(_) => Err(format!("`{ROUTER_BIN}` json field {key} is not a string")),
        None => Err(format!("`{ROUTER_BIN}` json has no {key}")),
    }
}

fn required_bool(value: &serde_json::Value, key: &str) -> std::result::Result<bool, String> {
    match value.get(key) {
        Some(serde_json::Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(format!("`{ROUTER_BIN}` json field {key} is not a boolean")),
        None => Err(format!("`{ROUTER_BIN}` json has no {key}")),
    }
}

fn required_object<'a>(
    value: &'a serde_json::Value,
    key: &str,
) -> std::result::Result<&'a serde_json::Map<String, serde_json::Value>, String> {
    match value.get(key) {
        Some(serde_json::Value::Object(object)) => Ok(object),
        Some(_) => Err(format!("`{ROUTER_BIN}` json field {key} is not an object")),
        None => Err(format!("`{ROUTER_BIN}` json has no {key}")),
    }
}

fn nullable_string(
    value: &serde_json::Value,
    key: &str,
) -> std::result::Result<Option<String>, String> {
    nullable_string_value(value.get(key), key)
}

fn nullable_string_from_object(
    value: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> std::result::Result<Option<String>, String> {
    nullable_string_value(value.get(key), key)
}

fn nullable_string_value(
    value: Option<&serde_json::Value>,
    key: &str,
) -> std::result::Result<Option<String>, String> {
    match value {
        Some(serde_json::Value::String(text)) => Ok(Some(text.clone())),
        Some(serde_json::Value::Null) => Ok(None),
        Some(_) => Err(format!(
            "`{ROUTER_BIN}` json field {key} is not a string or null"
        )),
        None => Err(format!("`{ROUTER_BIN}` json has no {key}")),
    }
}

fn required_string_array(
    value: &serde_json::Value,
    key: &str,
) -> std::result::Result<Vec<String>, String> {
    let Some(gates) = value.get(key) else {
        return Err(format!("`{ROUTER_BIN}` json has no {key}"));
    };
    let serde_json::Value::Array(gates) = gates else {
        return Err(format!("`{ROUTER_BIN}` json field {key} is not an array"));
    };
    gates
        .iter()
        .map(|gate| {
            gate.as_str().map(str::to_string).ok_or_else(|| {
                format!("`{ROUTER_BIN}` json field {key} contains a non-string value")
            })
        })
        .collect()
}

fn weekly_pct(
    usage: &serde_json::Map<String, serde_json::Value>,
    provider: &str,
) -> std::result::Result<f64, String> {
    usage
        .get(provider)
        .and_then(serde_json::Value::as_object)
        .and_then(|headroom| headroom.get("weekly_pct"))
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| {
            format!(
                "`{ROUTER_BIN}` json field usage.{provider}.weekly_pct is missing or not numeric"
            )
        })
}
