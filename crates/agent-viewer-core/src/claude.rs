use crate::backend::{Backend, BackendKind, Capabilities, PrRef, Session, SessionOrigin, Status};
use crate::error::{AttachRefusal, Error, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::SystemTime;

pub struct ClaudeBackend {
    binary: String,
    /// Root of claude's per-job state dirs (`~/.claude/jobs`). Injectable so the rename
    /// round-trip is testable against a real directory instead of the developer's own jobs.
    jobs_root: PathBuf,
    /// Root of claude's per-process registry (`~/.claude/sessions`), the only publisher of
    /// `entrypoint`. Injectable for the same reason as `jobs_root`: the companion rule must be
    /// testable against a fixture directory, not the developer's own live sessions.
    sessions_root: PathBuf,
    /// Per-job state.json cache keyed by (mtime, len): the file is re-read and re-parsed
    /// only when it changes, not every tick (mirrors the codex StatusResolver pattern).
    detail_cache: HashMap<PathBuf, ((SystemTime, u64), JobDetail)>,
    /// Discovered model list, computed once and reused (best-effort; degrades to just the
    /// opus[1m] default when ~/.claude.json is missing or unreadable).
    models_cache: OnceLock<Vec<String>>,
}

impl ClaudeBackend {
    pub fn new() -> ClaudeBackend {
        ClaudeBackend::with_binary("claude")
    }
    pub fn with_binary(binary: &str) -> ClaudeBackend {
        ClaudeBackend::with_binary_and_jobs_root(binary, default_jobs_root())
    }
    pub fn with_binary_and_jobs_root(binary: &str, jobs_root: PathBuf) -> ClaudeBackend {
        ClaudeBackend::with_roots(binary, jobs_root, default_sessions_root())
    }
    pub fn with_roots(binary: &str, jobs_root: PathBuf, sessions_root: PathBuf) -> ClaudeBackend {
        ClaudeBackend {
            binary: binary.to_string(),
            jobs_root,
            sessions_root,
            detail_cache: HashMap::new(),
            models_cache: OnceLock::new(),
        }
    }

    /// `<jobs_root>/<short_id>/state.json` - claude's own store of record for a bg job's
    /// name, summary, transcript path, and respawn contract.
    fn job_state_path(&self, short_id: &str) -> PathBuf {
        job_state_path_in(&self.jobs_root, short_id)
    }

    /// The parsed jobs state.json for `path`, plus its mtime (the caller's updated_at
    /// signal). Re-read and re-parsed only when (mtime, len) changes; None when the file
    /// is missing/unreadable (the jobs dir can lag the agents list).
    fn cached_job_detail(&mut self, path: &PathBuf) -> Option<(SystemTime, JobDetail)> {
        let meta = std::fs::metadata(path).ok()?;
        let key = (
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            meta.len(),
        );
        if let Some((cached_key, detail)) = self.detail_cache.get(path)
            && *cached_key == key
        {
            return Some((key.0, detail.clone()));
        }
        let detail = parse_job_state(&std::fs::read_to_string(path).ok()?);
        self.detail_cache
            .insert(path.clone(), (key, detail.clone()));
        Some((key.0, detail))
    }

    fn remove_for_platform(
        &self,
        platform: crate::platform::Platform,
        session: &Session,
    ) -> Result<()> {
        if !capabilities_for_session_on_platform(platform, session).delete {
            return Err(Error::Unsupported(self.kind().name()));
        }
        // `claude rm <short_id>` deletes the bg session (and its worktree). Blocking, headless,
        // and delegated to the CLI (never a raw jobs-dir delete). A row with no short id has no
        // bg job to remove, so it is Unsupported rather than a stray no-op.
        let Some(short_id) = session
            .short_id
            .as_deref()
            .filter(|short_id| !short_id.is_empty())
        else {
            return Err(Error::Unsupported(self.kind().name()));
        };
        crate::spawn::run_checked(&mut claude_rm_command(
            std::path::Path::new(&self.binary),
            short_id,
        ))
    }
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the `claude --bg` spawn command (extracted so the model-flag wiring is unit-
/// testable). `model` None falls back to the opus[1m] default.
fn claude_spawn_command(
    binary: &str,
    dir: &std::path::Path,
    task: &str,
    model: Option<&str>,
) -> std::process::Command {
    let name = crate::spawn::truncated_title(task);
    let mut cmd = std::process::Command::new(binary);
    cmd.current_dir(dir)
        .arg("--bg")
        .arg("--model")
        .arg(model.unwrap_or("opus[1m]"))
        .arg("--name")
        .arg(&name)
        .arg(task);
    cmd
}

/// Build the `claude rm <short_id>` remove command (extracted so the exact argv is unit-
/// testable, mirroring `claude_spawn_command`). `binary` is a &Path so the caller can pass
/// its configured binary directly. Deleting a bg session is headless; a bogus id is a no-op.
fn claude_rm_command(binary: &std::path::Path, short_id: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("rm").arg(short_id);
    cmd
}

pub fn capabilities_for_platform(platform: crate::platform::Platform) -> Capabilities {
    Capabilities {
        spawn: true,
        attach: true,
        rename: platform != crate::platform::Platform::Windows,
        archive: false,
        delete: true,
        stop: false,
        needs_input: true,
        pr_refs: true,
        live_status: true,
    }
}

pub fn capabilities_for_session_on_platform(
    platform: crate::platform::Platform,
    session: &Session,
) -> Capabilities {
    let mut capabilities = capabilities_for_platform(platform);
    let has_short_id = session
        .short_id
        .as_deref()
        .is_some_and(|short_id| !short_id.is_empty());
    capabilities.rename &= has_short_id;
    capabilities.delete = has_short_id
        && (platform == crate::platform::Platform::Linux
            || (session.pid.is_none() && session.status.is_finished()));
    capabilities
}

impl Backend for ClaudeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Claude
    }
    fn capabilities(&self) -> Capabilities {
        // DELIBERATE DIVERGENCE from the capability table in
        // specs/001-fleet-view-unification/data-model.md, which marks claude `archive` and
        // `stop` as yes. Reason: `ClaudeBackend` implements neither the `hide` nor the `stop`
        // trait method below, so the trait defaults return Unsupported for both. Advertising
        // true here would be a promise this backend cannot keep, and a capability that is
        // advertised and then fails at press time is worse than one advertised as
        // unsupported, which is exactly what the footer notice exists to signal. Keep the
        // gate; the table is the coarser statement.
        capabilities_for_platform(crate::platform::current_platform())
    }
    fn capabilities_for(&self, session: &Session) -> Capabilities {
        // Per-row, not backend-wide: `claude rm` keys off the short id, so an interactive row
        // (which carries none) has nothing to remove. Callers check this BEFORE terminating.
        // Rename rides the same gate for the same reason - the short id names the job dir
        // whose state.json holds the name, and an interactive row has no job dir at all.
        capabilities_for_session_on_platform(crate::platform::current_platform(), session)
    }
    fn list(&mut self) -> Result<Vec<Session>> {
        // A missing/failing binary or non-zero exit is a quiet empty backend, not an error.
        // `--all` includes completed sessions (required to populate the DONE section).
        let output = std::process::Command::new(&self.binary)
            .arg("agents")
            .arg("--json")
            .arg("--all")
            .output();
        let output = match output {
            Ok(output) if output.status.success() => output,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut sessions = parse_agents_json(&stdout)?;
        for session in &mut sessions {
            // Fill summary/updated_at_ms/rollout_path from the jobs state.json.
            if let Some(short_id) = session.short_id.as_deref() {
                let path = job_state_path_in(&self.jobs_root, short_id);
                if let Some((mtime, detail)) = self.cached_job_detail(&path) {
                    session.summary = detail.summary;
                    session.rollout_path = detail.transcript_path;
                    session.pr_refs = detail.prs;
                    if let Ok(since) = mtime.duration_since(std::time::UNIX_EPOCH) {
                        session.updated_at_ms = since.as_millis() as i64;
                    }
                }
            }
        }
        // Hide nested `claude -p` children. The entrypoint lives only in the per-process
        // registry, so this is a second (tiny, uncached) read per LIVE row; rows with no pid
        // are finished jobs and are skipped without touching the disk.
        let sessions_root = self.sessions_root.clone();
        mark_sdk_companions(&mut sessions, |pid| {
            std::fs::read_to_string(session_state_path_in(&sessions_root, pid))
                .ok()
                .as_deref()
                .and_then(parse_session_registry)
        });
        sessions.sort_by_key(|session| session.updated_at_ms);
        Ok(sessions)
    }
    fn turn_activity(&self, session: &Session, window: std::time::Duration) -> Result<Vec<i64>> {
        let Some(path) = session
            .rollout_path
            .as_deref()
            .filter(|path| path.is_file())
        else {
            return Ok(Vec::new());
        };
        read_claude_turn_activity(path, window)
    }
    fn available_models(&self) -> Vec<String> {
        // opus[1m] (the non-pinned alias) first, then whatever ~/.claude.json advertises.
        self.models_cache
            .get_or_init(|| {
                let mut models = vec![self.kind().default_model().to_string()];
                let path = crate::home_dir().join(".claude.json");
                if let Ok(text) = std::fs::read_to_string(&path) {
                    models.extend(parse_claude_json_models(&text));
                }
                crate::backend::dedup_preserve(models)
            })
            .clone()
    }
    fn spawn(
        &self,
        dir: &std::path::Path,
        task: &str,
        model: Option<&str>,
    ) -> Result<crate::backend::SpawnResult> {
        // Detach like the other backends so the TUI key handler returns immediately
        // (`claude --bg` still self-detaches; setsid + no wait keeps it off this thread).
        let cmd = claude_spawn_command(&self.binary, dir, task, model);
        let log_path = crate::spawn::viewer_log_path("claude");
        crate::spawn::spawn_detached(cmd, &log_path)?;
        // The forked pid is the dispatcher CLI, not the worker; claude rows are never
        // companions so spawn pinning is unnecessary.
        Ok(crate::backend::SpawnResult::default())
    }
    /// Rename a daemon-backed bg job the way claude itself does: a read-modify-write of
    /// `name`/`nameSource` in that job's `state.json`. Verified against the 2.1.220 bundle,
    /// whose fleet view Ctrl+R calls exactly this state writer for daemon rows (its failure
    /// notice, "its state file is unwritable", is the tell), and verified live on this box -
    /// writing the field made `claude agents --json` report the new name on the next listing.
    ///
    /// This is the one place the viewer writes another tool's store, and it is deliberate:
    /// claude ships no `rename` subcommand, the state file IS the store of record for a job's
    /// name, and the alternative (a viewer-local override) is the shadow state that makes the
    /// list disagree with `claude agents`. The rendezvous socket is NOT the channel: it
    /// authenticates its first frame as `attacher-caps`, merely opening it evicts the
    /// daemon's supervisor connection for a live session, and the `rename_session` frame we
    /// once sent there belongs to the unrelated SDK/bridge control protocol.
    ///
    /// Racing a live worker's own state write is possible and accepted: the worker re-reads
    /// the file immediately before each write, so it merges this name rather than reverting
    /// it, which is the same race claude's own fleet view runs.
    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        if !capabilities_for_platform(crate::platform::current_platform()).rename {
            return Err(Error::Unsupported(BackendKind::Claude.name()));
        }
        let Some(short_id) = session
            .short_id
            .as_deref()
            .filter(|short_id| !short_id.is_empty())
        else {
            return Err(Error::Unsupported(BackendKind::Claude.name()));
        };
        let path = self.job_state_path(short_id);
        // Read-modify-write, never a blind overwrite: state.json carries the job's respawn
        // contract, and a missing file means the job is gone (Err, so the TUI reports it).
        let existing = std::fs::read_to_string(&path)?;
        replace_atomic(
            &path,
            &job_state_with_name(&existing, name, SystemTime::now())?,
        )
    }
    fn remove(&self, session: &Session) -> Result<()> {
        self.remove_for_platform(crate::platform::current_platform(), session)
    }
    fn attach_command(
        &self,
        session: &Session,
    ) -> std::result::Result<std::process::Command, AttachRefusal> {
        let mut cmd = std::process::Command::new(&self.binary);
        // `claude attach <short_id>` resumes the SAME thread whether the session is live or
        // done: it wakes a finished session in place and resolves the session's own cwd from
        // its jobs dir, so there is no cwd pin and no CLAUDE_AGENTS_SELECT. Only a row with no
        // short id (a rare jobs entry missing its "id" key) falls back to `claude -r <full id>`,
        // pinned to its cwd when that dir still exists.
        match session.short_id.as_deref() {
            Some(short_id) if !short_id.is_empty() => {
                cmd.arg("attach").arg(short_id);
            }
            _ => {
                cmd.arg("-r").arg(&session.id);
                if session.cwd.is_dir() {
                    cmd.current_dir(&session.cwd);
                }
            }
        }
        Ok(cmd)
    }
}

/// Pre-accept the claude trust dialog for `cwd` in the config JSON at `config_path`, so a
/// non-interactive attach into a fresh project does not stall on the trust prompt. Returns
/// Ok(false) (no write) when `cwd` or any ancestor is already accepted, Ok(true) when the
/// flag was merged in (or the file was created). Every other key at every level is
/// preserved; the write is atomic (temp file in the same dir + rename). This is claude's
/// own sanctioned field — its error text instructs setting exactly `hasTrustDialogAccepted`
/// for non-interactive use.
pub fn ensure_trusted(config_path: &std::path::Path, cwd: &std::path::Path) -> Result<bool> {
    // Only a MISSING file may be created fresh. A read error or a parse failure returns Err
    // WITHOUT writing — substituting an empty object and renaming over the original would
    // erase the user's whole claude config (account, projects, MCP). The TUI caller treats
    // Err as best-effort and just proceeds with the attach.
    let mut config: serde_json::Value = match std::fs::read_to_string(config_path) {
        Ok(text) => serde_json::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };

    // A valid-but-non-object root (`[]`, `"str"`, a number) is not a claude config; return
    // Err WITHOUT writing rather than panicking on the object accessors below.
    if !config.is_object() {
        return Err(Error::Command("claude config is not a JSON object".into()));
    }

    // Already trusted if cwd or any ancestor has hasTrustDialogAccepted == true.
    if let Some(projects) = config.get("projects").and_then(|p| p.as_object()) {
        let mut dir = Some(cwd);
        while let Some(d) = dir {
            let accepted = projects
                .get(&d.to_string_lossy().into_owned())
                .and_then(|p| p.get("hasTrustDialogAccepted"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if accepted {
                return Ok(false);
            }
            dir = d.parent();
        }
    }

    // Merge hasTrustDialogAccepted: true into projects[<cwd>], preserving every other key.
    let projects = config
        .as_object_mut()
        .expect("json object")
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    if !projects.is_object() {
        *projects = serde_json::json!({});
    }
    let cwd_key = cwd.to_string_lossy().into_owned();
    let project = projects
        .as_object_mut()
        .expect("projects object")
        .entry(cwd_key)
        .or_insert_with(|| serde_json::json!({}));
    if !project.is_object() {
        *project = serde_json::json!({});
    }
    project.as_object_mut().expect("project object").insert(
        "hasTrustDialogAccepted".to_string(),
        serde_json::json!(true),
    );

    // Atomic write: temp file in the same dir, then rename over the target.
    let text = serde_json::to_string_pretty(&config)?;
    let dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        config_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "claude.json".to_string())
    ));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, config_path)?;
    Ok(true)
}

