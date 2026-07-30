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
    let provider = crate::json_str(&value, "provider")
        .ok_or_else(|| format!("`{ROUTER_BIN}` json has no provider"))?;
    let provider = provider_kind(provider)?;
    let dispatch = value.get("dispatch");
    Ok(RouterOutcome {
        provider,
        model: owned_str(&value, "model"),
        effort: owned_str(&value, "effort"),
        job_id: dispatch.and_then(|d| owned_str(d, "job_id")),
        job_name: dispatch.and_then(|d| owned_str(d, "job_name")),
        gates: value
            .get("gates")
            .and_then(|gates| gates.as_array())
            .map(|gates| {
                gates
                    .iter()
                    .filter_map(|gate| gate.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        rationale: crate::json_str(&value, "rationale")
            .unwrap_or_default()
            .to_string(),
        claude_weekly_pct: weekly_pct(&value, "claude"),
        codex_weekly_pct: weekly_pct(&value, "codex"),
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
        .arg("--")
        .arg(task);
    cmd
}

/// IMPURE: route one task and let the router dispatch it.
pub fn route(dir: &Path, task: &str) -> std::result::Result<RouterOutcome, String> {
    let stdout = crate::spawn::run_reporting_failure(route_command(dir, task), ROUTER_TIMEOUT)?;
    parse_outcome(&stdout)
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

/// PURE: an owned string field, treating JSON null (the router's "not applicable") as absent.
fn owned_str(value: &serde_json::Value, key: &str) -> Option<String> {
    crate::json_str(value, key).map(str::to_string)
}

/// PURE: `usage.<provider>.weekly_pct`, 0 when the router could not read it (its usage
/// readers fail open, so a missing number means "no known consumption", not an error).
fn weekly_pct(value: &serde_json::Value, provider: &str) -> f64 {
    value
        .get("usage")
        .and_then(|usage| usage.get(provider))
        .and_then(|headroom| headroom.get("weekly_pct"))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0)
}
