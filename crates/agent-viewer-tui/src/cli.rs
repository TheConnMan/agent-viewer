use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

use agent_viewer_core::backend::{ArchiveMode, all_backends};
use agent_viewer_core::state::apply_viewer_state;
use agent_viewer_core::{
    Backend, BackendKind, Capabilities, Error as CoreError, PrRef, Session, Status, TailEvent,
    ViewerDb, mark_dead_dirs,
};
use clap::{Arg, ArgAction, ColorChoice, Command, error::ErrorKind};
use serde::Serialize;
use serde_json::json;

use crate::shared_listing::invalidate_backend_scope;

const WAIT_WORKER_ENV: &str = "AGENT_VIEWER_INTERNAL_WAIT_WORKER";
const WAIT_POLL: Duration = Duration::from_millis(100);
const WAIT_STDOUT_CAP: usize = 1024 * 1024;
const WAIT_STDERR_CAP: usize = 64 * 1024;

#[derive(Debug)]
pub enum StartupAction {
    RunTui,
    RunCli(CliCommand),
    Exit(CliOutput),
}

#[derive(Debug)]
pub struct CliCommand {
    json: bool,
    command: CommandKind,
}

#[derive(Debug)]
enum CommandKind {
    List(ListArgs),
    Show(TargetArgs),
    Tail {
        target: TargetArgs,
        limit: usize,
    },
    Mutate {
        action: Mutation,
        target: TargetArgs,
        name: Option<String>,
        allow_delete: bool,
    },
    Wait {
        target: TargetArgs,
        timeout: Duration,
    },
}

#[derive(Debug)]
struct ListArgs {
    all: bool,
    provider: Option<BackendKind>,
    status: Option<StatusFilter>,
    dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct TargetArgs {
    value: String,
    provider: Option<BackendKind>,
}

#[derive(Debug, Clone, Copy)]
enum Mutation {
    Archive,
    Stop,
    Rename,
    Unarchive,
}

impl Mutation {
    fn name(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Stop => "stop",
            Self::Rename => "rename",
            Self::Unarchive => "unarchive",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StatusFilter {
    Working,
    NeedsInput,
    Idle,
    Done,
    Error,
    Unknown,
}

impl StatusFilter {
    fn matches(self, status: &Status) -> bool {
        matches!(
            (self, status),
            (Self::Working, Status::Working)
                | (Self::NeedsInput, Status::NeedsInput { .. })
                | (Self::Idle, Status::Idle)
                | (Self::Done, Status::Done)
                | (Self::Error, Status::Error)
                | (Self::Unknown, Status::Unknown)
        )
    }
}

#[derive(Debug)]
pub struct CliOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: u8,
}

impl CliOutput {
    fn success(value: serde_json::Value, human: String, json: bool) -> Self {
        let stdout = if json {
            format!("{value}\n")
        } else if human.is_empty() {
            String::new()
        } else {
            format!("{human}\n")
        };
        Self {
            stdout,
            stderr: String::new(),
            code: 0,
        }
    }

    fn failure(error: CliError, json: bool) -> Self {
        if json {
            let value = json!({
                "schema_version": 1,
                "error": {"code": error.kind, "message": error.message},
            });
            Self {
                stdout: format!("{value}\n"),
                stderr: String::new(),
                code: error.exit,
            }
        } else {
            Self {
                stdout: String::new(),
                stderr: format!("agent-viewer: {}\n", error.message),
                code: error.exit,
            }
        }
    }
}

#[derive(Debug)]
struct CliError {
    exit: u8,
    kind: &'static str,
    message: String,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self::new(2, "usage", message)
    }

    fn missing(message: impl Into<String>) -> Self {
        Self::new(3, "not_found", message)
    }

    fn ambiguous(message: impl Into<String>) -> Self {
        Self::new(4, "ambiguous", message)
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self::new(5, "unsupported", message)
    }

    fn delete_flag(message: impl Into<String>) -> Self {
        Self::new(6, "delete_flag_required", message)
    }

    fn backend(message: impl Into<String>) -> Self {
        Self::new(1, "backend", message)
    }

    fn timeout() -> Self {
        Self::new(124, "timeout", "wait deadline expired")
    }

    fn new(exit: u8, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit,
            kind,
            message: message.into(),
        }
    }
}

