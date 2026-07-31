//! The actions the key handlers trigger: attach/spawn, reply delivery, rename, stop/remove,
//! hide, and the completion/model list refresh. Split out of `keys` so that module holds only
//! per-mode key routing. Every fn mutates the shared `Ui` state owned by the run loop.

use std::io;

use agent_viewer_core::backend::{Backend, BackendKind};
use agent_viewer_core::pty::{PtySession, VIEWPORT_SCROLLBACK_ROWS, spec_from_command};
use agent_viewer_core::spawn::now_ms;
use agent_viewer_tui::app::{DetachTracker, KillStage, file_stems, subdir_names};
use agent_viewer_tui::shared_listing::{SpawnTarget, TargetRequest};
use agent_viewer_tui::ui::{ATTACHED_CHROME_ROWS, Mode, RenameModal, TriageState, triage_queue};
use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::keys::set_mouse_capture;
use crate::ops::{AttachPlan, Mutation};
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

pub(crate) fn activate_selected(ui: &mut Ui) {
    if !toggle_group_if_header(ui) {
        attach_selected(ui);
    }
}

/// Ctrl+W — toggle the video wall. The wall is a flag on the list view, not a mode, so key
/// routing never leaves `Mode::Normal` and every already-bound chord keeps its meaning.
pub(crate) fn toggle_wall(ui: &mut Ui) {
    if ui.wall.on {
        // Closing the wall closes every connection it opened: nothing stays connected to a
        // session that is not on screen.
        ui.wall.on = false;
        crate::close_wall(ui);
        ui.set_notice("video wall off".to_string());
        return;
    }
    ui.wall.on = true;
    ui.wall.clear();
    let keys = agent_viewer_tui::ui::wall::tile_keys(&ui.app, now_ms());
    match keys.first() {
        Some(key) => {
            ui.wall.set_focus(&keys, 0);
            ui.app.select_by_key(key);
            let plural = if keys.len() == 1 { "" } else { "s" };
            ui.set_notice(format!(
                "video wall: connecting {} session{plural} · type into the focused tile · Ctrl+W to exit",
                keys.len()
            ));
        }
        None => ui.set_notice("video wall: nothing is running right now".to_string()),
    }
}

/// Focus one tile outright, by index. The mouse path: hover or click puts the keyboard on
/// whatever is under the pointer, so typing lands where you are looking.
pub(crate) fn focus_wall_tile(ui: &mut Ui, index: usize) {
    let keys = agent_viewer_tui::ui::wall::tile_keys(&ui.app, now_ms());
    let Some(key) = keys.get(index).cloned() else {
        return;
    };
    ui.wall.set_focus(&keys, index);
    ui.app.select_by_key(&key);
}

/// Rows one wheel tick moves a tile's viewport, matching the attached view's feel.
const WALL_WHEEL_ROWS: usize = 3;

/// Scroll one tile, by whichever mechanism its child actually honours.
///
/// Measured on this box: `claude attach` runs in the alternate screen and requests SGR mouse
/// tracking, so it owns its own scrollback and the only way to scroll it is to hand it a
/// wheel report — a local viewport scroll finds an empty normal grid and does nothing. Codex
/// is the mirror image: it discards native wheel reports, which is why the attach view scrolls
/// it locally. So the wheel is forwarded when the child is tracking the mouse, and falls back
/// to moving the viewer's own viewport when it is not.
///
/// The report has to be translated into the child's coordinate space first; it thinks it is
/// alone on a terminal the size of the tile's content area, not offset into a grid.
pub(crate) fn scroll_wall_tile(ui: &mut Ui, index: usize, event: MouseEvent, content: Rect) {
    let keys = agent_viewer_tui::ui::wall::tile_keys(&ui.app, now_ms());
    let Some(key) = keys.get(index).cloned() else {
        return;
    };
    let Some(pty) = ui.attached.get_mut(&key) else {
        return;
    };
    let (mode, encoding) = pty.with_screen(|screen| {
        (
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
        )
    });
    let local = MouseEvent {
        column: event.column.saturating_sub(content.x),
        row: event.row.saturating_sub(content.y),
        ..event
    };
    if let Some(bytes) = agent_viewer_tui::mouse::encode_mouse_report(local, mode, encoding, 0) {
        let _ = pty.write_input(&bytes);
        return;
    }
    // The child is not tracking the mouse, so scroll our own viewport over its retained rows.
    if matches!(event.kind, MouseEventKind::ScrollDown) {
        pty.scroll_viewport_down(WALL_WHEEL_ROWS);
    } else {
        pty.scroll_viewport_up(WALL_WHEEL_ROWS);
    }
}

/// Shift+arrow movement inside the wall grid. Clamps at the edges like the list does, and pins
/// the list selection onto the tile so Ctrl+O zooms the tile that has the keyboard.
pub(crate) fn move_wall_selection(ui: &mut Ui, dx: i32, dy: i32) {
    let keys = agent_viewer_tui::ui::wall::tile_keys(&ui.app, now_ms());
    let count = keys.len();
    if count == 0 {
        return;
    }
    let (cols, rows) = agent_viewer_tui::ui::wall::grid_dims(count);
    let (cols, rows) = (i32::from(cols), i32::from(rows));
    let current = ui.wall.focus_index(&keys) as i32;
    let column = (current % cols + dx).clamp(0, cols - 1);
    let row = (current / cols + dy).clamp(0, rows - 1);
    // A short last row clamps back onto its final tile rather than landing on a hole.
    let selected = ((row * cols + column) as usize).min(count - 1);
    ui.wall.set_focus(&keys, selected);
    ui.app.select_by_key(&keys[selected]);
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
    let directory = ui
        .app
        .spawn_target()
        .map(|target| target.displayed_directory().to_path_buf());
    let key = (ui.composer.backend(), directory);
    if ui.composer.commands_key() != Some(&key) {
        let cmds = scan_commands(key.0, key.1.as_deref());
        ui.composer.set_commands(cmds, key);
    }
}