/// Companion filter for claude - the peer of the codex `Source::is_companion` rule and the
/// opencode `is_run_mode_permission` rule, which claude previously had no equivalent of.
///
/// Every live claude process registers itself in `~/.claude/sessions/<pid>.json`, and
/// `claude agents --json --all` returns all of them. That includes a NESTED `claude -p` (the
/// Agent SDK's headless entrypoint) that another session shelled out to - a skill, a hook, an
/// `/implement` planning pass. It is a real process, but not a fleet member anyone started,
/// and it carries no `jobId`, so claude derives its name from the cwd as `<dir-basename>-<n>`
/// ("opencode-server-runtime-69"). Those rows read as mystery sessions in the default view and
/// vanish when the child exits, since the pid file is removed on exit.
///
/// The discriminator is `entrypoint`, matched on the `sdk-` PREFIX so the whole Agent SDK
/// family (`sdk-cli`, `sdk-ts`, `sdk-py`) is one rule. Two alternatives were rejected because
/// a genuine interactive terminal session is indistinguishable from a nested `claude -p` under
/// both: `kind == "interactive"` is what a real terminal session reports too, and the absence
/// of `id`/`jobId` is equally true of one. A `--bg` job and an interactive session BOTH report
/// `entrypoint: "cli"`, which is exactly what makes the `sdk-` prefix the safe cut.
///
/// Same safe direction as the opencode rule: anything unrecognized is treated as a real
/// session, because a shown row one keypress from hidden beats a hidden row you cannot find.
/// Companion only HIDES (Ctrl+A reveals); it never drops the row.
pub fn is_sdk_entrypoint(entrypoint: Option<&str>) -> bool {
    entrypoint.is_some_and(|value| value.starts_with("sdk-"))
}

