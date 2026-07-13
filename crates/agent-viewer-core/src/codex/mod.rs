pub mod cli;
pub mod registry;
pub mod rollout;
pub mod source;
pub mod status;

use crate::backend::{Backend, BackendKind, Capabilities, Session};
use crate::error::Result;
use registry::Registry;
use status::StatusResolver;
use std::path::Path;

// NOTE: the experimental `codex app-server` JSON-RPC daemon (`thread/subscribe`,
// `thread/list`, `command/exec/terminate`) is the eventual "clean" backend and the v2
// upgrade path. v1 reads the same SQLite + rollout files directly.

/// Sandbox args passed to `codex exec` for viewer-spawned sessions. Least-privileged
/// choice (verified working unattended on this box). If a workspace-write run ever fails
/// to complete unattended, switch to ["--dangerously-bypass-approvals-and-sandbox"].
const SANDBOX_ARGS: &[&str] = &["--sandbox", "workspace-write"];

/// Build the `codex exec` spawn command (extracted so the model-flag wiring is unit-
/// testable). `model` Some adds `-m <m>`; None uses codex's own default.
fn codex_spawn_command(dir: &Path, task: &str, model: Option<&str>) -> std::process::Command {
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("exec").arg("--json").arg("-C").arg(dir);
    cmd.args(SANDBOX_ARGS);
    if let Some(model) = model {
        cmd.arg("-m").arg(model);
    }
    cmd.arg(task);
    cmd
}

/// Run `codex debug models` and parse its catalog. Any failure (spawn error, non-zero
/// exit, unparseable stdout) is a quiet empty Vec — discovery is best-effort.
fn codex_catalog_via_cli() -> Vec<String> {
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("debug").arg("models");
    match crate::spawn::run_with_timeout(cmd, std::time::Duration::from_secs(3)) {
        Some(stdout) => parse_codex_catalog(&stdout),
        None => Vec::new(),
    }
}

/// PURE parse of `codex debug models` JSON stdout. Keep only `visibility == "list"`
/// entries, sort by `priority` ascending (a missing priority sorts last), and return each
/// entry's `slug`. Malformed/missing JSON -> empty Vec (never panics).
pub fn parse_codex_catalog(json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(models) = value.get("models").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    // (priority, slug) with missing priority forced last; stable sort keeps input order
    // among equal priorities.
    let mut listed: Vec<(i64, String)> = models
        .iter()
        .filter(|m| crate::json_str(m, "visibility") == Some("list"))
        .filter_map(|m| {
            let slug = crate::json_str(m, "slug")?.to_string();
            let priority = m
                .get("priority")
                .and_then(|p| p.as_i64())
                .unwrap_or(i64::MAX);
            Some((priority, slug))
        })
        .collect();
    listed.sort_by_key(|(priority, _)| *priority);
    listed.into_iter().map(|(_, slug)| slug).collect()
}

/// Best-effort fallback: the distinct models the codex registry has seen, most-used first.
/// Opens a throwaway read-only connection (this runs on `&self`, so it cannot touch
/// `self.registry`). Any error -> empty Vec.
fn codex_models_via_registry(home: &Path) -> Vec<String> {
    let Ok(db) = registry::find_state_db(home) else {
        return Vec::new();
    };
    let Ok(reg) = Registry::open(&db) else {
        return Vec::new();
    };
    reg.distinct_models().unwrap_or_default()
}

pub struct CodexBackend {
    codex_home: std::path::PathBuf,
    registry: Option<Registry>,
    resolver: StatusResolver,
    /// Discovered model catalog, computed once and reused (best-effort; degrades to the
    /// default when both the CLI probe and the registry fallback come up empty).
    models_cache: std::sync::OnceLock<Vec<String>>,
}

impl CodexBackend {
    pub fn new(codex_home: std::path::PathBuf) -> CodexBackend {
        CodexBackend {
            codex_home,
            registry: None,
            resolver: StatusResolver::new(),
            models_cache: std::sync::OnceLock::new(),
        }
    }