pub fn startup_action<I, S>(args: I) -> StartupAction
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<_>>();
    if args.is_empty() {
        return StartupAction::RunTui;
    }
    let error_json = json_requested(&args);
    let argv = std::iter::once(OsString::from("agent-viewer"))
        .chain(args)
        .collect::<Vec<_>>();
    let matches = match cli_app().try_get_matches_from(argv) {
        Ok(matches) => matches,
        Err(error) if error.kind() == ErrorKind::DisplayVersion => {
            return StartupAction::Exit(CliOutput {
                stdout: format!("agent-viewer {}\n", env!("CARGO_PKG_VERSION")),
                stderr: String::new(),
                code: 0,
            });
        }
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            return StartupAction::Exit(CliOutput {
                stdout: error.to_string(),
                stderr: String::new(),
                code: 0,
            });
        }
        Err(error) => {
            return StartupAction::Exit(CliOutput::failure(
                CliError::usage(error.to_string().trim().to_string()),
                error_json,
            ));
        }
    };
    let json = matches.get_flag("json");
    match command_from_matches(&matches) {
        Ok(command) => StartupAction::RunCli(CliCommand { json, command }),
        Err(error) => StartupAction::Exit(CliOutput::failure(error, json)),
    }
}

fn cli_app() -> Command {
    Command::new("agent-viewer")
        .version(env!("CARGO_PKG_VERSION"))
        .propagate_version(true)
        .about("Inspect and control coding agent sessions")
        .color(ColorChoice::Never)
        .arg(
            Arg::new("json")
                .long("json")
                .global(true)
                .help("Write one versioned JSON result to stdout")
                .action(ArgAction::SetTrue),
        )
        .subcommand(list_command())
        .subcommand(target_command("show"))
        .subcommand(
            target_command("tail").arg(
                Arg::new("limit")
                    .long("limit")
                    .default_value("20")
                    .help("Return the last N events, from 1 through 10000")
                    .value_parser(clap::value_parser!(usize)),
            ),
        )
        .subcommand(
            target_command("archive").arg(
                Arg::new("allow-delete")
                    .long("allow-delete")
                    .help("Allow archive to delete a Claude or Grok session and its owned data")
                    .action(ArgAction::SetTrue),
            ),
        )
        .subcommand(target_command("stop"))
        .subcommand(target_command("rename").arg(Arg::new("name").required(true)))
        .subcommand(target_command("unarchive"))
        .subcommand(
            target_command("wait").arg(
                Arg::new("timeout")
                    .long("timeout")
                    .default_value("300")
                    .help("Seconds to wait for done or needs input; error exits 1 and timeout 124")
                    .value_parser(clap::value_parser!(f64)),
            ),
        )
}

fn list_command() -> Command {
    Command::new("list")
        .about("List current primary sessions")
        .arg(
            Arg::new("all")
                .long("all")
                .help("Include archived, companion, subagent, hold, and missing directory rows")
                .action(ArgAction::SetTrue),
        )
        .arg(provider_arg())
        .arg(
            Arg::new("status")
                .long("status")
                .help("Keep sessions in one status")
                .value_parser(["working", "needs_input", "idle", "done", "error", "unknown"]),
        )
        .arg(
            Arg::new("dir")
                .long("dir")
                .help("Keep sessions whose normalized working directory equals PATH"),
        )
}

fn target_command(name: &'static str) -> Command {
    Command::new(name)
        .about(match name {
            "show" => "Show one session",
            "tail" => "Read recent transcript events",
            "archive" => "Archive one session",
            "stop" => "Stop one active session without removing it",
            "rename" => "Rename one session",
            "unarchive" => "Restore one reversibly archived session",
            "wait" => "Wait for done, needs input, error, or the deadline",
            _ => "Operate on one session",
        })
        .arg(
            Arg::new("target")
                .required(true)
                .help("Full ID, short ID, unique prefix, or provider:id"),
        )
        .arg(provider_arg())
}

fn provider_arg() -> Arg {
    Arg::new("provider")
        .long("provider")
        .help("Limit lookup to codex, claude, or grok")
        .value_parser(["codex", "claude", "grok"])
}

fn json_requested(args: &[OsString]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
        .any(|arg| arg.as_os_str() == OsStr::new("--json"))
}