/// `<sessions_root>/<pid>.json` - the per-process registry entry claude writes for every live
/// session and removes on exit. This is the ONLY place `entrypoint` is published: `claude
/// agents --json --all` projects just cwd/id/kind/name/sessionId/startedAt/state/pid, so the
/// companion rule cannot be decided from the agents output alone (verified live on this box
/// 2026-07-27 against claude 2.1.220 - reading `entrypoint` off an agents row always yields
/// None, which is why this second read exists).
fn session_state_path_in(sessions_root: &std::path::Path, pid: u32) -> PathBuf {
    sessions_root.join(format!("{pid}.json"))
}

/// The two fields of a `~/.claude/sessions/<pid>.json` the companion rule needs: what kind of
/// process it is, and WHICH session it is (the identity the pid-reuse guard checks).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionRegistryEntry {
    pub session_id: Option<String>,
    pub entrypoint: Option<String>,
}

/// PURE: parse a `~/.claude/sessions/<pid>.json`. Absent, non-string, or unparseable fields
/// come back as None rather than an error, so a schema drift degrades to "real session".
/// Never panics.
pub fn parse_session_registry(text: &str) -> Option<SessionRegistryEntry> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(SessionRegistryEntry {
        session_id: crate::json_str(&value, "sessionId").map(str::to_string),
        entrypoint: crate::json_str(&value, "entrypoint").map(str::to_string),
    })
}