    /// Query threads, reopening the DB once on failure (state_N rollover, transient error).
    fn query_threads(&mut self) -> Result<Vec<registry::Thread>> {
        if self.registry.is_none() {
            let db = registry::find_state_db(&self.codex_home)?;
            self.registry = Some(Registry::open(&db)?);
        }
        match self.registry.as_ref().unwrap().threads() {
            Ok(threads) => Ok(threads),
            Err(_) => {
                self.registry = None;
                let db = registry::find_state_db(&self.codex_home)?;
                let reg = Registry::open(&db)?;
                let threads = reg.threads()?;
                self.registry = Some(reg);
                Ok(threads)
            }
        }
    }
}

impl Backend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Codex
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            spawn: true,
            hide: true,
            attach: true,
            stop: true,
            remove: true,
            rename: true,
            reply: true,
        }
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        let threads = self.query_threads()?;
        let open = status::open_rollout_paths();
        let mut sessions = Vec::with_capacity(threads.len());
        for thread in threads {
            // The resolver canonicalizes once (cached) and returns the owning pid from the
            // same open map, so no per-tick canonicalize is needed here.
            let (status, pid) = self.resolver.resolve(&thread.rollout_path, &open);
            let companion = thread.source.is_companion();
            let source_label = thread.source.label().to_string();
            sessions.push(Session {
                backend: BackendKind::Codex,
                id: thread.id,
                short_id: None,
                title: thread.title,
                cwd: thread.cwd,
                created_at_ms: thread.created_at_ms,
                updated_at_ms: thread.updated_at_ms,
                status,
                hidden: thread.archived,
                source_label,
                summary: thread.preview,
                companion,
                pid,
                rollout_path: Some(thread.rollout_path),
                pr_refs: Vec::new(),
            });
        }
        Ok(sessions)
    }

    fn available_models(&self) -> Vec<String> {
        // default first, then the CLI catalog, falling back to the registry's used models.
        self.models_cache
            .get_or_init(|| {
                let mut discovered = codex_catalog_via_cli();
                if discovered.is_empty() {
                    discovered = codex_models_via_registry(&self.codex_home);
                }
                let mut models = vec!["default".to_string()];
                models.extend(discovered);
                crate::backend::dedup_preserve(models)
            })
            .clone()
    }

    fn spawn(&self, dir: &Path, task: &str, model: Option<&str>) -> Result<Option<u32>> {
        let cmd = codex_spawn_command(dir, task, model);
        let log_path = crate::default_codex_home()
            .join("bg-logs")
            .join(format!("{}.log", crate::spawn::now_ms()));
        let pid = crate::spawn::spawn_detached(cmd, &log_path)?;
        Ok(Some(pid))
    }

    fn hide(&self, id: &str) -> Result<()> {
        cli::archive(id)
    }

    fn unhide(&self, id: &str) -> Result<()> {
        cli::unarchive(id)
    }

    fn stop(&self, session: &Session) -> Result<()> {
        match session.pid {
            Some(pid) => crate::spawn::terminate(pid, "codex"),
            None => Err(crate::error::Error::Unsupported(self.kind().name())),
        }
    }

    fn remove(&self, session: &Session) -> Result<()> {
        cli::archive(&session.id)
    }

    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        cli::rename(&session.id, name)
    }

    fn attach_command(&self, session: &Session) -> Option<std::process::Command> {
        let mut cmd = cli::resume_command(&session.id);
        // `codex resume` inherits the viewer's cwd and otherwise prompts "Choose working
        // directory" on attach. Pin it to the session's own cwd when that directory still
        // exists; leave it unset when the dir was deleted so the spawn cannot fail on it.
        if session.cwd.is_dir() {
            cmd.current_dir(&session.cwd);
        }
        Some(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::codex_spawn_command;
    use crate::backend::args;
    use std::path::Path;

    #[test]
    fn codex_spawn_command_carries_model_flag() {
        // A non-default model becomes `-m <model>`, before the task arg.
        let cmd = codex_spawn_command(Path::new("/tmp"), "do a thing", Some("gpt-5.3-codex"));
        let a = args(&cmd);
        let i = a.iter().position(|x| x == "-m").expect("-m present");
        assert_eq!(a[i + 1], "gpt-5.3-codex");
        assert_eq!(a.last().map(String::as_str), Some("do a thing"));

        // None (the "default" model) adds no -m flag.
        let cmd = codex_spawn_command(Path::new("/tmp"), "t", None);
        assert!(!args(&cmd).iter().any(|x| x == "-m"));
    }
}