fn command_from_matches(matches: &clap::ArgMatches) -> Result<CommandKind, CliError> {
    let command = match matches.subcommand() {
        None => CommandKind::List(ListArgs {
            all: false,
            provider: None,
            status: None,
            dir: None,
        }),
        Some(("list", args)) => CommandKind::List(ListArgs {
            all: args.get_flag("all"),
            provider: provider_from(args),
            status: args
                .get_one::<String>("status")
                .map(|value| match value.as_str() {
                    "working" => StatusFilter::Working,
                    "needs_input" => StatusFilter::NeedsInput,
                    "idle" => StatusFilter::Idle,
                    "done" => StatusFilter::Done,
                    "error" => StatusFilter::Error,
                    _ => StatusFilter::Unknown,
                }),
            dir: args.get_one::<String>("dir").map(PathBuf::from),
        }),
        Some(("show", args)) => CommandKind::Show(target_args(args)),
        Some(("tail", args)) => {
            let limit = *args.get_one::<usize>("limit").expect("defaulted by clap");
            if !(1..=10_000).contains(&limit) {
                return Err(CliError::usage("tail limit must be between 1 and 10000"));
            }
            CommandKind::Tail {
                target: target_args(args),
                limit,
            }
        }
        Some(("archive", args)) => {
            mutation_command(Mutation::Archive, args, None, args.get_flag("allow-delete"))?
        }
        Some(("stop", args)) => mutation_command(Mutation::Stop, args, None, false)?,
        Some(("rename", args)) => {
            let name = args.get_one::<String>("name").cloned();
            if name.as_deref().is_none_or(|name| name.trim().is_empty()) {
                return Err(CliError::usage("rename requires a nonempty name"));
            }
            mutation_command(Mutation::Rename, args, name, false)?
        }
        Some(("unarchive", args)) => mutation_command(Mutation::Unarchive, args, None, false)?,
        Some(("wait", args)) => {
            let seconds = *args.get_one::<f64>("timeout").expect("defaulted by clap");
            let timeout = Duration::try_from_secs_f64(seconds)
                .map_err(|_| CliError::usage("wait timeout must be finite and nonnegative"))?;
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| CliError::usage("wait timeout is too large"))?;
            CommandKind::Wait {
                target: target_args(args),
                timeout,
            }
        }
        Some((name, _)) => return Err(CliError::usage(format!("unknown command {name}"))),
    };
    Ok(command)
}

fn mutation_command(
    action: Mutation,
    args: &clap::ArgMatches,
    name: Option<String>,
    allow_delete: bool,
) -> Result<CommandKind, CliError> {
    let target = target_args(args);
    let _ = TargetSpec::parse(&target, true)?;
    Ok(CommandKind::Mutate {
        action,
        target,
        name,
        allow_delete,
    })
}

fn target_args(args: &clap::ArgMatches) -> TargetArgs {
    TargetArgs {
        value: args
            .get_one::<String>("target")
            .expect("required by clap")
            .clone(),
        provider: provider_from(args),
    }
}

fn provider_from(args: &clap::ArgMatches) -> Option<BackendKind> {
    args.get_one::<String>("provider")
        .and_then(|value| parse_provider(value))
}