/// PURE, unit-tested: flag every session whose registry entry is an SDK one as a companion.
/// `registry_of` resolves a pid to that entry (the caller reads the file; the test passes a
/// map), so the rule itself is testable with no filesystem at all.
///
/// Sessions with NO pid are left alone: a pid is absent exactly for finished background jobs,
/// which are real fleet members. Only ever SETS companion, mirroring `mark_dead_dirs` - a
/// session another rule already flagged stays flagged.
///
/// **Pid-reuse guard.** The registry is keyed by pid and the kernel recycles pids, so between
/// the `claude agents` snapshot and this read the file at `<pid>.json` can belong to a
/// DIFFERENT session - and if that new process is an SDK one, a real row would be hidden. The
/// entry's own `sessionId` must therefore match the row's before the flag is applied. A
/// missing or mismatched `sessionId` means "not proven to be this session", so the row stays
/// visible: the same safe direction the rest of this rule takes.
pub fn mark_sdk_companions(
    sessions: &mut [Session],
    registry_of: impl Fn(u32) -> Option<SessionRegistryEntry>,
) {
    for session in sessions.iter_mut() {
        if session.companion {
            continue;
        }
        let Some(pid) = session.pid else {
            continue;
        };
        let Some(entry) = registry_of(pid) else {
            continue;
        };
        if entry.session_id.as_deref() != Some(session.id.as_str()) {
            continue;
        }
        if is_sdk_entrypoint(entry.entrypoint.as_deref()) {
            session.companion = true;
        }
    }
}

