//! The `agent-router` shell-out every spawn goes through when the router is installed.
//!
//! Two callers, one path. Auto sends no provider and the router classifies the task, weighs
//! weekly usage headroom, and picks the provider plus model/effort. A composer sitting on a
//! concrete backend sends that backend as `--provider`, which the router honours as an
//! override: no classification runs, but the job still gets the router's derived name and its
//! decision-log row, which is the whole reason an explicitly chosen provider routes at all
//! rather than calling the backend directly.
//!
//! Auto is deliberately NOT a backend: it lists nothing, owns no sessions, and never
//! implements `Backend`. Either way the job appears through the dispatching backend's normal
//! listing path, which is why the viewer only needs the decision, not a session of its own.
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
    /// What the viewer asked for: `None` on the Auto path, `Some` when the composer named a
    /// provider and the router ran with that override. Not parsed from the JSON — the router
    /// reports the decision, and only the caller knows whether it was free to make one.
    pub requested: Option<BackendKind>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The backend's own identity (codex thread id, claude short id) when
    /// the router resolved one. `None` still means dispatched: a `claude --bg` job whose short
    /// id had not surfaced yet is findable by name.
    pub job_id: Option<String>,
    pub job_name: Option<String>,
    /// The router made a valid decision but proved that none of its configured providers has a
    /// capability the task needs. No backend was started, so there is no dispatch identity.
    pub capability_blocked: Option<String>,
    /// The gate tags that fired, in order (`claude_signals`, `headroom_tiebreak`, ...).
    pub gates: Vec<String>,
    pub rationale: String,
    pub claude_weekly_pct: f64,
    pub codex_weekly_pct: f64,
}

impl RouterOutcome {
    /// PURE: the one-line footer notice for a routed spawn. The full rationale is a paragraph,
    /// so the footer carries the decision and the headroom that shaped it, not the prose.
    ///
    /// An overridden route reads as an ordinary spawn (`spawned on codex …`) rather than
    /// `auto:`, because nothing was decided for the user, and it drops the gate list: the only
    /// gate an override ever fires is `explicit_provider`, which repeats what the line says.
    /// The headroom stays on both, since it is the same free reading either way.
    pub fn notice(&self) -> String {
        let routed_by_provider = self.requested.is_none();
        if let Some(reason) = &self.capability_blocked {
            return format!(
                "{}: blocked: {} (codex weekly {:.0}%, claude {:.0}%)",
                if routed_by_provider { "auto" } else { "spawn" },
                one_line(reason),
                self.codex_weekly_pct,
                self.claude_weekly_pct
            );
        }
        let mut line = if routed_by_provider {
            format!("auto: {}", self.provider.name())
        } else {
            format!("spawned on {}", self.provider.name())
        };
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
        if routed_by_provider && !self.gates.is_empty() {
            line.push_str(&format!(" gates[{}]", self.gates.join(",")));
        }
        format!(
            "{line} (codex weekly {:.0}%, claude {:.0}%)",
            self.codex_weekly_pct, self.claude_weekly_pct
        )
    }
}