fn parse_provider(value: &str) -> Option<BackendKind> {
    match value {
        "codex" => Some(BackendKind::Codex),
        "claude" => Some(BackendKind::Claude),
        "grok" => Some(BackendKind::Grok),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct TargetSpec {
    id: String,
    provider: Option<BackendKind>,
}

impl TargetSpec {
    fn parse(args: &TargetArgs, provider_required: bool) -> Result<Self, CliError> {
        let qualified = args
            .value
            .split_once(':')
            .and_then(|(name, id)| parse_provider(name).map(|provider| (provider, id)));
        let (provider, id) = match qualified {
            Some((qualified, _)) if args.provider.is_some_and(|flag| flag != qualified) => {
                return Err(CliError::usage(
                    "qualified target conflicts with the provider flag",
                ));
            }
            Some((qualified, id)) => (Some(qualified), id),
            None => (args.provider, args.value.as_str()),
        };
        if id.is_empty() {
            return Err(CliError::usage("target identity cannot be empty"));
        }
        if provider_required && provider.is_none() {
            return Err(CliError::usage(
                "mutations require a provider flag or a qualified provider:id target",
            ));
        }
        Ok(Self {
            id: id.to_string(),
            provider,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Observation {
    Listed,
    EmptyOrUnavailable,
    Unavailable,
}

struct Source {
    backend: Box<dyn Backend>,
    sessions: Vec<Session>,
    observation: Observation,
}

fn load_sources(provider: Option<BackendKind>) -> Result<Vec<Source>, CliError> {
    all_backends()
        .into_iter()
        .filter(|backend| provider.is_none_or(|selected| backend.kind() == selected))
        .map(|mut backend| {
            let kind = backend.kind();
            let (sessions, observation) = match backend.list() {
                Ok(sessions) => {
                    let observation = if kind == BackendKind::Claude && sessions.is_empty() {
                        Observation::EmptyOrUnavailable
                    } else {
                        Observation::Listed
                    };
                    (sessions, observation)
                }
                Err(CoreError::NoStateDb(_)) => (Vec::new(), Observation::Unavailable),
                Err(error) => {
                    return Err(CliError::backend(format!("{}: {error}", kind.name())));
                }
            };
            Ok(Source {
                backend,
                sessions,
                observation,
            })
        })
        .collect()
}

#[derive(Serialize)]
struct ProviderDto {
    provider: &'static str,
    observation: Observation,
}

#[derive(Serialize)]
struct CapabilitiesDto {
    archive: bool,
    stop: bool,
    rename: bool,
    unarchive: bool,
}

#[derive(Serialize)]
struct SessionDto {
    provider: &'static str,
    id: String,
    short_id: Option<String>,
    target: String,
    title: String,
    status: &'static str,
    cwd: String,
    git_branch: Option<String>,
    summary: String,
    created_at_ms: i64,
    updated_at_ms: i64,
    hidden: bool,
    companion: bool,
    subagent: bool,
    pr_refs: Vec<PrRef>,
    archive_mode: Option<&'static str>,
    capabilities: CapabilitiesDto,
}

impl SessionDto {
    fn new(session: Session, capabilities: Capabilities) -> Self {
        let provider = session.backend.name();
        Self {
            provider,
            target: format!("{provider}:{}", session.id),
            id: session.id,
            short_id: session.short_id,
            title: session.title,
            status: status_name(&session.status),
            cwd: session.cwd.to_string_lossy().into_owned(),
            git_branch: session.git_branch,
            summary: session.summary,
            created_at_ms: session.created_at_ms,
            updated_at_ms: session.updated_at_ms,
            hidden: session.hidden,
            companion: session.companion,
            subagent: session.subagent,
            pr_refs: session.pr_refs,
            archive_mode: match capabilities.archive {
                Some(ArchiveMode::Reversible) => Some("reversible"),
                Some(ArchiveMode::Deletes) => Some("deletes"),
                None => None,
            },
            capabilities: CapabilitiesDto {
                archive: capabilities.can_archive(),
                stop: capabilities.stop,
                rename: capabilities.rename,
                unarchive: capabilities.can_unarchive(),
            },
        }
    }
}

fn status_name(status: &Status) -> &'static str {
    match status {
        Status::Working => "working",
        Status::NeedsInput { .. } => "needs_input",
        Status::Idle => "idle",
        Status::Done => "done",
        Status::Error => "error",
        Status::Unknown => "unknown",
    }
}

pub fn run_cli(command: CliCommand) -> CliOutput {
    let json = command.json;
    let result = match command.command {
        CommandKind::List(args) => run_list(args),
        CommandKind::Show(args) => run_show(args),
        CommandKind::Tail { target, limit } => run_tail(target, limit),
        CommandKind::Mutate {
            action,
            target,
            name,
            allow_delete,
        } => run_mutation(action, target, name.as_deref(), allow_delete),
        CommandKind::Wait { target, timeout } => return run_wait(target, timeout, json),
    };
    match result {
        Ok(success) => CliOutput::success(success.value, success.human, json),
        Err(error) => CliOutput::failure(error, json),
    }
}

struct Success {
    value: serde_json::Value,
    human: String,
}

fn run_list(args: ListArgs) -> Result<Success, CliError> {
    let dir_filter = args.dir.as_deref().map(normalize_path);
    let sources = load_sources(args.provider)?;
    let providers = sources
        .iter()
        .map(|source| ProviderDto {
            provider: source.backend.kind().name(),
            observation: source.observation,
        })
        .collect::<Vec<_>>();
    let mut capabilities = HashMap::new();
    let mut sessions = Vec::new();
    for source in &sources {
        for session in &source.sessions {
            capabilities.insert(
                (session.backend, session.id.clone()),
                source.backend.capabilities_for(session),
            );
        }
        sessions.extend(source.sessions.iter().cloned());
    }
    if let Ok(db) = ViewerDb::open_default()
        && let Ok(state) = db.viewer_state()
    {
        apply_viewer_state(&mut sessions, &state);
    }
    mark_dead_dirs(&mut sessions);
    sessions.retain(|session| {
        (args.all
            || (!session.hidden
                && !session.companion
                && !session.subagent
                && !session.title.eq_ignore_ascii_case("hold")))
            && args
                .status
                .is_none_or(|status| status.matches(&session.status))
            && dir_filter
                .as_ref()
                .is_none_or(|dir| normalize_path(&session.cwd).as_path() == dir.as_path())
    });
    sessions.sort_by(|left, right| {
        left.backend
            .name()
            .cmp(right.backend.name())
            .then(left.created_at_ms.cmp(&right.created_at_ms))
            .then(left.id.cmp(&right.id))
    });
    let rows = sessions
        .into_iter()
        .map(|session| {
            let caps = capabilities
                .get(&(session.backend, session.id.clone()))
                .copied()
                .unwrap_or_else(Capabilities::none);
            SessionDto::new(session, caps)
        })
        .collect::<Vec<_>>();
    let human = rows
        .iter()
        .map(|row| format!("{}\t{}\t{}\t{}", row.target, row.status, row.title, row.cwd))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Success {
        value: json!({
            "schema_version": 1,
            "command": "list",
            "providers": providers,
            "sessions": rows,
        }),
        human,
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn run_show(args: TargetArgs) -> Result<Success, CliError> {
    let spec = TargetSpec::parse(&args, false)?;
    let mut sources = load_sources(spec.provider)?;
    let (index, listed) = resolve_target(&sources, &spec.id)?;
    let mut session = authoritative(&mut sources[index], &listed.id)?;
    if session.backend == BackendKind::Codex {
        session.pr_refs = listed.pr_refs;
    }
    let capabilities = sources[index].backend.capabilities_for(&session);
    let row = SessionDto::new(session, capabilities);
    let human = format!("{}\t{}\t{}\t{}", row.target, row.status, row.title, row.cwd);
    Ok(Success {
        value: json!({"schema_version": 1, "command": "show", "session": row}),
        human,
    })
}

fn run_tail(args: TargetArgs, limit: usize) -> Result<Success, CliError> {
    let spec = TargetSpec::parse(&args, false)?;
    let mut sources = load_sources(spec.provider)?;
    let (index, listed) = resolve_target(&sources, &spec.id)?;
    let session = authoritative(&mut sources[index], &listed.id)?;
    let events = sources[index]
        .backend
        .tail(&session, limit)
        .map_err(|error| CliError::backend(format!("tail failed: {error}")))?;
    let event_rows = events.iter().map(TailEventDto::from).collect::<Vec<_>>();
    let human = events
        .iter()
        .map(|event| match event {
            TailEvent::User(text) => format!("user\t{text}"),
            TailEvent::Agent(text) => format!("agent\t{text}"),
            TailEvent::Tool { name, detail } => format!("tool\t{name}\t{detail}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Success {
        value: json!({
            "schema_version": 1,
            "command": "tail",
            "provider": session.backend.name(),
            "id": session.id,
            "events": event_rows,
        }),
        human,
    })
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TailEventDto {
    User { text: String },
    Agent { text: String },
    Tool { name: String, detail: String },
}

impl From<&TailEvent> for TailEventDto {
    fn from(event: &TailEvent) -> Self {
        match event {
            TailEvent::User(text) => Self::User { text: text.clone() },
            TailEvent::Agent(text) => Self::Agent { text: text.clone() },
            TailEvent::Tool { name, detail } => Self::Tool {
                name: name.clone(),
                detail: detail.clone(),
            },
        }
    }
}

fn run_mutation(
    action: Mutation,
    args: TargetArgs,
    name: Option<&str>,
    allow_delete: bool,
) -> Result<Success, CliError> {
    let spec = TargetSpec::parse(&args, true)?;
    let mut sources = load_sources(spec.provider)?;
    let (index, listed) = resolve_target(&sources, &spec.id)?;
    let source = &mut sources[index];
    let session = authoritative(source, &listed.id)?;
    let capabilities = source.backend.capabilities_for(&session);
    match action {
        Mutation::Archive if !capabilities.can_archive() => {
            return Err(CliError::unsupported(format!(
                "{} does not support archive for this session",
                session.backend.name()
            )));
        }
        Mutation::Archive
            if capabilities.archive == Some(ArchiveMode::Deletes) && !allow_delete =>
        {
            return Err(CliError::delete_flag(
                "archive deletes this provider session; pass --allow-delete to continue",
            ));
        }
        Mutation::Stop if !capabilities.stop => {
            return Err(CliError::unsupported(format!(
                "{} does not support stop for this session",
                session.backend.name()
            )));
        }
        Mutation::Rename if !capabilities.rename => {
            return Err(CliError::unsupported(format!(
                "{} does not support rename for this session",
                session.backend.name()
            )));
        }
        Mutation::Unarchive if !capabilities.can_unarchive() => {
            return Err(CliError::unsupported(format!(
                "{} does not support unarchive for this session",
                session.backend.name()
            )));
        }
        _ => {}
    }
    let result = match action {
        Mutation::Archive => source.backend.archive(&session),
        Mutation::Stop => source.backend.stop(&session),
        Mutation::Rename => source
            .backend
            .rename(&session, name.expect("validated by clap")),
        Mutation::Unarchive => source.backend.unarchive(&session),
    };
    let db = ViewerDb::open_default().ok();
    invalidate_backend_scope(db.as_ref(), source.backend.as_ref());
    result.map_err(|error| CliError::backend(format!("{} failed: {error}", action.name())))?;
    let provider = session.backend.name();
    let target = format!("{provider}:{}", session.id);
    Ok(Success {
        value: json!({
            "schema_version": 1,
            "command": action.name(),
            "provider": provider,
            "id": session.id,
            "target": target,
            "outcome": "accepted",
        }),
        human: format!("accepted\t{}\t{target}", action.name()),
    })
}

fn resolve_target(sources: &[Source], id: &str) -> Result<(usize, Session), CliError> {
    let exact = target_matches(sources, |session| {
        session.id == id || session.short_id.as_deref() == Some(id)
    });
    let matches = if exact.is_empty() {
        target_matches(sources, |session| {
            session.id.starts_with(id)
                || session
                    .short_id
                    .as_deref()
                    .is_some_and(|short_id| short_id.starts_with(id))
        })
    } else {
        exact
    };
    match matches.as_slice() {
        [] => Err(CliError::missing(format!(
            "session {id} not found or provider unavailable"
        ))),
        [(index, session)] => Ok((*index, (*session).clone())),
        _ => Err(CliError::ambiguous(format!(
            "session prefix {id} matches {} sessions",
            matches.len()
        ))),
    }
}

fn target_matches(
    sources: &[Source],
    predicate: impl Fn(&Session) -> bool,
) -> Vec<(usize, &Session)> {
    let mut matches = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        for session in &source.sessions {
            if predicate(session) {
                matches.push((index, session));
            }
        }
    }
    matches
}

fn authoritative(source: &mut Source, id: &str) -> Result<Session, CliError> {
    source
        .backend
        .session_by_id(id)
        .map_err(|error| CliError::backend(format!("{}: {error}", source.backend.kind().name())))?
        .ok_or_else(|| CliError::missing(format!("session {id} not found or provider unavailable")))
}

fn run_wait(args: TargetArgs, timeout: Duration, json_output: bool) -> CliOutput {
    let supervised_worker = std::env::var_os(WAIT_WORKER_ENV).is_some();
    if supervised_worker && let Err(error) = start_wait_parent_monitor() {
        return CliOutput::failure(error, json_output);
    }
    if timeout.is_zero() || supervised_worker {
        return match wait_inline(args, timeout) {
            Ok((session, capabilities, code)) => {
                wait_output(session, capabilities, json_output, code)
            }
            Err(error) => CliOutput::failure(error, json_output),
        };
    }
    supervise_wait(args, timeout, json_output)
}

fn wait_inline(
    args: TargetArgs,
    timeout: Duration,
) -> Result<(Session, Capabilities, u8), CliError> {
    let spec = TargetSpec::parse(&args, false)?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| CliError::usage("wait timeout is too large"))?;
    let mut sources = load_sources(spec.provider)?;
    let (index, mut session) = resolve_target(&sources, &spec.id)?;
    loop {
        let capabilities = sources[index].backend.capabilities_for(&session);
        match &session.status {
            Status::Done | Status::NeedsInput { .. } => {
                return Ok((session, capabilities, 0));
            }
            Status::Error => return Ok((session, capabilities, 1)),
            Status::Working | Status::Idle | Status::Unknown => {}
        }
        if timeout.is_zero() || Instant::now() >= deadline {
            return Err(CliError::timeout());
        }
        std::thread::sleep(WAIT_POLL.min(deadline.saturating_duration_since(Instant::now())));
        let id = session.id.clone();
        match sources[index].backend.session_by_id(&id) {
            Ok(Some(fresh)) => session = fresh,
            Ok(None) if session.backend == BackendKind::Claude => continue,
            Err(_) if session.backend == BackendKind::Claude => continue,
            Ok(None) => {
                return Err(CliError::missing(format!(
                    "session {id} not found or provider unavailable"
                )));
            }
            Err(error) => {
                return Err(CliError::backend(format!(
                    "{}: {error}",
                    session.backend.name()
                )));
            }
        }
    }
}

fn wait_output(
    session: Session,
    capabilities: Capabilities,
    json_output: bool,
    code: u8,
) -> CliOutput {
    let row = SessionDto::new(session, capabilities);
    let human = format!("{}\t{}\t{}\t{}", row.target, row.status, row.title, row.cwd);
    let mut output = CliOutput::success(
        json!({"schema_version": 1, "command": "wait", "session": row}),
        human,
        json_output,
    );
    output.code = code;
    output
}

fn supervise_wait(args: TargetArgs, timeout: Duration, json_output: bool) -> CliOutput {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (args, timeout);
        return CliOutput::failure(
            CliError::unsupported("bounded wait cleanup is unsupported on this platform"),
            json_output,
        );
    }
    #[cfg(any(unix, windows))]
    {
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return CliOutput::failure(CliError::usage("wait timeout is too large"), json_output);
        };
        let mut command = match std::env::current_exe() {
            Ok(executable) => ProcessCommand::new(executable),
            Err(error) => {
                return CliOutput::failure(
                    CliError::backend(format!("could not locate wait worker: {error}")),
                    json_output,
                );
            }
        };
        command
            .env(WAIT_WORKER_ENV, "1")
            .arg("wait")
            .arg("--timeout")
            .arg(timeout.as_secs_f64().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(provider) = args.provider {
            command.arg("--provider").arg(provider.name());
        }
        if json_output {
            command.arg("--json");
        }
        command.arg("--").arg(&args.value);
        configure_wait_process_group(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return CliOutput::failure(
                    CliError::backend(format!("could not start wait worker: {error}")),
                    json_output,
                );
            }
        };
        let mut worker_stdin = child.stdin.take();
        let Some(stdout_rx) = child
            .stdout
            .take()
            .map(|pipe| capture_wait_output(pipe, WAIT_STDOUT_CAP))
        else {
            let cleanup = cancel_active_wait_worker(&mut child, &mut worker_stdin);
            return CliOutput::failure(
                CliError::backend(format!(
                    "wait worker had no stdout pipe{}",
                    cleanup
                        .err()
                        .map(|error| format!("; {error}"))
                        .unwrap_or_default()
                )),
                json_output,
            );
        };
        let Some(stderr_rx) = child
            .stderr
            .take()
            .map(|pipe| capture_wait_output(pipe, WAIT_STDERR_CAP))
        else {
            let cleanup = cancel_active_wait_worker(&mut child, &mut worker_stdin);
            return CliOutput::failure(
                CliError::backend(format!(
                    "wait worker had no stderr pipe{}",
                    cleanup
                        .err()
                        .map(|error| format!("; {error}"))
                        .unwrap_or_default()
                )),
                json_output,
            );
        };
        let mut stdout = None;
        let mut stderr = None;
        loop {
            if let Err(error) = poll_wait_output(&stdout_rx, &mut stdout, "stdout") {
                let _ = cancel_active_wait_worker(&mut child, &mut worker_stdin);
                return CliOutput::failure(error, json_output);
            }
            if let Err(error) = poll_wait_output(&stderr_rx, &mut stderr, "stderr") {
                let _ = cancel_active_wait_worker(&mut child, &mut worker_stdin);
                return CliOutput::failure(error, json_output);
            }
            if stdout.is_some() && stderr.is_some() {
                let stdout = stdout.take().expect("checked above");
                let stderr = stderr.take().expect("checked above");
                drop(worker_stdin.take());
                let status = match terminate_active_wait_process_group(&mut child) {
                    Ok(status) => status,
                    Err(error) => {
                        return CliOutput::failure(CliError::backend(error), json_output);
                    }
                };
                if Instant::now() >= deadline {
                    return CliOutput::failure(CliError::timeout(), json_output);
                }
                let stdout = match checked_wait_output(stdout, "stdout") {
                    Ok(output) => output,
                    Err(error) => return CliOutput::failure(error, json_output),
                };
                let stderr = match checked_wait_output(stderr, "stderr") {
                    Ok(output) => output,
                    Err(error) => return CliOutput::failure(error, json_output),
                };
                return validate_wait_worker_output(status, stdout, stderr, json_output);
            }
            if Instant::now() >= deadline {
                return match cancel_active_wait_worker(&mut child, &mut worker_stdin) {
                    Ok(()) => CliOutput::failure(CliError::timeout(), json_output),
                    Err(error) => CliOutput::failure(CliError::backend(error), json_output),
                };
            }
            std::thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

fn validate_wait_worker_output(
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    json_output: bool,
) -> CliOutput {
    let code = status.code();
    let documented = matches!(code, Some(0 | 1 | 2 | 3 | 4 | 124));
    let valid_output = if json_output {
        serde_json::from_str::<serde_json::Value>(&stdout)
            .ok()
            .is_some_and(|value| {
                value.get("schema_version").and_then(|value| value.as_u64()) == Some(1)
                    && (value.get("error").is_some()
                        || value.get("command").and_then(|value| value.as_str()) == Some("wait"))
            })
    } else {
        !stdout.is_empty() || !stderr.is_empty()
    };
    if !documented || !valid_output {
        let diagnostic = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        let diagnostic = if diagnostic.is_empty() {
            String::new()
        } else {
            format!(": {}", diagnostic.chars().take(300).collect::<String>())
        };
        return CliOutput::failure(
            CliError::backend(format!(
                "wait worker exited with {} or invalid output{diagnostic}",
                code.map_or_else(|| "a signal".to_string(), |code| code.to_string())
            )),
            json_output,
        );
    }
    CliOutput {
        stdout,
        stderr,
        code: u8::try_from(code.expect("documented exit has a code")).unwrap_or(1),
    }
}

fn cancel_active_wait_worker(
    child: &mut std::process::Child,
    worker_stdin: &mut Option<std::process::ChildStdin>,
) -> Result<(), String> {
    drop(worker_stdin.take());
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                return terminate_active_wait_process_group(child).map(|_| ());
            }
        }
    }
}

fn start_wait_parent_monitor() -> Result<(), CliError> {
    verify_wait_worker_group()?;
    std::thread::Builder::new()
        .name("agent_viewer_wait_parent".to_string())
        .spawn(|| {
            let mut stdin = std::io::stdin();
            let mut buffer = [0u8; 64];
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => {
                        terminate_own_wait_process_group();
                        return;
                    }
                    Ok(_) => {}
                }
            }
        })
        .map(|_| ())
        .map_err(|error| CliError::backend(format!("could not monitor wait parent: {error}")))
}

struct CapturedWaitOutput {
    text: String,
    truncated: bool,
    readable: bool,
}

fn capture_wait_output(
    mut pipe: impl Read + Send + 'static,
    cap: usize,
) -> std::sync::mpsc::Receiver<CapturedWaitOutput> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut truncated = false;
        let readable = loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break true,
                Ok(count) => {
                    let retained = count.min(cap.saturating_sub(bytes.len()));
                    bytes.extend_from_slice(&chunk[..retained]);
                    truncated |= retained < count;
                }
                Err(_) => break false,
            }
        };
        let _ = sender.send(CapturedWaitOutput {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
            readable,
        });
    });
    receiver
}

