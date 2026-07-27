//! The actions the key handlers trigger: attach/spawn, reply delivery, rename, stop/remove,
//! hide, and the completion/model list refresh. Split out of `keys` so that module holds only
//! per-mode key routing. Every fn mutates the shared `Ui` state owned by the run loop.

use std::io;

use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Status};
use agent_viewer_core::claude::ensure_trusted;
use agent_viewer_core::pty::{PtySession, spec_from_command};
use agent_viewer_core::spawn::now_ms;
use agent_viewer_core::{AttachRefusal, Session};
use agent_viewer_tui::app::{DetachTracker, KillStage, file_stems, subdir_names};
use agent_viewer_tui::ui::{Mode, RenameModal};

use crate::ops::{Mutation, run_mutation};
use crate::{Key, Refresher, Ui};

/// Enter/Space on a header toggles and persists the collapse. Returns true when a header was
/// handled so the caller skips attach.
pub(crate) fn toggle_group_if_header(ui: &mut Ui) -> bool {
    let Some((key, collapsed)) = ui.app.toggle_selected_group() else {
        return false;
    };
    if let Some(db) = &ui.db {
        let _ = db.set_group_collapsed(&key.to_storage(), collapsed);
    }
    true
}

pub(crate) fn activate_selected<B: ratatui::backend::Backend>(
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
) -> io::Result<()> {
    if !toggle_group_if_header(ui) {
        attach_selected(backends, ui, terminal)?;
    }
    Ok(())
}

/// Ctrl+F — enter filter mode with a fresh, empty query.
pub(crate) fn open_filter(ui: &mut Ui) {
    ui.app.set_filter(String::new());
    ui.notice.clear();
    ui.mode = Mode::Filter;
}

/// The slash-command names for a backend (scanned from disk; missing dir -> empty, no error).
/// claude: skill dir names under ~/.claude/skills plus <target>/.claude/skills (project
/// skills). opencode: file stems under ~/.config/opencode/command. codex: file stems under
/// ~/.codex/prompts. All home paths go through core's `home_dir`.
fn scan_commands(backend: BackendKind, target: Option<&std::path::Path>) -> Vec<String> {
    let home = agent_viewer_core::home_dir();
    let mut cmds = match backend {
        BackendKind::Claude => {
            let mut v = subdir_names(&home.join(".claude/skills"));
            if let Some(t) = target {
                v.extend(subdir_names(&t.join(".claude/skills")));
            }
            v
        }
        BackendKind::Opencode => file_stems(&home.join(".config/opencode/command")),
        BackendKind::Codex => file_stems(&home.join(".codex/prompts")),
    };
    cmds.sort();
    cmds.dedup();
    cmds
}

/// Keep the composer's slash-command list current: re-scan the filesystem only when the
/// text is a "/…" command AND the (backend, spawn target) it was scanned for has changed.
pub(crate) fn ensure_completions(ui: &mut Ui) {
    if !ui.composer.text().starts_with('/') {
        return;
    }
    let target = ui.app.spawn_target();
    let key = (ui.composer.backend(), target.clone());
    if ui.composer.commands_key() != Some(&key) {
        let cmds = scan_commands(key.0, target.as_deref());
        ui.composer.set_commands(cmds, key);
    }
}

/// Keep the composer's discovered model list current: re-install from the model cache only
/// when the composer's backend has changed (mirrors `ensure_completions`). Discovery itself
/// never runs here: `request` hands it to a worker thread, because the CLI probe behind it
/// takes seconds and this runs on the key path. Until a list exists the picker holds just the
/// backend's default; `install_models` swaps the real one in when the probe lands.
pub(crate) fn ensure_models(ui: &mut Ui) {
    let backend = ui.composer.backend();
    ui.models.request(backend);
    if ui.composer.models_key() != Some(backend) {
        let models = ui
            .models
            .models(backend)
            .map(|m| m.to_vec())
            .unwrap_or_else(|| vec![backend.default_model().to_string()]);
        ui.composer.set_models(models, backend);
    }
}

/// Drain landed model probes: persist each discovered catalog for the next viewer run, and
/// install it into the composer when it is the backend the composer is currently on.
pub(crate) fn install_models(ui: &mut Ui) {
    for (backend, models) in ui.models.poll() {
        if let Some(db) = &ui.db {
            let _ = db.set_cached_models(backend, &models, now_ms());
        }
        if ui.composer.backend() == backend {
            ui.composer.set_models(models, backend);
        }
    }
}