/// PURE parser, unit tested against the fixture. Input: stdout of
/// `claude agents --json --all`, a JSON array of objects.
/// Mapping: id = sessionId (attach takes it); title = name; cwd = cwd;
/// created_at_ms = updated_at_ms = startedAt; hidden = false;
/// origin comes from kind; companion = false (decided later by `mark_sdk_companions`, since
/// the agents output does not publish `entrypoint`); summary = "".
/// state: "working" -> Working, "blocked" -> NeedsInput, "idle" -> Idle,
/// "done" -> Done, "failed" -> Error, "stopped" -> Done,
/// missing or unknown -> Unknown. pid: entry "pid" as u32 when present.
/// The SHORT id (entry "id") the caller needs for the jobs path is folded into
/// `Session.short_id`. Entries missing sessionId/cwd/name are SKIPPED. Non-array top
/// level -> Err(Json).
pub fn parse_agents_json(stdout: &str) -> Result<Vec<Session>> {
    // Non-array top level surfaces as Err(Json) via the From conversion.
    let entries: Vec<serde_json::Value> = serde_json::from_str(stdout.trim())?;
    let mut sessions = Vec::with_capacity(entries.len());
    for entry in entries {
        // sessionId/cwd/name are required; anything missing them is skipped.
        let Some(session_id) = crate::json_str(&entry, "sessionId") else {
            continue;
        };
        let Some(cwd) = crate::json_str(&entry, "cwd") else {
            continue;
        };
        let Some(name) = crate::json_str(&entry, "name") else {
            continue;
        };
        let short_id = crate::json_str(&entry, "id")
            .filter(|short_id| !short_id.is_empty())
            .map(str::to_string);
        let started_at = entry.get("startedAt").and_then(|v| v.as_i64()).unwrap_or(0);
        let origin = if crate::json_str(&entry, "kind") == Some("background") {
            SessionOrigin::Background
        } else {
            SessionOrigin::Interactive
        };
        let status = match crate::json_str(&entry, "state") {
            Some("working") => Status::Working,
            Some("blocked") => Status::needs_input(),
            Some("idle") => Status::Idle,
            Some("done") => Status::Done,
            Some("failed") => Status::Error,
            Some("stopped") => Status::Done,
            _ => Status::Unknown,
        };
        let pid = entry.get("pid").and_then(|v| v.as_u64()).map(|p| p as u32);
        sessions.push(Session {
            backend: BackendKind::Claude,
            id: session_id.to_string(),
            short_id,
            origin,
            title: name.to_string(),
            cwd: std::path::PathBuf::from(cwd),
            git_branch: None,
            status,
            created_at_ms: started_at,
            updated_at_ms: started_at,
            hidden: false,
            // The agents output does not publish `entrypoint`; `list()` applies the companion
            // rule afterwards via `mark_sdk_companions`.
            companion: false,
            summary: String::new(),
            pid,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        });
    }
    Ok(sessions)
}