fn poll_wait_output(
    receiver: &std::sync::mpsc::Receiver<CapturedWaitOutput>,
    output: &mut Option<CapturedWaitOutput>,
    stream: &str,
) -> Result<(), CliError> {
    if output.is_some() {
        return Ok(());
    }
    match receiver.try_recv() {
        Ok(captured) => *output = Some(captured),
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            return Err(CliError::backend(format!(
                "wait worker {stream} capture stopped unexpectedly"
            )));
        }
    }
    Ok(())
}

fn checked_wait_output(output: CapturedWaitOutput, stream: &str) -> Result<String, CliError> {
    if !output.readable {
        return Err(CliError::backend(format!(
            "wait worker {stream} was unreadable"
        )));
    }
    if output.truncated {
        return Err(CliError::backend(format!(
            "wait worker {stream} exceeded its output limit"
        )));
    }
    Ok(output.text)
}

#[cfg(unix)]
fn configure_wait_process_group(command: &mut ProcessCommand) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_wait_process_group(_command: &mut ProcessCommand) {}

#[cfg(unix)]
fn verify_wait_worker_group() -> Result<(), CliError> {
    // SAFETY: these calls only read process identity maintained by the kernel.
    let (pid, process_group) = unsafe { (libc::getpid(), libc::getpgrp()) };
    if pid == process_group {
        Ok(())
    } else {
        Err(CliError::backend(
            "internal wait worker did not start in its owned process group",
        ))
    }
}