/// Keep the composer's discovered model list current: re-install from the model cache only
/// when the composer's backend has changed (mirrors `ensure_completions`). Discovery itself
/// never runs here: `request` hands it to a worker thread, because the CLI probe behind it
/// takes seconds and this runs on the key path. Until a list exists the picker holds just the
/// backend's default; `install_models` swaps the real one in when the probe lands.
pub(crate) fn ensure_models(ui: &mut Ui) {
    // Auto has no catalog to discover: the router picks model and effort, so the picker holds
    // one "auto" entry and no probe runs.
    if ui.composer.is_auto() {
        ui.composer.set_auto_model();
        return;
    }
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
        // A catalog landing while Auto is selected must not overwrite its single entry: the
        // composer's `backend` still holds the concrete selection underneath Auto.
        if !ui.composer.is_auto() && ui.composer.backend() == backend {
            ui.composer.set_models(models, backend);
        }
    }
}

/// `Ctrl+R` — open the rename modal for the selected session, gated PER ROW on rename: claude
/// renames a bg job by writing its job dir's state.json, so an interactive row (which has no
/// job dir) is a footer notice even though the backend itself advertises rename.
pub(crate) fn open_rename(_backends: &[Box<dyn Backend>], ui: &mut Ui) {
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    open_rename_request(ui, TargetRequest::from(&session));
}