/// `Ctrl+R` — open the rename modal for the selected session, gated PER ROW on rename: claude
/// renames a bg job by writing its job dir's state.json, so an interactive row (which has no
/// job dir) is a footer notice even though the backend itself advertises rename.
pub(crate) fn open_rename(backends: &[Box<dyn Backend>], ui: &mut Ui) {
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let caps = backend_of(backends, session.backend)
        .map(|backend| backend.capabilities_for(&session))
        .unwrap_or_else(Capabilities::none);
    if !caps.rename {
        ui.set_notice(format!(
            "{} does not support rename",
            session.backend.name()
        ));
        return;
    }
    // DELIBERATE DIVERGENCE from Fleet View, which prefills its Ctrl+R field with the current
    // name (`J2(Uf(fu.state.name ?? ""))`). Renaming here always means typing a new name from
    // scratch, so a prefill is only text to clear first. Enter on a blank buffer therefore
    // cancels rather than renaming (see `apply_rename`).
    ui.mode = Mode::Rename(RenameModal {
        backend: session.backend,
        id: session.id.clone(),
        buffer: String::new(),
    });
}

/// Reply is not supported by any backend.
pub(crate) fn open_reply(_backends: &[Box<dyn Backend>], ui: &mut Ui) {
    if ui.app.selected().is_none() {
        return;
    }
    ui.set_notice("reply is not supported".to_string());
}