/// PURE: parse `agent-router run --json` stdout into a decision. `requested` is the provider
/// the caller pinned, or `None` for Auto.
///
/// Every failure is reported as a message the user can act on rather than a default: a
/// decision the viewer cannot read must never become a silent spawn on a guessed provider.
pub fn parse_outcome(
    stdout: &str,
    requested: Option<BackendKind>,
) -> std::result::Result<RouterOutcome, String> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| format!("`{ROUTER_BIN}` printed unreadable json: {e}"))?;
    let provider = required_str(&value, "provider")?;
    let provider = provider_kind(provider)?;
    // An override that did not take is not a spawn to relabel: the job is running on a backend
    // the user did not pick, and every downstream identity (which listing to fence, which
    // preexisting id set to exclude, which row to select) would be read off the wrong one.
    if let Some(requested) = requested
        && requested != provider
    {
        return Err(format!(
            "`{ROUTER_BIN}` was pinned to {} but dispatched to {}",
            requested.name(),
            provider.name()
        ));
    }
    let dry_run = required_bool(&value, "dry_run")?;
    if dry_run {
        return Err(format!("`{ROUTER_BIN}` json reports dry_run true"));
    }
    let gates = required_string_array(&value, "gates")?;
    let rationale = required_str(&value, "rationale")?.to_string();
    let usage = required_object(&value, "usage")?;
    // `capability_blocked` was added after the viewer's first router integration. Its absence is
    // therefore the old dispatched contract, while a present null keeps the current contract
    // explicit. A present malformed value remains an error rather than a guessed outcome.
    let capability_blocked = optional_nullable_string(&value, "capability_blocked")?;
    if capability_blocked.is_some() {
        return Ok(RouterOutcome {
            provider,
            requested,
            model: nullable_string(&value, "model")?,
            effort: nullable_string(&value, "effort")?,
            job_id: None,
            job_name: None,
            capability_blocked,
            gates,
            rationale,
            claude_weekly_pct: weekly_pct(usage, "claude")?,
            codex_weekly_pct: weekly_pct(usage, "codex")?,
        });
    }
    let dispatch = required_object(&value, "dispatch")?;
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
        requested,
        model: nullable_string(&value, "model")?,
        effort: nullable_string(&value, "effort")?,
        job_id,
        job_name,
        capability_blocked: None,
        gates,
        rationale,
        claude_weekly_pct: weekly_pct(usage, "claude")?,
        codex_weekly_pct: weekly_pct(usage, "codex")?,
    })
}

/// PURE: the argv for one routing run.
///
/// `provider` is `None` for Auto, which sends the router's own `auto` literal and no `--model`,
/// since the router then owns model and effort selection. A pinned provider sends its name plus
/// the composer's model when one was chosen; `--model` is refused by the router without an
/// explicit `--provider`, which is exactly the pairing here.
///
/// Effort is never sent on either path: the composer has no effort control, so a pinned route
/// leaves it to the backend's own default exactly as a direct spawn did.
///
/// The task is separated from the options by a literal `--`: it is user text and can begin with a
/// hyphen (`--help ...`, `-v ...`), which the router's own clap would otherwise parse as an option
/// and fail on instead of routing.
pub fn route_command(
    dir: &Path,
    task: &str,
    provider: Option<BackendKind>,
    model: Option<&str>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(ROUTER_BIN);
    cmd.arg("run")
        .arg("--json")
        .arg("--dir")
        .arg(dir)
        .arg("--provider")
        .arg(provider.map_or("auto", BackendKind::name));
    if let Some(model) = model.filter(|_| provider.is_some()) {
        cmd.arg("--model").arg(model);
    }
    cmd.arg("--").arg(task);
    cmd
}

/// IMPURE: route one task and let the router dispatch it. Every error is collapsed to one
/// line before it reaches the caller: it lands in the single-line footer, and both the
/// described argv (which embeds the raw task) and the router's stderr can carry newlines.
pub fn route(
    dir: &Path,
    task: &str,
    provider: Option<BackendKind>,
    model: Option<&str>,
) -> std::result::Result<RouterOutcome, String> {
    let stdout = crate::spawn::run_reporting_failure(
        route_command(dir, task, provider, model),
        ROUTER_TIMEOUT,
    )
    .map_err(|error| one_line(&error))?;
    parse_outcome(&stdout, provider).map_err(|error| one_line(&error))
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
        "grok" => Ok(BackendKind::Grok),
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

fn optional_nullable_string(
    value: &serde_json::Value,
    key: &str,
) -> std::result::Result<Option<String>, String> {
    match value.get(key) {
        Some(value) => nullable_string_value(Some(value), key),
        None => Ok(None),
    }
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
