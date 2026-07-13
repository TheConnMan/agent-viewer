//! The actions the key handlers trigger: attach/spawn, reply delivery, rename, stop/remove,
//! hide, and the completion/model list refresh. Split out of `keys` so that module holds only
//! per-mode key routing. Every fn mutates the shared `Ui` state owned by the run loop.

use std::io;
use std::time::Instant;

use agent_viewer_core::backend::{Backend, BackendKind, Capabilities};
use agent_viewer_core::claude::ensure_trusted;
use agent_viewer_core::pty::{PtySession, spec_from_command};
use agent_viewer_core::spawn::now_ms;
use agent_viewer_core::Session;
use agent_viewer_tui::app::{
    CodexReply, DetachTracker, KillStage, codex_reply_keystroke, file_stems, reply_allowed,
    subdir_names,
};
use agent_viewer_tui::ui::{Mode, RenameModal, ReplyModal};

use crate::ops::{Mutation, run_mutation};
use crate::pending_reply::PendingReply;
use crate::{Key, Refresher, Ui};

/// Enter/Space on a header toggles + persists the collapse. Returns true when a header was
/// handled (so the caller skips attach/peek).
pub(crate) fn toggle_group_if_header(ui: &mut Ui) -> bool {
    let Some((key, collapsed)) = ui.app.toggle_selected_group() else {
        return false;
    };
    if let Some(db) = &ui.db {
        let _ = db.set_group_collapsed(&key.to_storage(), collapsed);
    }
    true
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

/// Keep the composer's discovered model list current: re-install from the backend's
/// `available_models()` only when the composer's backend has changed (mirrors
/// `ensure_completions`). Degrades to just "default" when the backend is absent.
pub(crate) fn ensure_models(ui: &mut Ui, backends: &[Box<dyn Backend>]) {
    let backend = ui.composer.backend();
    if ui.composer.models_key() != Some(backend) {
        let models = backend_of(backends, backend)
            .map(|b| b.available_models())
            .unwrap_or_else(|| vec!["default".to_string()]);
        ui.composer.set_models(models, backend);
    }
}

/// Open the rename modal for the selected session (claude falls back to the local
/// name override on apply, so it opens regardless of the backend's rename capability).
pub(crate) fn open_rename(ui: &mut Ui) {
    let Some(session) = ui.app.selected() else {
        return;
    };
    ui.mode = Mode::Rename(RenameModal {
        backend: session.backend,
        id: session.id.clone(),
        buffer: session.title.clone(),
    });
}

/// `Ctrl+E` — focus a reply input for the selected session, gated on the
/// backend supporting reply AND the session actually being blocked (the sole safety gate).
/// Force the peek open so the pending ask stays visible above the input.
pub(crate) fn open_reply(backends: &[Box<dyn Backend>], ui: &mut Ui) {
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let caps = caps_of(backends, session.backend);
    if !reply_allowed(caps.reply, session.status) {
        ui.set_notice(if !caps.reply {
            format!("{} does not support reply", session.backend.name())
        } else {
            "only a needs-input session can be replied to".to_string()
        });
        return;
    }
    ui.app.expand_selected();
    ui.mode = Mode::Reply(ReplyModal {
        backend: session.backend,
        id: session.id.clone(),
        buffer: String::new(),
    });
}

/// Deliver the composed reply: re-resolve the target, re-check the safety gate (state may
/// have changed while typing), attach (native `claude attach`, or a reused live PTY), then arm
/// the one-shot injector to write the payload once we are safely in the run. Claude sends the
/// text + Enter; codex maps y/n approvals to a single keystroke and otherwise attaches with
/// focus for the user to finish; opencode is gated out upstream.
pub(crate) fn send_reply(
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<()> {
    let Mode::Reply(modal) = &ui.mode else {
        return Ok(());
    };
    let backend_kind = modal.backend;
    let id = modal.id.clone();
    let buffer = modal.buffer.clone();

    // Re-resolve by (backend, id), NOT selected() — the background refresh reorders rows.
    let Some(session) = ui.app.session_for(&(backend_kind, id.clone())).cloned() else {
        ui.set_notice("session is gone".to_string());
        return Ok(());
    };
    // Safety re-check: never send to a session that is no longer waiting for input.
    if !reply_allowed(caps_of(backends, backend_kind).reply, session.status) {
        ui.set_notice("session is no longer waiting for input".to_string());
        return Ok(());
    }

    // Decide the payload bytes per backend; None attaches with focus only (the run-view gate
    // is derived from the backend/attach kind after attach, below).
    let payload: Option<Vec<u8>> = match backend_kind {
        BackendKind::Claude => Some(format!("{buffer}\r").into_bytes()),
        BackendKind::Codex => match codex_reply_keystroke(&buffer) {
            // Approve auto-sends the stable `y` approve key. Deny is deliberately NOT
            // auto-injected: the reject key is Codex-version/config specific (0.144.1 binds
            // deny to `d`, and `n` is a different decline-with-guidance action), so a guessed
            // byte could invoke the wrong action. Attach with focus and let the user confirm
            // the denial by hand rather than risk it.
            CodexReply::Approve => Some(b"y".to_vec()),
            CodexReply::Deny => {
                ui.set_notice("confirm the denial in the attached session".to_string());
                None
            }
            CodexReply::Freeform => {
                ui.set_notice("type your reply in the attached session".to_string());
                None
            }
        },
        BackendKind::Opencode => None,
    };

    // Whether a live PTY already exists BEFORE we attach decides fresh-vs-reused for the
    // reply gate: attach_session reuses an existing PTY (already in the run) and otherwise
    // spawns a fresh `claude attach` (which must show its transient before we may type).
    let key: Key = (backend_kind, id);
    let reused = ui.attached.contains_key(&key);
    if !attach_session(backends, ui, terminal, &session)? {
        return Ok(());
    }
    if let Some(payload) = payload {
        // require_run_view: only claude prints a "Waking…/Attaching…" transient before its
        // prompt renders, so the injector must wait for that to clear before typing. Codex
        // resume lands straight in the pending prompt (always ready).
        let require_run_view = matches!(backend_kind, BackendKind::Claude);
        ui.pending_reply = Some(PendingReply {
            key,
            payload,
            require_run_view,
            fresh_attach: !reused,
            transient_seen: false,
            armed_at: Instant::now(),
            ready_since: None,
        });
    }
    Ok(())
}

/// Submit the rename to the background runner (the app-server/UDS rename can take 1-2s).
pub(crate) fn apply_rename(ui: &mut Ui) {
    let Mode::Rename(modal) = &ui.mode else {
        return;
    };
    let backend_kind = modal.backend;
    let id = modal.id.clone();
    let name = modal.buffer.clone();
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
    let caps = caps_of(backends, session.backend);
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
            if !caps.remove {
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
            if !caps.stop {
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
    if !caps.hide {
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

pub(crate) fn attach_selected(
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<()> {
    let Some(session) = ui.app.selected().cloned() else {
        return Ok(());
    };
    attach_session(backends, ui, terminal, &session)?;
    Ok(())
}

/// Attach a GIVEN session (shared by `attach_selected` and the reply delivery path): reuse a
/// live PTY (resize) or spawn one, and focus it. Returns true when it ended attached
/// (Mode::Attached), false when it bailed with a notice.
fn attach_session(
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
    session: &Session,
) -> io::Result<bool> {
    let Some(backend) = backend_of(backends, session.backend) else {
        return Ok(false);
    };
    if !backend.capabilities().attach {
        ui.set_notice(format!("{} does not support attach", backend.kind().name()));
        return Ok(false);
    }

    let key: Key = (session.backend, session.id.clone());
    let size = terminal.size()?;
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
        // prompt — `claude attach <short_id>` resolves the trusted jobs cwd itself, and other
        // backends never need it).
        let claude_fallback = session.backend == BackendKind::Claude
            && session.short_id.as_deref().unwrap_or_default().is_empty();
        if claude_fallback {
            let home = std::env::var("HOME").unwrap_or_default();
            let config = std::path::PathBuf::from(&home).join(".claude.json");
            let _ = ensure_trusted(&config, &session.cwd);
        }
        let Some(command) = backend.attach_command(session) else {
            ui.set_notice(format!("{} does not support attach", backend.kind().name()));
            return Ok(false);
        };
        let spec = spec_from_command(&command, rows, cols);
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
    match backend.spawn(&target, &task, model) {
        Ok(Some(pid)) => {
            // Record the spawn so the overlay can pin (and later stop) the session.
            if let Some(db) = &ui.db {
                let _ = db.record_spawn(backend_kind, &target, pid, now_ms());
            }
            ui.set_notice(notice);
        }
        Ok(None) => ui.set_notice(notice),
        Err(e) => {
            // Keep the composer text so the user can retry.
            ui.set_notice(format!("spawn failed: {e}"));
            return;
        }
    }
    ui.composer.clear();
    // Hasten the next listing so the spawned row (and its bloom) appears promptly; the
    // notice survives until the 1s clear cadence since apply_snapshot preserves it.
    refresher.force();
}