/// Reply is not supported by any backend.
pub(crate) fn send_reply(
    _backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    _terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<()> {
    if !matches!(ui.mode, Mode::Reply(_)) {
        return Ok(());
    }
    ui.set_notice("reply is not supported".to_string());
    Ok(())
}

/// Submit the rename to the background runner (the app-server/UDS rename can take 1-2s).
pub(crate) fn apply_rename(ui: &mut Ui) {
    let Mode::Rename(modal) = &ui.mode else {
        return;
    };
    let backend_kind = modal.backend;
    let id = modal.id.clone();
    let name = modal.buffer.trim().to_string();
    // The field opens blank, so a bare Enter is an easy slip; an empty name is never a rename
    // any backend should be asked to perform. Cancel silently, exactly as Fleet View does.
    if name.is_empty() {
        return;
    }
    // Resolve the target by (backend, id), NOT by selected() — the background refresh
    // reorders rows while the user types, so selection may have drifted off the rename row
    // (which would silently no-op the rename).
    let Some(session) = ui.app.session_for(&(backend_kind, id.clone())).cloned() else {
        return;
    };
    let key = format!("{}:{}:rename", backend_kind.name(), id);
    let mutation = Mutation::Rename(session, name.clone());
    if ui.mutations.submit(key, move || run_mutation(mutation)) {
        ui.set_notice(format!("renaming… {name}"));
    }
}

pub(crate) fn kill_selected(backends: &[Box<dyn Backend>], ui: &mut Ui) {
    let now = now_ms();
    let stage = ui.app.kill_stage(now);
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let caps = backend_of(backends, session.backend)
        .map(|backend| backend.capabilities_for(&session))
        .unwrap_or_else(Capabilities::none);
    match stage {
        KillStage::Stop => {
            if !caps.stop {
                ui.set_notice(format!("{} does not support stop", session.backend.name()));
                return;
            }
            submit_mutation(
                ui,
                &session,
                "stop",
                "stopping",
                Mutation::Stop(session.clone()),
            );
        }
        KillStage::Remove => {
            // Per-row, not just per-backend: claude advertises remove but can only act on a
            // row carrying a short id. Asking the row keeps the notice honest at keypress.
            let removable = caps.delete;
            if !removable {
                ui.set_notice(format!(
                    "{} does not support remove",
                    session.backend.name()
                ));
                return;
            }
            submit_mutation(
                ui,
                &session,
                "remove",
                "removing",
                Mutation::Remove(session.clone()),
            );
        }
        KillStage::Noop => {
            // Noop is the FIRST press on a row a stop does not apply to: it arms the remove
            // and the footer countdown hint is the whole signal, so arming stays silent.
            // A refusal notice only belongs to a row that IS running yet cannot be stopped;
            // gating it on the status keeps `caps.stop` being per-row (codex sets it from
            // `pid`) from refusing an action the user never asked for.
            let running = matches!(session.status, Status::Working | Status::NeedsInput { .. });
            if running && !caps.stop {
                ui.set_notice(format!("{} cannot be stopped", session.backend.name()));
            }
        }
    }
}

pub(crate) fn hide_selected(backends: &[Box<dyn Backend>], ui: &mut Ui, hide: bool) {
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let caps = caps_of(backends, session.backend);
    if !caps.archive {
        ui.set_notice(format!("{} does not support hide", session.backend.name()));
        return;
    }
    if hide {
        submit_mutation(
            ui,
            &session,
            "hide",
            "archiving",
            Mutation::Hide(session.clone()),
        );
    } else {
        submit_mutation(
            ui,
            &session,
            "unhide",
            "unarchiving",
            Mutation::Unhide(session.clone()),
        );
    }
}

/// Route a blocking mutation to the runner with a backend+id+op dedup key and an
/// immediate "<verb>… <title>" notice (a duplicate keypress while pending is a no-op).
fn submit_mutation(ui: &mut Ui, session: &Session, op: &str, verb: &str, mutation: Mutation) {
    let key = format!("{}:{}:{}", session.backend.name(), session.id, op);
    if ui.mutations.submit(key, move || run_mutation(mutation)) {
        ui.set_notice(format!("{verb}… {}", session.title));
    }
}

/// The live backend instance for a kind, if present in the slice.
fn backend_of(backends: &[Box<dyn Backend>], kind: BackendKind) -> Option<&dyn Backend> {
    backends
        .iter()
        .find(|b| b.kind() == kind)
        .map(|b| b.as_ref())
}

/// Capabilities for a backend kind from the live slice (falls back to none if absent).
fn caps_of(backends: &[Box<dyn Backend>], kind: BackendKind) -> Capabilities {
    backend_of(backends, kind)
        .map(|b| b.capabilities())
        .unwrap_or_else(Capabilities::none)
}

pub(crate) fn attach_selected<B: ratatui::backend::Backend>(
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
) -> io::Result<()> {
    let Some(session) = ui.app.selected().cloned() else {
        return Ok(());
    };
    attach_session(backends, ui, terminal, &session)?;
    Ok(())
}

/// What a refused attach does to the UI.
///
/// A `tail` refusal means the session IS running and watchable but cannot be JOINED: a
/// `codex exec` thread hosts its app-server in process, so nothing outside it can subscribe to
/// the live turn, the ChatGPT app included. A plain resume of a live thread must never happen
/// because it forks the thread and appends a synthesized interrupt to its rollout.
fn apply_attach_refusal(ui: &mut Ui, refusal: AttachRefusal) {
    ui.set_notice(refusal.reason);
}

/// Attach a GIVEN session (shared by `attach_selected` and the reply delivery path): reuse a
/// live PTY (resize) or spawn one, and focus it. Returns true when it ended attached
/// (Mode::Attached), false when it bailed with a notice.
fn attach_session<B: ratatui::backend::Backend>(
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
    session: &Session,
) -> io::Result<bool> {
    let Some(backend) = backend_of(backends, session.backend) else {
        return Ok(false);
    };
    if !backend.capabilities_for(session).attach {
        ui.set_notice(format!("{} does not support attach", backend.kind().name()));
        return Ok(false);
    }

    let key: Key = (session.backend, session.id.clone());
    let size = terminal
        .size()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let rows = size.height.saturating_sub(1).max(1);
    let cols = size.width.max(1);

    if let Some(pty) = ui.attached.get_mut(&key) {
        // Re-attach: reuse the live PTY, resizing it to the current content area. The
        // per-PTY detach tracker is preserved so a half-typed input line still gates Left.
        let _ = pty.resize(rows, cols);
        ui.detach_trackers.entry(key.clone()).or_default();
    } else {
        // Pre-accept the trust dialog before a claude `-r` RESUME attach into a fresh project
        // (best-effort; only the no-short-id fallback resumes by full id and can hit the trust
        // prompt, since `claude attach <short_id>` resolves the trusted jobs cwd itself and
        // other backends never need it).
        let claude_fallback = session.backend == BackendKind::Claude
            && session.short_id.as_deref().unwrap_or_default().is_empty();
        if claude_fallback {
            let home = std::env::var("HOME").unwrap_or_default();
            let config = std::path::PathBuf::from(&home).join(".claude.json");
            let _ = ensure_trusted(&config, &session.cwd);
        }
        let command = match backend.attach_command(session) {
            Ok(command) => command,
            Err(refusal) => {
                apply_attach_refusal(ui, refusal);
                return Ok(false);
            }
        };
        let mut spec = spec_from_command(&command, rows, cols);
        spec.palette = ui.terminal_palette;
        match PtySession::spawn(spec) {
            Ok(pty) => {
                ui.attached.insert(key.clone(), pty);
                // Fresh Left-gate: a brand-new PTY starts with an empty input line.
                ui.detach_trackers.insert(key.clone(), DetachTracker::new());
            }
            Err(e) => {
                ui.set_notice(format!("attach failed: {e}"));
                return Ok(false);
            }
        }
    }

    ui.focused = Some(key);
    ui.focused_session = Some(session.clone());
    ui.mode = Mode::Attached;
    Ok(true)
}

/// Spawn the composed task into the current spawn target, record it for pinning, and
/// clear the composer. The spawn itself is detached (fast); only its record persists.
pub(crate) fn spawn_from_composer(
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
) {
    // Defense-in-depth: never spawn the /model meta-command as a task (Enter routing already
    // avoids this, but keep the spawn path safe).
    if ui.composer.is_model_command() {
        return;
    }
    let Some(target) = ui.app.spawn_target() else {
        ui.set_notice("no target directory".to_string());
        return;
    };
    let backend_kind = ui.composer.backend();
    let Some(backend) = backend_of(backends, backend_kind) else {
        return;
    };
    if !backend.capabilities().spawn {
        ui.set_notice(format!("{} does not support spawn", backend_kind.name()));
        return;
    }
    let task = ui.composer.text().to_string();
    // "default" (codex/opencode) passes no model flag; any other value is a real model.
    let model_str = ui.composer.model();
    let model = (model_str != "default").then_some(model_str);
    let notice = match model {
        Some(m) => format!("spawned on {} {m}", backend_kind.name()),
        None => format!("spawned on {}", backend_kind.name()),
    };
    // On the mutation runner, not here. A codex spawn now dials the app-server daemon (and may
    // start one), so running it inline froze the composer for as long as that took. The dedup
    // key is backend+task, so a double Enter cannot spawn the same task twice while the first
    // is still in flight, while a different task still goes straight through.
    let key = format!("{}:{}:spawn", backend_kind.name(), task);
    let mutation = Mutation::spawn(
        &ui.app,
        backend_kind,
        target,
        task.clone(),
        model.map(str::to_string),
        now_ms(),
        notice,
    );
    let executor = ui.mutation_executor.clone();
    if !ui.mutations.submit(key, move || executor(mutation)) {
        return;
    }
    ui.set_notice(format!("spawning… on {}", backend_kind.name()));
    ui.composer.clear();
    // Hasten the next listing so the spawned row (and its bloom) appears promptly; the
    // notice survives until the 1s clear cadence since apply_snapshot preserves it.
    refresher.force();
}

#[cfg(test)]
mod tests {
    use super::{apply_attach_refusal, spawn_from_composer};
    use crate::Refresher;
    use crate::keys::handle_paste;
    use crate::keys::tests::{sess, test_ui_with};
    use crate::ops::Mutation;
    use agent_viewer_core::{AttachRefusal, BackendKind, Capabilities, Session};
    use agent_viewer_tui::mutations::MutationOutcome;
    use std::sync::{Arc, Mutex, mpsc::channel};
    use std::time::{Duration, Instant};

    struct SpawnCapableBackend;

    impl agent_viewer_core::Backend for SpawnCapableBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                spawn: true,
                ..Capabilities::none()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            unreachable!("listing is not exercised by composer submission")
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("the external mutation executor must intercept spawn")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, AttachRefusal> {
            unreachable!("attach is not exercised by composer submission")
        }
    }

    fn inert_refresher() -> Refresher {
        let (_snapshot_tx, snapshots) = channel();
        let (wake, _wake_rx) = channel();
        Refresher { snapshots, wake }
    }

    #[test]
    fn multiline_paste_submits_once_only_when_the_composer_action_runs() {
        let payload = "first line\nsecond line\nthird line";
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let mut ui = test_ui_with(vec![sess(
            "spawn_target",
            "/tmp/agentviewer_composer_submit",
            100,
        )]);
        ui.mutation_executor = Arc::new(move |mutation| {
            match mutation {
                Mutation::Spawn { task, .. } => recorded.lock().unwrap().push(task),
                _ => panic!("composer submission must only execute spawn"),
            }
            Ok(MutationOutcome {
                notice: "recorded".to_string(),
                spawned: None,
            })
        });

        handle_paste(payload, &mut ui);

        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(ui.composer.text(), payload);

        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(SpawnCapableBackend)];
        spawn_from_composer(&backends, &inert_refresher(), &mut ui);

        assert_eq!(ui.composer.text(), "");
        let deadline = Instant::now() + Duration::from_secs(1);
        while ui.mutations.poll().is_none() {
            assert!(
                Instant::now() < deadline,
                "recording mutation executor did not finish"
            );
            std::thread::yield_now();
        }
        assert_eq!(*calls.lock().unwrap(), vec![payload.to_string()]);
        assert!(ui.mutations.poll().is_none());
    }

    #[test]
    fn a_tailable_refusal_stays_a_notice() {
        // A live `codex exec` thread cannot be joined because its app server runs in process.
        // The refusal reason must remain visible without attempting a plain resume.
        let mut ui = test_ui_with(Vec::new());

        apply_attach_refusal(&mut ui, AttachRefusal::tailable("cannot be joined"));

        assert_eq!(ui.notice.text, "cannot be joined");
    }

    #[test]
    fn a_plain_refusal_stays_a_notice() {
        let mut ui = test_ui_with(Vec::new());

        apply_attach_refusal(&mut ui, AttachRefusal::new("no attach here"));

        assert_eq!(ui.notice.text, "no attach here");
    }
}