/// PURE: candidate model ids collected from a claude config JSON (`~/.claude.json`).
/// Order: every `additionalModelOptionsCache[].value` (a top-level array of objects) in
/// array order, then the UNION of every `projects.<path>.lastModelUsage` key (each is a
/// map of model-id -> usage stats) sorted ascending. Empty strings are skipped; the
/// caller dedups. Malformed/missing keys yield whatever can be collected; never panics.
pub fn parse_claude_json_models(json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // additionalModelOptionsCache: [{ "value": "<model>" }, ...] in array order.
    if let Some(cache) = value
        .get("additionalModelOptionsCache")
        .and_then(|c| c.as_array())
    {
        for entry in cache {
            if let Some(v) = crate::json_str(entry, "value")
                && !v.is_empty()
            {
                out.push(v.to_string());
            }
        }
    }
    // projects.<path>.lastModelUsage keys, unioned across projects and sorted ascending
    // (BTreeSet gives both dedup and sort for free).
    let mut usage_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(projects) = value.get("projects").and_then(|p| p.as_object()) {
        for project in projects.values() {
            if let Some(usage) = project.get("lastModelUsage").and_then(|u| u.as_object()) {
                for key in usage.keys() {
                    if !key.is_empty() {
                        usage_keys.insert(key.clone());
                    }
                }
            }
        }
    }
    out.extend(usage_keys);
    out
}

/// Parsed subset of a claude jobs `state.json` (verified fields 2026-07-11).
/// The file's own ISO updatedAt is deliberately NOT parsed — the caller uses the
/// state.json file mtime as the updated_at signal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobDetail {
    /// needs if state=="blocked" && needs present, else detail, else "".
    pub summary: String,
    /// linkScanPath (verified field).
    pub transcript_path: Option<std::path::PathBuf>,
    /// `children[].id` where `kind == "pr"` — the associated pull requests.
    pub prs: Vec<PrRef>,
}