pub(crate) fn open_rename_request(ui: &mut Ui, request: TargetRequest) {
    // DELIBERATE DIVERGENCE from Fleet View, which prefills its Ctrl+R field with the current
    // name (`J2(Uf(fu.state.name ?? ""))`). Renaming here always means typing a new name from
    // scratch, so a prefill is only text to clear first. Enter on a blank buffer therefore
    // cancels rather than renaming (see `apply_rename`).
    ui.mode = Mode::Rename(RenameModal {
        backend: request.backend(),
        id: request.id().to_string(),
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
pub(crate) fn send_reply<B: ratatui::backend::Backend>(
    _backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    _terminal: &mut ratatui::Terminal<B>,
) -> io::Result<()> {
    if !matches!(ui.mode, Mode::Reply(_)) {
        return Ok(());
    }
    ui.set_notice("reply is not supported".to_string());
    Ok(())
}

/// `Ctrl+N` — open the triage inbox on the needs-input queue, oldest first.
///
/// The queue is snapshotted here and not rebuilt while the modal is up: the 1s background
/// refresh must not reorder or resize it under the user's fingers mid-answer. An empty queue
/// opens nothing — a footer notice, and the list is untouched.
pub(crate) fn open_triage(ui: &mut Ui) {
    let items = triage_queue(ui.app.sessions());
    if items.is_empty() {
        ui.set_notice("nothing waiting for input".to_string());
        return;
    }
    ui.mode = Mode::Triage(TriageState::new(items));
    attach_triage_item(ui);
}

/// Put the item under the cursor live in the panel, using the SAME attach the list's Enter
/// uses — a resolved backend command in a `PtySession`, cached in `ui.attached` and keyed by
/// session. Triage invents no second way to reach a session, so whatever attach semantics a
/// backend has (claude resumes the same thread; codex goes through the app-server daemon)
/// are exactly what triage inherits.
///
/// Attach resolution is off-thread, so this only submits; `install_attach_plan` lands the
/// child. A session already attached from an earlier visit is reused, not respawned. Nothing
/// is prefetched: walking a queue of forty costs one attach per item you actually look at.
pub(crate) fn attach_triage_item(ui: &mut Ui) {
    let Mode::Triage(state) = &ui.mode else {
        return;
    };
    let Some(item) = state.current() else {
        return;
    };
    let key: Key = item.key();
    // The panel shows whatever is under `ui.focused`; point it at this item before the attach
    // lands so a revisit renders its live child on the very next frame.
    ui.focused = Some(key.clone());
    let Some(session) = ui.app.session_for(&key).cloned() else {
        ui.set_notice(format!("{} is no longer listed", item.title));
        return;
    };
    ui.focused_session = Some(session.clone());
    if ui.attached.contains_key(&key) {
        return;
    }
    submit_attach(ui, TargetRequest::from(&session));
}

/// `Ctrl+N` inside the modal — step to the next item; running off the end closes the modal.
pub(crate) fn skip_triage_item(ui: &mut Ui) {
    let Mode::Triage(state) = &mut ui.mode else {
        return;
    };
    if state.advance() {
        attach_triage_item(ui);
        return;
    }
    close_triage(ui);
}

/// `Ctrl+P` inside the modal — step back to the previous item. A no-op on the first.
pub(crate) fn back_triage_item(ui: &mut Ui) {
    let Mode::Triage(state) = &mut ui.mode else {
        return;
    };
    if state.back() {
        attach_triage_item(ui);
    }
}

/// Leave the queue for the list. The children stay alive and stay in `ui.attached`: they are
/// real sessions, and detaching from a session has never meant stopping it.
pub(crate) fn close_triage(ui: &mut Ui) {
    ui.mode = Mode::Normal;
    ui.focused = None;
    ui.focused_session = None;
    ui.set_notice("triage: queue closed".to_string());
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
    let key = format!("{}:{}:rename", backend_kind.name(), id);
    let mutation = Mutation::Rename(TargetRequest::new(backend_kind, id), name.clone());
    let executor = ui.mutation_executor.clone();
    if ui.mutations.submit(key, move || executor(mutation)) {
        ui.set_notice(format!("renaming… {name}"));
    }
}

pub(crate) fn kill_selected(_backends: &[Box<dyn Backend>], ui: &mut Ui) {
    let now = now_ms();
    let stage = ui.app.kill_stage(now);
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let request = TargetRequest::from(&session);
    kill_request(ui, request, session.title, stage);
}

pub(crate) fn kill_request(ui: &mut Ui, request: TargetRequest, title: String, stage: KillStage) {
    match stage {
        KillStage::Stop => {
            let key = format!("{}:{}:kill", request.backend().name(), request.id());
            let mutation = Mutation::Stop(request);
            let executor = ui.mutation_executor.clone();
            if ui.mutations.submit(key, move || executor(mutation)) {
                ui.set_notice(format!("stopping… {title}"));
            }
        }
        KillStage::Remove => {
            let key = (request.backend(), request.id().to_string());
            let Some(require_finished) = ui
                .app
                .session_for(&key)
                .map(|session| session.status.is_finished())
            else {
                ui.set_notice("session is no longer available".to_string());
                return;
            };
            submit_kill_mutation(ui, request, &title, "removing", move |request| {
                Mutation::Remove {
                    request,
                    require_finished,
                }
            });
        }
        KillStage::Noop => {}
    }
}

pub(crate) fn hide_selected(_backends: &[Box<dyn Backend>], ui: &mut Ui, hide: bool) {
    let Some(session) = ui.app.selected().cloned() else {
        return;
    };
    let request = TargetRequest::from(&session);
    hide_request(ui, request, session.title, hide);
}

pub(crate) fn hide_request(ui: &mut Ui, request: TargetRequest, title: String, hide: bool) {
    if hide {
        submit_mutation(ui, request, &title, "hide", "archiving", Mutation::Hide);
    } else {
        submit_mutation(
            ui,
            request,
            &title,
            "unhide",
            "unarchiving",
            Mutation::Unhide,
        );
    }
}

fn submit_kill_mutation(
    ui: &mut Ui,
    request: TargetRequest,
    title: &str,
    verb: &str,
    mutation: impl FnOnce(TargetRequest) -> Mutation,
) {
    let key = format!("{}:{}:kill", request.backend().name(), request.id());
    let mutation = mutation(request);
    let executor = ui.mutation_executor.clone();
    if ui
        .mutations
        .submit_after_success(key, move || executor(mutation))
    {
        ui.set_notice(format!("{verb}… {title}"));
    }
}

/// Route a blocking mutation to the runner with a backend+id+op dedup key and an
/// immediate "<verb>… <title>" notice (a duplicate keypress while pending is a no-op).
fn submit_mutation(
    ui: &mut Ui,
    request: TargetRequest,
    title: &str,
    op: &str,
    verb: &str,
    mutation: impl FnOnce(TargetRequest) -> Mutation,
) {
    let key = format!("{}:{}:{}", request.backend().name(), request.id(), op);
    let mutation = mutation(request);
    let executor = ui.mutation_executor.clone();
    if ui.mutations.submit(key, move || executor(mutation)) {
        ui.set_notice(format!("{verb}… {title}"));
    }
}

/// The live backend instance for a kind, if present in the slice.
fn backend_of(backends: &[Box<dyn Backend>], kind: BackendKind) -> Option<&dyn Backend> {
    backends
        .iter()
        .find(|b| b.kind() == kind)
        .map(|b| b.as_ref())
}

pub(crate) fn attach_selected(ui: &mut Ui) -> bool {
    let Some(session) = ui.app.selected().cloned() else {
        return false;
    };
    submit_attach(ui, TargetRequest::from(&session))
}

pub(crate) fn submit_attach(ui: &mut Ui, request: TargetRequest) -> bool {
    let id = request.id().to_string();
    let key: Key = (request.backend(), id.clone());
    // Triage hosts its child in the modal's panel rather than the whole screen, so a plan
    // resolved for the queue must be able to tell that apart when it lands.
    let triage = matches!(ui.mode, Mode::Triage(_));
    let executor = ui.attach_executor.clone();
    // Keyed per session: mashing → on one row still dedups, but a request for a DIFFERENT row
    // must not be silently swallowed because an earlier one is still resolving. Which landed
    // plan is still the one the user is looking at is decided on completion, by the ownership
    // guard in the run loop, exactly as the wall's per-session joins are.
    let runner_key = format!("attach:{}:{}", key.0.name(), key.1);
    let outcome_key = key;
    if !ui.attaches.submit(runner_key, move || {
        executor(request).map(|plan| crate::AttachOutcome::Focus {
            key: outcome_key,
            triage,
            plan,
        })
    }) {
        return false;
    }
    ui.set_notice(format!("attaching… {id}"));
    true
}

/// Install a fresh authoritative attach plan on the UI thread.
pub(crate) fn install_attach_plan<B: ratatui::backend::Backend>(
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
    plan: AttachPlan,
) -> io::Result<bool> {
    let AttachPlan { session, command } = plan;
    let key: Key = (session.backend, session.id.clone());
    let capture_on_attach = matches!(session.backend, BackendKind::Codex | BackendKind::Claude);
    let size = terminal
        .size()
        .map_err(|error| io::Error::other(error.to_string()))?;
    // The triage inbox hosts the child in a panel inside its chrome; full-screen attach gives
    // it everything but the header and notice rows. A child sized to anything other than the
    // rect it is drawn into wraps its own output at the wrong column.
    let triage = matches!(ui.mode, Mode::Triage(_));
    let (rows, cols) = if triage {
        crate::ui::panel_pty_size(size.into()).unwrap_or((1, 1))
    } else {
        (
            size.height.saturating_sub(ATTACHED_CHROME_ROWS).max(1),
            size.width.max(1),
        )
    };
    let palette = ui
        .themes
        .active()
        .terminal_palette()
        .or(ui.terminal_palette);

    if let Some(pty) = ui.attached.get_mut(&key) {
        // The wall already holds a connection to this session and the user is zooming into
        // that tile: reuse the live PTY, resized to the full content area. This is the only
        // way a PTY exists here, since leaving a session otherwise closes it.
        pty.set_palette(palette);
        let _ = pty.resize(rows, cols);
        ui.detach_trackers.entry(key.clone()).or_default();
    } else {
        let mut spec = spec_from_command(&command, rows, cols);
        spec.palette = palette;
        if session.backend == BackendKind::Codex {
            spec.scrollback_rows = VIEWPORT_SCROLLBACK_ROWS;
        }
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
    ui.focused_session = Some(session);
    // Triage keeps its modal: the child it just landed belongs in the panel, not on the whole
    // screen. Leaving the mode alone is what makes the queue survive an attach.
    if !triage {
        ui.mode = Mode::Attached;
    }
    // Codex and Claude scroll immediately. External opencode keeps host text selection until
    // Ctrl+T opts into native wheel forwarding.
    set_mouse_capture(ui, capture_on_attach);
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
    if ui.composer.is_auto() {
        spawn_through_router(refresher, ui, target);
        return;
    }
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

/// The Auto path: hand the task to `agent-router`, which classifies it, weighs weekly usage
/// headroom, and dispatches to whichever backend won. No backend is consulted here — Auto is
/// not one — and no model is passed, because the router owns model and effort selection.
fn spawn_through_router(refresher: &Refresher, ui: &mut Ui, target: SpawnTarget) {
    let task = ui.composer.text().to_string();
    // Same dedup shape as a backend spawn, so a double Enter cannot route the same task twice
    // while the first classifier call is still out.
    let key = format!("auto:{task}:spawn");
    // No submission timestamp: the router's own return is what stamps the routed spawn, since
    // classification plus dispatch can outlive the 30s row-matching window from here.
    let mutation = Mutation::spawn_auto(&ui.app, target, task);
    let executor = ui.mutation_executor.clone();
    if !ui.mutations.submit(key, move || executor(mutation)) {
        return;
    }
    ui.set_notice("routing… via agent-router".to_string());
    ui.composer.clear();
    refresher.force();
}

#[cfg(test)]
mod tests {
    use super::{install_attach_plan, kill_request, spawn_from_composer};
    use crate::Refresher;
    use crate::keys::handle_paste;
    use crate::keys::tests::{sess, test_ui_with};
    use crate::ops::{Mutation, resolve_attach_with_backend};
    use agent_viewer_core::pty::TerminalPalette;
    use agent_viewer_core::{AttachRefusal, BackendKind, Capabilities, Session, Status};
    use agent_viewer_tui::app::KillStage;
    use agent_viewer_tui::mutations::MutationOutcome;
    use agent_viewer_tui::shared_listing::TargetRequest;
    use agent_viewer_tui::ui::{ATTACHED_CHROME_ROWS, Mode};
    use std::sync::{
        Arc, Mutex,
        mpsc::{TryRecvError, channel},
    };
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
            _effort: Option<&str>,
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

    struct PaletteQueryBackend {
        session: Session,
    }

    impl agent_viewer_core::Backend for PaletteQueryBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                attach: true,
                ..Capabilities::none()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            Ok(vec![self.session.clone()])
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawning is not exercised by palette attach")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, AttachRefusal> {
            let script = concat!(
                "stty raw -echo; ",
                "printf '\\033]10;?\\007\\033]11;?\\007'; ",
                "bytes=$(dd bs=1 count=50 2>/dev/null | od -An -v -tx1 | tr -d ' \\n'); ",
                "printf 'OSC:%s\\r\\n' \"$bytes\"; ",
                "sleep 30"
            );
            let mut command = std::process::Command::new("sh");
            command.args(["-c", script]);
            Ok(command)
        }
    }

    struct RefusingAttachBackend {
        session: Session,
        refusal: AttachRefusal,
    }

    impl agent_viewer_core::Backend for RefusingAttachBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                attach: true,
                ..Capabilities::none()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            Ok(vec![self.session.clone()])
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawning is not exercised by refused attach")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, AttachRefusal> {
            Err(self.refusal.clone())
        }
    }

    fn osc_reply_hex(slot: u8, color: [u8; 3]) -> String {
        let [red, green, blue] = color;
        format!(
            "\x1b]{slot};rgb:{red:02X}{red:02X}/{green:02X}{green:02X}/{blue:02X}{blue:02X}\x1b\\"
        )
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect()
    }

    fn osc_replies(palette: TerminalPalette) -> String {
        format!(
            "OSC:{}{}",
            osc_reply_hex(10, palette.foreground),
            osc_reply_hex(11, palette.background),
        )
    }

    fn wait_for_attached_screen(ui: &crate::Ui, key: &(BackendKind, String), needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ui
            .attached
            .get(key)
            .is_some_and(|pty| pty.with_screen(|screen| screen.contents().contains(needle)))
        {
            assert!(
                Instant::now() < deadline,
                "attached child screen did not contain {needle:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn inert_refresher() -> Refresher {
        let (_snapshot_tx, snapshots) = channel();
        let (wake, _wake_rx) = channel();
        Refresher { snapshots, wake }
    }

    fn poll_mutation(ui: &mut crate::Ui) -> Result<MutationOutcome, String> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(result) = ui.mutations.poll() {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "background mutation did not finish"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn kill_remove_waits_for_stop_success() {
        let session = sess("kill_success", "/tmp/agentviewer_kill_success", 100);
        let request = TargetRequest::from(&session);
        let mut ui = test_ui_with(vec![session]);
        let (stop_started_tx, stop_started_rx) = channel();
        let (release_stop_tx, release_stop_rx) = channel();
        let release_stop_rx = Arc::new(Mutex::new(release_stop_rx));
        let (remove_started_tx, remove_started_rx) = channel();
        ui.mutation_executor = Arc::new(move |mutation| match mutation {
            Mutation::Stop(_) => {
                stop_started_tx.send(()).expect("report stop start");
                release_stop_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(1))
                    .expect("release stop");
                Ok(MutationOutcome {
                    notice: "stopped".to_string(),
                    spawned: None,
                })
            }
            Mutation::Remove { .. } => {
                remove_started_tx.send(()).expect("report remove start");
                Ok(MutationOutcome {
                    notice: "removed".to_string(),
                    spawned: None,
                })
            }
            _ => panic!("unexpected mutation"),
        });

        kill_request(
            &mut ui,
            request.clone(),
            "kill success".to_string(),
            KillStage::Stop,
        );
        stop_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("stop started");
        kill_request(
            &mut ui,
            request,
            "kill success".to_string(),
            KillStage::Remove,
        );
        assert_eq!(remove_started_rx.try_recv(), Err(TryRecvError::Empty));

        release_stop_tx.send(()).expect("finish stop");
        assert_eq!(
            poll_mutation(&mut ui),
            Ok(MutationOutcome {
                notice: "stopped".to_string(),
                spawned: None,
            })
        );
        remove_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("remove started after stop");
        assert_eq!(
            poll_mutation(&mut ui),
            Ok(MutationOutcome {
                notice: "removed".to_string(),
                spawned: None,
            })
        );
    }

    #[test]
    fn kill_remove_is_discarded_after_stop_failure() {
        let session = sess("kill_failure", "/tmp/agentviewer_kill_failure", 100);
        let request = TargetRequest::from(&session);
        let mut ui = test_ui_with(vec![session]);
        let (stop_started_tx, stop_started_rx) = channel();
        let (release_stop_tx, release_stop_rx) = channel();
        let release_stop_rx = Arc::new(Mutex::new(release_stop_rx));
        let (remove_started_tx, remove_started_rx) = channel();
        ui.mutation_executor = Arc::new(move |mutation| match mutation {
            Mutation::Stop(_) => {
                stop_started_tx.send(()).expect("report stop start");
                release_stop_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(1))
                    .expect("release stop");
                Err("stop failed".to_string())
            }
            Mutation::Remove { .. } => {
                remove_started_tx.send(()).expect("report remove start");
                Ok(MutationOutcome {
                    notice: "removed".to_string(),
                    spawned: None,
                })
            }
            _ => panic!("unexpected mutation"),
        });

        kill_request(
            &mut ui,
            request.clone(),
            "kill failure".to_string(),
            KillStage::Stop,
        );
        stop_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("stop started");
        kill_request(
            &mut ui,
            request,
            "kill failure".to_string(),
            KillStage::Remove,
        );
        assert_eq!(remove_started_rx.try_recv(), Err(TryRecvError::Empty));

        release_stop_tx.send(()).expect("finish stop");
        assert_eq!(poll_mutation(&mut ui), Err("stop failed".to_string()));
        drop(ui);
        assert_eq!(
            remove_started_rx.try_recv(),
            Err(TryRecvError::Disconnected)
        );
    }

    #[test]
    fn kill_stop_deduplicates_after_the_removal_window_expires() {
        let mut session = sess("kill_dedup", "/tmp/agentviewer_kill_dedup", 100);
        session.status = Status::Working;
        let request = TargetRequest::from(&session);
        let mut ui = test_ui_with(vec![session]);
        let (stop_started_tx, stop_started_rx) = channel();
        let (release_stop_tx, release_stop_rx) = channel();
        let release_stop_rx = Arc::new(Mutex::new(release_stop_rx));
        ui.mutation_executor = Arc::new(move |mutation| match mutation {
            Mutation::Stop(_) => {
                stop_started_tx.send(()).expect("report stop start");
                release_stop_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(1))
                    .expect("release stop");
                Ok(MutationOutcome {
                    notice: "stopped".to_string(),
                    spawned: None,
                })
            }
            _ => panic!("unexpected mutation"),
        });

        let first_stage = ui.app.kill_stage(1_000);
        assert_eq!(first_stage, KillStage::Stop);
        kill_request(
            &mut ui,
            request.clone(),
            "kill dedup".to_string(),
            first_stage,
        );
        stop_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("stop started");

        let repeated_stage = ui.app.kill_stage(3_001);
        assert_eq!(repeated_stage, KillStage::Stop);
        kill_request(&mut ui, request, "kill dedup".to_string(), repeated_stage);
        assert_eq!(stop_started_rx.try_recv(), Err(TryRecvError::Empty));

        release_stop_tx.send(()).expect("finish stop");
        assert_eq!(
            poll_mutation(&mut ui),
            Ok(MutationOutcome {
                notice: "stopped".to_string(),
                spawned: None,
            })
        );
        drop(ui);
        assert_eq!(stop_started_rx.try_recv(), Err(TryRecvError::Disconnected));
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

    /// With Auto selected the spawn must leave the Backend trait entirely: no backend is
    /// consulted, no model is chosen, and the winning provider comes back from the router.
    #[test]
    fn an_auto_submission_routes_through_agent_router_instead_of_a_backend() {
        let routed = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&routed);
        let mut ui = test_ui_with(vec![sess("router_target", "/tmp/agentviewer_auto", 100)]);
        ui.mutation_executor = Arc::new(move |mutation| {
            let Mutation::SpawnAuto {
                task,
                preexisting_ids,
                ..
            } = mutation
            else {
                panic!("an auto submission must never reach a backend spawn");
            };
            // The row selection needs the winning provider's preexisting ids, and which
            // provider wins is unknown until the router answers, so all three are captured.
            assert!(preexisting_ids.contains_key(&BackendKind::Claude));
            assert!(preexisting_ids.contains_key(&BackendKind::Codex));
            assert!(preexisting_ids.contains_key(&BackendKind::Opencode));
            recorded.lock().unwrap().push(task);
            Ok(MutationOutcome {
                notice: "auto: codex effort xhigh (codex weekly 87%, claude 52%)".to_string(),
                spawned: None,
            })
        });
        ui.composer.set_auto_available(true);
        while !ui.composer.is_auto() {
            ui.composer.cycle_backend();
        }
        ui.composer.push_str("route this somewhere");

        // No backends at all: the auto path must not need one.
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        spawn_from_composer(&backends, &inert_refresher(), &mut ui);

        assert_eq!(ui.composer.text(), "");
        let deadline = Instant::now() + Duration::from_secs(1);
        while ui.mutations.poll().is_none() {
            assert!(Instant::now() < deadline, "auto mutation did not finish");
            std::thread::yield_now();
        }
        assert_eq!(
            *routed.lock().unwrap(),
            vec!["route this somewhere".to_string()]
        );
    }

    #[test]
    fn initial_attach_sizes_the_real_pty_to_the_terminal_content_area() {
        let session = sess("sized_attach", "/tmp/agentviewer_sized_attach", 100);
        let mut ui = test_ui_with(vec![session.clone()]);
        let terminal_width = 137;
        let terminal_height = 29;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
            terminal_width,
            terminal_height,
        ))
        .expect("test terminal");
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let plan = crate::ops::AttachPlan {
            session: session.clone(),
            command,
        };

        assert!(install_attach_plan(&mut ui, &mut terminal, plan).expect("install attach plan"));

        let key = (BackendKind::Claude, session.id);
        let screen_size = ui
            .attached
            .get(&key)
            .expect("fresh attached pty")
            .with_screen(|screen| screen.size());
        assert_eq!(
            screen_size,
            (terminal_height - ATTACHED_CHROME_ROWS, terminal_width)
        );

        ui.attached
            .get_mut(&key)
            .expect("fresh attached pty")
            .kill();
    }

    #[test]
    fn retained_reattach_resizes_the_real_pty_to_the_terminal_content_area() {
        let session = sess("sized_reattach", "/tmp/agentviewer_sized_reattach", 100);
        let mut ui = test_ui_with(vec![session.clone()]);
        let mut initial_terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(91, 17))
                .expect("initial test terminal");
        let mut initial_command = std::process::Command::new("sh");
        initial_command.args(["-c", "sleep 30"]);
        let initial_plan = crate::ops::AttachPlan {
            session: session.clone(),
            command: initial_command,
        };
        assert!(
            install_attach_plan(&mut ui, &mut initial_terminal, initial_plan)
                .expect("install initial attach plan")
        );

        let key = (BackendKind::Claude, session.id.clone());
        let initial_pid = ui.attached.get(&key).expect("initial attached pty").pid();
        let terminal_width = 149;
        let terminal_height = 43;
        let mut retained_terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
            terminal_width,
            terminal_height,
        ))
        .expect("retained test terminal");
        let mut retained_command = std::process::Command::new("sh");
        retained_command.args(["-c", "exit 99"]);
        let retained_plan = crate::ops::AttachPlan {
            session,
            command: retained_command,
        };

        assert!(
            install_attach_plan(&mut ui, &mut retained_terminal, retained_plan)
                .expect("install retained attach plan")
        );

        let retained_pty = ui.attached.get(&key).expect("retained attached pty");
        assert_eq!(retained_pty.pid(), initial_pid);
        assert_eq!(
            retained_pty.with_screen(|screen| screen.size()),
            (terminal_height - ATTACHED_CHROME_ROWS, terminal_width)
        );

        ui.attached
            .get_mut(&key)
            .expect("retained attached pty")
            .kill();
    }

    #[test]
    fn new_attach_answers_osc_palette_queries_with_the_active_amber_theme() {
        let mut session = sess("palette", "/tmp/agentviewer-palette", 100);
        session.short_id = Some("palette".into());
        let mut ui = test_ui_with(vec![session.clone()]);
        ui.terminal_palette = Some(TerminalPalette {
            foreground: [0x01, 0x02, 0x03],
            background: [0x04, 0x05, 0x06],
        });
        let mut backend = PaletteQueryBackend {
            session: session.clone(),
        };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 24))
            .expect("test terminal");

        let plan = resolve_attach_with_backend(&mut backend, TargetRequest::from(&session))
            .expect("resolve palette query child");
        assert!(
            install_attach_plan(&mut ui, &mut terminal, plan).expect("attach palette query child")
        );

        let theme = agent_viewer_tui::ui::theme::amber(false);
        let ratatui::style::Color::Rgb(red, green, blue) = theme.text else {
            panic!("amber text must be RGB");
        };
        let ratatui::style::Color::Rgb(bg_red, bg_green, bg_blue) = theme.bg else {
            panic!("amber background must be RGB");
        };
        let expected = format!(
            "OSC:{}{}",
            osc_reply_hex(10, [red, green, blue]),
            osc_reply_hex(11, [bg_red, bg_green, bg_blue]),
        );
        let key = (BackendKind::Claude, session.id.clone());

        wait_for_attached_screen(&ui, &key, &expected);

        ui.attached.get_mut(&key).expect("attached child").kill();
    }

    #[test]
    fn reattaching_a_retained_pty_refreshes_its_palette_for_the_new_active_theme() {
        let mut session = sess("palette_refresh", "/tmp/agentviewer_palette_refresh", 100);
        session.short_id = Some("palette_refresh".into());
        let mut ui = test_ui_with(vec![session.clone()]);
        let old_palette = ui
            .themes
            .active()
            .terminal_palette()
            .expect("default amber palette");
        let mut backend = PaletteQueryBackend {
            session: session.clone(),
        };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 24))
            .expect("test terminal");
        let key = (BackendKind::Claude, session.id.clone());

        let initial_plan = resolve_attach_with_backend(&mut backend, TargetRequest::from(&session))
            .expect("resolve initial palette attach");
        assert!(
            install_attach_plan(&mut ui, &mut terminal, initial_plan)
                .expect("initial palette attach")
        );
        let first_pty = ui.attached.get(&key).expect("initial retained pty") as *const _;
        let first_pid = ui
            .attached
            .get(&key)
            .expect("initial retained pty")
            .pid()
            .expect("initial retained pty pid");
        assert_eq!(
            ui.attached
                .get(&key)
                .expect("initial retained pty")
                .palette(),
            Some(old_palette)
        );

        ui.themes.move_preview(2);
        let new_palette = ui
            .themes
            .active()
            .terminal_palette()
            .expect("new active RGB theme palette");
        assert_ne!(old_palette, new_palette);

        let retained_plan =
            resolve_attach_with_backend(&mut backend, TargetRequest::from(&session))
                .expect("resolve retained palette attach");
        assert!(
            install_attach_plan(&mut ui, &mut terminal, retained_plan)
                .expect("reattach retained pty")
        );
        let retained_pty = ui.attached.get(&key).expect("retained pty after reattach");
        assert_eq!(
            first_pty, retained_pty as *const _,
            "reattach must preserve the original PtySession instance"
        );
        assert_eq!(
            retained_pty.pid(),
            Some(first_pid),
            "reattach must preserve the original PTY child"
        );
        assert_eq!(
            retained_pty.palette(),
            Some(new_palette),
            "reattach must replace the retained PTY palette with the active theme"
        );

        ui.attached.get_mut(&key).expect("retained child").kill();
    }

    #[test]
    fn terminal_match_attach_answers_osc_palette_queries_with_the_captured_host_palette() {
        let mut session = sess("terminal-palette", "/tmp/agentviewer-terminal-palette", 100);
        session.short_id = Some("terminal-palette".into());
        let host_palette = TerminalPalette {
            foreground: [0x11, 0x22, 0x33],
            background: [0x44, 0x55, 0x66],
        };
        let mut ui = test_ui_with(vec![session.clone()]);
        ui.terminal_palette = Some(host_palette);
        ui.themes.move_preview(1);
        assert_eq!(ui.themes.active().id, "terminal");
        let mut backend = PaletteQueryBackend {
            session: session.clone(),
        };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 24))
            .expect("test terminal");

        let plan = resolve_attach_with_backend(&mut backend, TargetRequest::from(&session))
            .expect("resolve host palette attach");
        assert!(
            install_attach_plan(&mut ui, &mut terminal, plan).expect("attach palette query child")
        );

        let key = (BackendKind::Claude, session.id.clone());
        wait_for_attached_screen(&ui, &key, &osc_replies(host_palette));

        ui.attached.get_mut(&key).expect("attached child").kill();
    }

    #[test]
    fn rgb_theme_named_terminal_answers_osc_queries_with_its_own_palette() {
        let theme_dir = tempfile::tempdir().expect("theme directory");
        std::fs::write(
            theme_dir.path().join("terminal.theme"),
            "text=#010203\nbg=#040506\n",
        )
        .expect("user theme");
        let (mut themes, notices) =
            agent_viewer_tui::ui::ThemeState::load(false, None, theme_dir.path());
        assert!(notices.is_empty());
        let user_theme_index = themes.themes().len() - 1;
        themes.move_preview(user_theme_index as i32);
        assert_eq!(themes.active_index(), user_theme_index);
        assert_eq!(themes.active().id, "terminal");
        let expected_palette = themes
            .active()
            .terminal_palette()
            .expect("user theme palette");

        let mut session = sess("rgb-terminal", "/tmp/agentviewer-rgb-terminal", 100);
        session.short_id = Some("rgb-terminal".into());
        let mut ui = test_ui_with(vec![session.clone()]);
        ui.themes = themes;
        ui.terminal_palette = Some(TerminalPalette {
            foreground: [0xa1, 0xb2, 0xc3],
            background: [0xd4, 0xe5, 0xf6],
        });
        let mut backend = PaletteQueryBackend {
            session: session.clone(),
        };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 24))
            .expect("test terminal");

        let plan = resolve_attach_with_backend(&mut backend, TargetRequest::from(&session))
            .expect("resolve RGB palette attach");
        assert!(
            install_attach_plan(&mut ui, &mut terminal, plan).expect("attach palette query child")
        );

        let key = (BackendKind::Claude, session.id.clone());
        wait_for_attached_screen(&ui, &key, &osc_replies(expected_palette));

        ui.attached.get_mut(&key).expect("attached child").kill();
    }

    #[test]
    fn a_tailable_refusal_returns_its_reason_without_changing_ui() {
        // A live `codex exec` thread cannot be joined because its app server runs in process.
        // The resolver must return its reason without attempting a plain resume or mutating UI.
        let session = sess("refused_tail", "/tmp/agentviewer_refused_tail", 100);
        let mut backend = RefusingAttachBackend {
            session: session.clone(),
            refusal: AttachRefusal::tailable("cannot be joined"),
        };
        let ui = test_ui_with(vec![session.clone()]);

        let result = resolve_attach_with_backend(&mut backend, TargetRequest::from(&session));

        assert_eq!(result.err().as_deref(), Some("cannot be joined"));
        assert!(ui.notice.text.is_empty());
        assert!(ui.attached.is_empty());
        assert!(ui.focused.is_none());
        assert!(matches!(ui.mode, Mode::Normal));
    }

    #[test]
    fn a_plain_refusal_returns_its_reason_without_changing_ui() {
        let session = sess("refused_plain", "/tmp/agentviewer_refused_plain", 100);
        let mut backend = RefusingAttachBackend {
            session: session.clone(),
            refusal: AttachRefusal::new("no attach here"),
        };
        let ui = test_ui_with(vec![session.clone()]);

        let result = resolve_attach_with_backend(&mut backend, TargetRequest::from(&session));

        assert_eq!(result.err().as_deref(), Some("no attach here"));
        assert!(ui.notice.text.is_empty());
        assert!(ui.attached.is_empty());
        assert!(ui.focused.is_none());
        assert!(matches!(ui.mode, Mode::Normal));
    }
}