#[cfg(windows)]
fn verify_wait_worker_group() -> Result<(), CliError> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_wait_worker_group() -> Result<(), CliError> {
    Err(CliError::unsupported(
        "bounded wait cleanup is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn terminate_own_wait_process_group() {
    // SAFETY: `verify_wait_worker_group` proved this worker leads its own group. Killing that
    // group reaches only the worker and provider processes it started.
    unsafe {
        libc::kill(-libc::getpgrp(), libc::SIGKILL);
    }
}

#[cfg(windows)]
fn terminate_own_wait_process_group() {
    let _ = ProcessCommand::new("taskkill")
        .args(["/PID", &std::process::id().to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(not(any(unix, windows)))]
fn terminate_own_wait_process_group() {}

#[cfg(unix)]
fn terminate_active_wait_process_group(
    child: &mut std::process::Child,
) -> Result<std::process::ExitStatus, String> {
    let process_group = i32::try_from(child.id()).unwrap_or(i32::MAX);
    // SAFETY: the child was placed in a fresh process group whose id is its pid. The negative
    // target reaches only that worker and the provider processes it owns.
    let killed = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    let signal_error = std::io::Error::last_os_error();
    let waited = child
        .wait()
        .map_err(|error| format!("could not reap wait worker: {error}"))?;
    if killed == 0 || signal_error.raw_os_error() == Some(libc::ESRCH) {
        Ok(waited)
    } else {
        Err(format!(
            "could not terminate wait worker group: {signal_error}"
        ))
    }
}

#[cfg(windows)]
fn terminate_active_wait_process_group(
    child: &mut std::process::Child,
) -> Result<std::process::ExitStatus, String> {
    match child.try_wait() {
        Ok(Some(status)) => return Ok(status),
        Ok(None) => {}
        Err(error) => return Err(format!("could not inspect wait worker: {error}")),
    }
    let output = ProcessCommand::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .output()
        .map_err(|error| format!("could not run taskkill for wait worker: {error}"))?;
    let waited = child
        .wait()
        .map_err(|error| format!("could not reap wait worker: {error}"))?;
    if output.status.success() {
        Ok(waited)
    } else {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "taskkill failed for wait worker with {}: {}",
            output.status,
            diagnostic.trim()
        ))
    }
}