/// PURE parse of state.json text (verified fields: state, detail, needs, linkScanPath;
/// blocked jobs carry needs, working/done carry detail).
pub fn parse_job_state(text: &str) -> JobDetail {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return JobDetail::default();
    };
    let needs = crate::json_str(&value, "needs");
    let summary = if crate::json_str(&value, "state") == Some("blocked")
        && let Some(needs) = needs
    {
        needs.to_string()
    } else if let Some(detail) = crate::json_str(&value, "detail") {
        detail.to_string()
    } else {
        String::new()
    };
    let transcript_path = crate::json_str(&value, "linkScanPath").map(std::path::PathBuf::from);
    // children: [{id, href, kind}] — keep the kind=="pr" entries as PrRef.
    let prs = value
        .get("children")
        .and_then(|v| v.as_array())
        .map(|children| {
            children
                .iter()
                .filter(|c| crate::json_str(c, "kind") == Some("pr"))
                .filter_map(|c| {
                    crate::json_str(c, "id").map(|id| PrRef {
                        id: id.to_string(),
                        href: crate::json_str(c, "href").map(String::from),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    JobDetail {
        summary,
        transcript_path,
        prs,
    }
}

/// $CLAUDE_CONFIG_DIR/jobs if set, else $HOME/.claude/jobs.
pub fn default_jobs_root() -> PathBuf {
    jobs_root_from(
        std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
        &crate::home_dir(),
    )
}

/// $CLAUDE_CONFIG_DIR/sessions if set, else $HOME/.claude/sessions. Same env precedence as
/// the jobs root, and for the same reason: a viewer that inherited `CLAUDE_CONFIG_DIR` must
/// read the same tree the `claude` it shells out to is writing.
pub fn default_sessions_root() -> PathBuf {
    sessions_root_from(
        std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
        &crate::home_dir(),
    )
}

/// PURE peer of `jobs_root_from` for the per-process registry dir.
pub fn sessions_root_from(config_dir: Option<&str>, home: &std::path::Path) -> PathBuf {
    match config_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("sessions"),
        None => home.join(".claude/sessions"),
    }
}

/// The jobs root for a given config dir, split out from the env read so the precedence is
/// testable. `claude agents` lists jobs out of `$CLAUDE_CONFIG_DIR` when it is set, so a
/// viewer that inherited that env must read AND write the same tree - otherwise rename either
/// fails on a missing file or writes a same-short-id job in the default tree.
pub fn jobs_root_from(config_dir: Option<&str>, home: &std::path::Path) -> PathBuf {
    match config_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("jobs"),
        None => home.join(".claude/jobs"),
    }
}

/// <jobs_root>/<short_id>/state.json
pub fn job_state_path_in(jobs_root: &std::path::Path, short_id: &str) -> PathBuf {
    jobs_root.join(short_id).join("state.json")
}

/// Set `name`/`nameSource` on a parsed job state, preserving every other field. Split out
/// from the filesystem so the merge itself is provable: a rename must never drop
/// `respawnFlags`, `intent`, or anything else the job needs to respawn. `Err` when the file
/// is not a JSON object, since a synthesized replacement would drop exactly those fields.
fn job_state_with_name(existing: &str, name: &str, now: SystemTime) -> Result<String> {
    let mut state: serde_json::Value = serde_json::from_str(existing)?;
    let Some(object) = state.as_object_mut() else {
        return Err(Error::Command(
            "claude job state.json is not a JSON object".to_string(),
        ));
    };
    object.insert("name".to_string(), serde_json::Value::from(name));
    // `user` is what fleet view stamps for a human rename; it is what stops claude's own
    // auto-titler from overwriting the name later.
    object.insert("nameSource".to_string(), serde_json::Value::from("user"));
    // Claude's own writer stamps this on every state write, so a rename that left it stale
    // would be invisible to anything sorting or invalidating on the field.
    object.insert(
        "updatedAt".to_string(),
        serde_json::Value::from(iso8601_utc_millis(now)),
    );
    Ok(serde_json::to_string_pretty(&state)?)
}

/// `SystemTime` as `YYYY-MM-DDTHH:MM:SS.mmmZ` — the exact shape JavaScript's `toISOString()`
/// produces, because the value lands in a field claude writes with that call. Pre-epoch times
/// clamp to the epoch (they cannot occur for a job state and are not worth a signed path).
pub fn iso8601_utc_millis(time: SystemTime) -> String {
    let since = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (secs, millis) = (since.as_secs(), since.subsec_millis());
    let (days, secs_of_day) = ((secs / 86_400) as i64, secs % 86_400);
    // days_to_civil (Howard Hinnant's civil-from-days), shifted to an era starting 0000-03-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    )
}

/// Create `path` for writing, owner-read/write only, failing if it already exists. Split out
/// so the creation mode is directly assertable: the security property is that the file is
/// NEVER group/other readable, not even for the instant between creation and a later chmod,
/// and a test that watches for the temp from another thread can only observe that by luck.
/// umask can only clear bits, so 0600 is an upper bound, never a floor.
#[cfg(unix)]
pub fn create_owner_only(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::os::unix::fs::OpenOptionsExt::mode(
        std::fs::OpenOptions::new().write(true).create_new(true),
        0o600,
    )
    .open(path)
}

#[cfg(not(unix))]
pub fn create_owner_only(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Replace `path`'s contents with `body` atomically: write a temp file beside it, then
/// rename over the target, so a concurrent reader (the claude daemon, our own list tick)
/// sees either the old file or the new one, never a partial write. Mirrors claude's own
/// state writer.
///
/// The temp file inherits the target's mode BEFORE the rename. Claude writes state.json 0600
/// while the jobs dir itself is traversable, so leaving the temp at the umask default (0644 or
/// 0664) would quietly publish that job's intent, output, respawn flags, and transcript path
/// to every local user.
///
/// REPLACE, never create: a missing target is an error. `claude rm` can unlink state.json
/// between the caller's read and this write (the mutation runner keys rename and remove
/// separately, so they can overlap), and resurrecting the file there would leave a ghost job
/// behind the removal.
#[cfg(unix)]
pub fn replace_atomic(path: &std::path::Path, body: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let tmp = dir.join(format!(
        ".{}.agent-viewer.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    // Also the freshness check: metadata on the target is what proves it still exists.
    let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path)?.permissions());
    // Create OWNER-ONLY, before a single byte is written: chmod-after-write leaves a window in
    // which another local user can open the temp and read the whole job state. umask can only
    // clear bits, so the file is never wider than 0600 at creation.
    let write = |tmp: &std::path::Path| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = create_owner_only(tmp)?;
        file.write_all(body.as_bytes())?;
        // Widen to the target's own mode only once the content is in place.
        file.set_permissions(
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(mode),
        )?;
        Ok(())
    };
    if let Err(e) = write(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn replace_atomic(_path: &std::path::Path, _body: &str) -> Result<()> {
    Err(Error::Unsupported(BackendKind::Claude.name()))
}

/// Peek parser for the claude session JSONL (verified record shapes 2026-07-11):
/// keep type=="user" (message.content is a STRING, or a list with
/// content[].type=="text") and type=="assistant" (content[].type=="text";
/// "thinking"/tool blocks skipped); skip attachment/system/queue-operation/etc.
/// Return at most the LAST max_items items. Reuses codex::rollout::TranscriptItem.
pub fn read_claude_transcript(
    path: &std::path::Path,
    max_items: usize,
) -> Result<Vec<crate::codex::rollout::TranscriptItem>> {
    use crate::codex::rollout::TranscriptItem;
    use std::io::BufRead;

    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut items = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Some(value) = crate::parse_json_line(&line) else {
            continue;
        };
        let role = match crate::json_str(&value, "type") {
            Some("user") => "user",
            Some("assistant") => "assistant",
            // attachment/system/queue-operation/etc. are skipped.
            _ => continue,
        };
        let content = value.get("message").and_then(|m| m.get("content"));
        let text = match content {
            // message.content is either a plain string...
            Some(serde_json::Value::String(s)) => s.clone(),
            // ...or a list of blocks; keep only type=="text" (thinking/tool skipped).
            Some(serde_json::Value::Array(blocks)) => {
                let mut text = String::new();
                for block in blocks {
                    if crate::json_str(block, "type") == Some("text")
                        && let Some(t) = crate::json_str(block, "text")
                    {
                        text.push_str(t);
                    }
                }
                text
            }
            _ => continue,
        };
        // A message whose content is only thinking/tool blocks extracts to "" — skip it
        // so peek never shows a blank role-only line.
        if text.is_empty() {
            continue;
        }
        items.push(TranscriptItem {
            role: role.to_string(),
            text,
        });
    }
    if items.len() > max_items {
        items = items.split_off(items.len() - max_items);
    }
    Ok(items)
}

/// Read turn timestamps from the full Claude transcript.
fn read_claude_turn_activity(
    path: &std::path::Path,
    window: std::time::Duration,
) -> Result<Vec<i64>> {
    use std::io::BufRead;

    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let (cutoff, now) = crate::activity_window(window);
    let mut timestamps = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line);
        let Some(value) = crate::parse_json_line(&line) else {
            continue;
        };
        let Some(record_type) = crate::json_str(&value, "type") else {
            continue;
        };
        if !matches!(record_type, "user" | "assistant") {
            continue;
        }
        let meaningful = match value
            .get("message")
            .and_then(|message| message.get("content"))
        {
            Some(serde_json::Value::String(text)) => !text.is_empty(),
            Some(serde_json::Value::Array(blocks)) => {
                blocks.iter().any(|block| {
                    crate::json_str(block, "type") == Some("text")
                        && crate::json_str(block, "text").is_some_and(|text| !text.is_empty())
                }) || (record_type == "assistant"
                    && blocks.iter().any(|block| {
                        crate::json_str(block, "type") == Some("tool_use")
                            && crate::json_str(block, "name").is_some_and(|name| !name.is_empty())
                    }))
            }
            _ => false,
        };
        if !meaningful {
            continue;
        }
        let Some(timestamp) = crate::json_str(&value, "timestamp")
            .and_then(crate::rfc3339_millis)
            .filter(|timestamp| (*timestamp >= cutoff) && (*timestamp <= now))
        else {
            continue;
        };
        timestamps.push(timestamp);
    }
    timestamps.sort_unstable();
    Ok(timestamps)
}

#[cfg(test)]
mod tests {
    use super::{ClaudeBackend, claude_rm_command, claude_spawn_command};
    use crate::backend::{BackendKind, Session, SessionOrigin, Status, args};
    use crate::error::Error;
    use crate::platform::Platform;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    #[test]
    fn portable_claude_remove_refuses_unresolved_session_before_cli() {
        let backend = ClaudeBackend::with_binary("/definitely/not/a/real/claude");
        let session = Session {
            backend: BackendKind::Claude,
            id: "session-uuid".to_string(),
            short_id: Some("abc12345".to_string()),
            origin: SessionOrigin::Background,
            title: "unsafe unresolved session".to_string(),
            cwd: PathBuf::from("/some/proj"),
            git_branch: None,
            status: Status::Unknown,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        };

        let error = backend
            .remove_for_platform(Platform::Windows, &session)
            .expect_err("portable remove must refuse before invoking the CLI");

        match error {
            Error::Unsupported(name) => assert_eq!(name, "claude"),
            other => panic!("expected Unsupported(\"claude\"), got {other:?}"),
        }
    }

    #[test]
    fn claude_rm_command_builds_rm_subcommand() {
        // The pure builder for `claude rm <short_id>` (mirrors claude_spawn_command) so the
        // exact argv is asserted without running claude. binary is a &Path here.
        let cmd = claude_rm_command(Path::new("claude"), "abc12345");
        assert_eq!(cmd.get_program(), OsStr::new("claude"));
        assert_eq!(args(&cmd), vec!["rm".to_string(), "abc12345".to_string()]);
    }

    #[test]
    fn claude_spawn_command_carries_model_flag() {
        // A non-default model is passed through as `--model <m>`.
        let cmd = claude_spawn_command("claude", Path::new("/tmp"), "do a thing", Some("fable"));
        assert_eq!(cmd.get_program(), OsStr::new("claude"));
        let a = args(&cmd);
        let i = a
            .iter()
            .position(|x| x == "--model")
            .expect("--model present");
        assert_eq!(a[i + 1], "fable");
        assert!(a.contains(&"do a thing".to_string())); // task still the final arg

        // None falls back to the opus[1m] default (never a missing --model).
        let cmd = claude_spawn_command("claude", Path::new("/tmp"), "t", None);
        let a = args(&cmd);
        let i = a
            .iter()
            .position(|x| x == "--model")
            .expect("--model present");
        assert_eq!(a[i + 1], "opus[1m]");
    }
}