#[cfg(test)]
mod async_attach_tests {
    use super::{close_triage, install_attach_plan, open_triage, skip_triage_item, submit_attach};
    use crate::keys::tests::{sess, test_ui_with};
    use crate::ops::AttachPlan;
    use agent_viewer_core::{BackendKind, Session, Status};
    use agent_viewer_tui::shared_listing::TargetRequest;
    use agent_viewer_tui::ui::Mode;
    use std::sync::{Arc, Mutex, mpsc::channel};
    use std::thread;
    use std::time::{Duration, Instant};

    /// A session already waiting on the user, so `open_triage` has a queue to walk.
    fn blocked(id: &str, updated_at_ms: i64) -> Session {
        let mut session = sess(id, "/tmp/agentviewer_triage_attach", updated_at_ms);
        session.status = Status::NeedsInput {
            reason: Some("Pick a direction.".to_string()),
        };
        session
    }

    fn sleeping_plan(session: &Session) -> AttachPlan {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30"]);
        AttachPlan {
            session: session.clone(),
            command,
        }
    }

    fn poll_attach(ui: &mut crate::Ui) -> Result<crate::AttachOutcome, String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = ui.attaches.poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "attach worker did not finish");
            thread::yield_now();
        }
    }

    /// Drive one landed attach result through the exact path the run loop uses.
    fn land_attach(ui: &mut crate::Ui, result: Result<crate::AttachOutcome, String>) {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24))
            .expect("test terminal");
        let mut output = Vec::new();
        let mut applied = true;
        crate::apply_attach_result(ui, &mut terminal, result, &mut output, &mut applied)
            .expect("apply attach result");
    }

    #[test]
    fn attach_submission_returns_before_authority_finishes_and_deduplicates_the_target() {
        let displayed = sess(
            "blocked_authority",
            "/tmp/agentviewer_blocked_authority",
            100,
        );
        let request = TargetRequest::from(&displayed);
        let mut ui = test_ui_with(vec![displayed]);
        let caller = thread::current().id();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        ui.attach_executor = Arc::new(move |request| {
            recorded.lock().expect("record calls").push(request);
            started_tx
                .send(thread::current().id())
                .expect("report authority worker");
            release_rx
                .lock()
                .expect("release receiver")
                .recv_timeout(Duration::from_secs(2))
                .expect("release blocked authority");
            Err("fresh authority refused attach".to_string())
        });

        let started = Instant::now();
        assert!(submit_attach(&mut ui, request.clone()));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "attach submission blocked on authoritative listing"
        );
        let worker = started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority worker started");
        assert_ne!(worker, caller);
        assert!(!submit_attach(&mut ui, request.clone()));
        assert_eq!(
            ui.notice.text, "attaching… blocked_authority",
            "duplicate submission must preserve the first pending attach"
        );

        release_tx.send(()).expect("release authority worker");
        let deadline = Instant::now() + Duration::from_secs(1);
        let result = loop {
            if let Some(result) = ui.attaches.poll() {
                break result;
            }
            assert!(Instant::now() < deadline, "authority worker did not finish");
            thread::yield_now();
        };

        assert_eq!(
            *calls.lock().expect("recorded calls"),
            vec![request.clone()]
        );
        assert_eq!(
            result.err().as_deref(),
            Some("fresh authority refused attach")
        );
        assert!(ui.attached.is_empty());
        assert!(ui.focused.is_none());

        ui.attach_executor = Arc::new(|_| Err("claude session is no longer available".to_string()));
        assert!(submit_attach(&mut ui, request));
        let deadline = Instant::now() + Duration::from_secs(1);
        let missing = loop {
            if let Some(result) = ui.attaches.poll() {
                break result;
            }
            assert!(Instant::now() < deadline, "missing result did not finish");
            thread::yield_now();
        };
        assert_eq!(
            missing.err().as_deref(),
            Some("claude session is no longer available")
        );
        assert!(ui.attached.is_empty());
        assert!(ui.focused.is_none());
    }

    #[test]
    fn completed_attach_plan_focuses_the_fresh_authoritative_session() {
        let displayed = sess("fresh_focus", "/tmp/agentviewer_displayed_attach", 100);
        let mut fresh = displayed.clone();
        fresh.title = "fresh authoritative title".to_string();
        fresh.cwd = "/tmp/agentviewer_fresh_attach".into();
        fresh.updated_at_ms = 200;
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let plan = AttachPlan {
            session: fresh.clone(),
            command,
        };
        let mut ui = test_ui_with(vec![displayed]);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 24))
            .expect("test terminal");

        assert!(install_attach_plan(&mut ui, &mut terminal, plan).expect("install attach plan"));

        let key = (BackendKind::Claude, fresh.id.clone());
        assert_eq!(ui.focused.as_ref(), Some(&key));
        assert_eq!(ui.focused_session.as_ref(), Some(&fresh));
        assert!(matches!(ui.mode, Mode::Attached));
        assert!(ui.attached.contains_key(&key));

        ui.attached
            .get_mut(&key)
            .expect("fresh attached child")
            .kill();
    }

    /// The control for the two drop tests below: a plan that lands while its row is still the
    /// selected one is installed exactly as before.
    #[test]
    fn a_landed_focus_attach_is_installed_while_its_row_is_still_selected() {
        let first = sess("still_selected", "/tmp/agentviewer_still_selected", 100);
        let other = sess("elsewhere", "/tmp/agentviewer_elsewhere", 200);
        let mut ui = test_ui_with(vec![first.clone(), other]);
        let planned = first.clone();
        ui.attach_executor = Arc::new(move |_| Ok(sleeping_plan(&planned)));
        let key = (BackendKind::Claude, first.id.clone());
        assert!(ui.app.select_by_key(&key));

        assert!(submit_attach(&mut ui, TargetRequest::from(&first)));
        let result = poll_attach(&mut ui);
        land_attach(&mut ui, result);

        assert!(matches!(ui.mode, Mode::Attached));
        assert_eq!(ui.focused.as_ref(), Some(&key));
        ui.attached.get_mut(&key).expect("attached child").kill();
    }

    /// The interleaving the review found: the resolution blocks, the user walks to another
    /// row, and the late plan arrives. Installing it there hands every following keystroke to
    /// a session the user is not looking at.
    #[test]
    fn a_landed_focus_attach_is_dropped_when_the_selection_moved_on() {
        let first = sess("left_behind", "/tmp/agentviewer_left_behind", 100);
        let second = sess("moved_to", "/tmp/agentviewer_moved_to", 200);
        let mut ui = test_ui_with(vec![first.clone(), second.clone()]);
        let planned = first.clone();
        ui.attach_executor = Arc::new(move |_| Ok(sleeping_plan(&planned)));
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Claude, first.id.clone()))
        );

        assert!(submit_attach(&mut ui, TargetRequest::from(&first)));
        let result = poll_attach(&mut ui);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Claude, second.id.clone()))
        );
        land_attach(&mut ui, result);

        assert!(
            ui.attached.is_empty(),
            "a late attach must not spawn a child for the row the user left"
        );
        assert!(ui.focused.is_none());
        assert!(
            matches!(ui.mode, Mode::Normal),
            "the list must not be taken over by a session the user moved off"
        );
        assert_eq!(
            ui.notice.text, "attach cancelled: left_behind is no longer in focus",
            "a dropped attach says so rather than vanishing"
        );
    }

    /// A triage attach that lands after the queue closed must not reopen as a full-screen
    /// attach: the user already left that view.
    #[test]
    fn a_landed_triage_attach_is_dropped_after_the_queue_closed() {
        let waiting = blocked("closed_queue", 100);
        let mut ui = test_ui_with(vec![waiting.clone()]);
        let planned = waiting.clone();
        ui.attach_executor = Arc::new(move |_| Ok(sleeping_plan(&planned)));

        open_triage(&mut ui);
        assert!(matches!(ui.mode, Mode::Triage(_)));
        let result = poll_attach(&mut ui);
        close_triage(&mut ui);
        land_attach(&mut ui, result);

        assert!(
            matches!(ui.mode, Mode::Normal),
            "a closed queue must not reopen as an attach view"
        );
        assert!(ui.attached.is_empty());
        assert!(ui.focused.is_none());
    }

    /// The same guard inside the queue: walking on before the first item's attach resolves
    /// must not put that child in the panel the second item now owns.
    #[test]
    fn a_landed_triage_attach_is_dropped_after_the_queue_moved_on() {
        let first = blocked("queue_first", 100);
        let second = blocked("queue_second", 200);
        let mut ui = test_ui_with(vec![first.clone(), second.clone()]);
        let planned = first.clone();
        ui.attach_executor = Arc::new(move |_| Ok(sleeping_plan(&planned)));

        open_triage(&mut ui);
        let result = poll_attach(&mut ui);
        skip_triage_item(&mut ui);
        land_attach(&mut ui, result);

        assert!(
            !ui.attached
                .contains_key(&(BackendKind::Claude, first.id.clone())),
            "the item the queue walked off must not land in the panel"
        );
        assert_eq!(
            ui.focused.as_ref(),
            Some(&(BackendKind::Claude, second.id.clone())),
            "the panel stays pointed at the item the queue is actually on"
        );
    }
}
