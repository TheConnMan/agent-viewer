//! The actions the key handlers trigger: attach/spawn, reply delivery, rename, stop/remove,
//! hide, and the completion/model list refresh. Split out of `keys` so that module holds only
//! per-mode key routing. Every fn mutates the shared `Ui` state owned by the run loop.

use std::io;

use agent_viewer_core::backend::{Backend, BackendKind};
use agent_viewer_core::pty::{PtySession, VIEWPORT_SCROLLBACK_ROWS, spec_from_command};
use agent_viewer_core::router::AUTO_MODEL;
use agent_viewer_core::spawn::now_ms;
use agent_viewer_tui::app::{DetachTracker, KillStage, SpawnRoute};
use agent_viewer_tui::composer::CommandEntry;
use agent_viewer_tui::shared_listing::{SpawnTarget, TargetRequest};
use agent_viewer_tui::ui::{ATTACHED_CHROME_ROWS, Mode, RenameModal, TriageState, triage_queue};
use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::keys::{refresh_palette_commands, set_mouse_capture};
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

fn command_target(ui: &Ui) -> Option<std::path::PathBuf> {
    ui.app
        .spawn_target()
        .map(|target| target.displayed_directory().to_path_buf())
}

fn installed_commands(ui: &Ui, target: &Option<std::path::PathBuf>) -> Vec<CommandEntry> {
    let mut commands = vec![CommandEntry::viewer("model"), CommandEntry::viewer("theme")];
    let providers = if ui.composer.is_auto() {
        ui.composer.available_backends().to_vec()
    } else {
        vec![ui.composer.backend()]
    };
    for provider in providers {
        let key = (provider, target.clone());
        if let Some(entries) = ui.models.commands(&key) {
            commands.extend_from_slice(entries);
        }
    }
    commands.sort_by(|left, right| {
        left.display()
            .cmp(right.display())
            .then_with(|| {
                left.owner()
                    .map(BackendKind::name)
                    .cmp(&right.owner().map(BackendKind::name))
            })
            .then_with(|| left.kind().cmp(&right.kind()))
            .then_with(|| left.codex_skill_path().cmp(&right.codex_skill_path()))
    });
    commands.dedup();
    commands
}

fn command_discovery_pending(ui: &Ui, target: &Option<std::path::PathBuf>) -> bool {
    let providers = if ui.composer.is_auto() {
        ui.composer.available_backends().to_vec()
    } else {
        vec![ui.composer.backend()]
    };
    providers
        .into_iter()
        .any(|provider| ui.models.commands_pending(&(provider, target.clone())))
}

/// Keep the composer's command list current. Discovery starts only when the composer contains
/// command text. Ordinary navigation may change the cached target scope, but it never probes a
/// new target by itself.
pub(crate) fn ensure_completions(ui: &mut Ui) -> usize {
    let discover = matches!(ui.composer.text().chars().next(), Some('/' | '$'));
    update_completions(ui, discover)
}

/// Ctrl K is an explicit request for the full command catalog even when the composer is empty.
pub(crate) fn request_completions(ui: &mut Ui) -> usize {
    update_completions(ui, true)
}

fn update_completions(ui: &mut Ui, discover: bool) -> usize {
    let target = command_target(ui);
    let providers = if ui.composer.is_auto() {
        ui.composer.available_backends().to_vec()
    } else {
        vec![ui.composer.backend()]
    };
    let mut requested = 0;
    if discover {
        for provider in providers {
            if ui.models.request_commands((provider, target.clone())) {
                requested += 1;
            }
        }
    }
    let key = (ui.composer.backend(), target.clone());
    if !ui.composer.commands_match_scope(&key) {
        let commands = installed_commands(ui, &target);
        ui.composer.set_commands(commands, key);
    }
    requested
}

/// Install command discovery results that landed on workers. Failed or empty probes are not
/// returned by the cache, so an existing usable catalog remains installed.
pub(crate) fn install_commands(ui: &mut Ui) {
    if ui.models.poll_commands().is_empty() {
        return;
    }
    let target = command_target(ui);
    let key = (ui.composer.backend(), target.clone());
    let commands = installed_commands(ui, &target);
    ui.composer.set_commands(commands, key);
    refresh_palette_commands(ui);
}

/// Keep the composer's discovered model list current: re-install from the model cache only
/// when the composer's backend has changed (mirrors `ensure_completions`). Discovery itself
/// never runs here: `request` hands it to a worker thread, because the CLI probe behind it
/// takes seconds and this runs on the key path. Until a list exists the picker holds just the
/// backend's initial choice; `install_models` swaps the real one in when the probe lands.
pub(crate) fn ensure_models(ui: &mut Ui) {
    // Provider Auto has no catalog to discover because the router also chooses the provider.
    // A concrete provider still installs its raw catalog, with any automatic choice overlaid
    // by the composer.
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
        // A catalog landing while provider Auto is selected must not overwrite its single entry.
        // Concrete automatic and explicit selections are preserved by `set_models`.
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

/// `Ctrl+N` — open the triage inbox on needs-input work, then completed work, oldest first
/// within each group.
///
/// The queue is snapshotted here and not rebuilt while the modal is up: the 1s background
/// refresh must not reorder or resize it under the user's fingers mid-answer. An empty queue
/// opens nothing — a footer notice, and the list is untouched.
pub(crate) fn open_triage(ui: &mut Ui) {
    let items = triage_queue(ui.app.sessions());
    if items.is_empty() {
        ui.set_notice("nothing needs triage".to_string());
        return;
    }
    ui.mode = Mode::Triage(TriageState::new(items));
    attach_triage_item(ui);
}

/// `Ctrl+D` in triage runs the same two-stage state machine as pressing list-view `Ctrl+X`
/// twice against this exact session, then advances the queue. An active/needs-input item is
/// stopped first and removed only after that succeeds. Codex implements the completed-session
/// action as a reversible archive; Claude and Grok use their existing row-removal commands.
pub(crate) fn archive_triage_item(ui: &mut Ui) {
    let Some(item) = (match &ui.mode {
        Mode::Triage(state) => state.current().cloned(),
        _ => None,
    }) else {
        return;
    };
    let key = item.key();
    let Some(session) = ui.app.session_for(&key).cloned() else {
        ui.set_notice(format!("{} is no longer listed", item.title));
        return;
    };
    // A prefetched/current Claude or Grok item owns a live attach client. Their remove
    // operations can refuse or race while that client is still resident, unlike Codex archive.
    // Drop it before the mutation worker performs its fresh authoritative listing and remove.
    release_triage_attachment(ui, Some(key));
    let now = now_ms();
    let should_stop = !session.status.is_finished();
    for _ in 0..2 {
        let stage = ui
            .app
            .kill_stage_for(session.backend, session.id.clone(), should_stop, now);
        kill_request(
            ui,
            TargetRequest::from(&session),
            session.title.clone(),
            stage,
        );
    }
    skip_triage_item(ui);
}

/// Put the item under the cursor live in the panel, using the SAME attach the list's Enter
/// uses — a resolved backend command in a `PtySession`, cached in `ui.attached` and keyed by
/// session. Triage invents no second way to reach a session, so whatever attach semantics a
/// backend has (claude resumes the same thread; codex goes through the app-server daemon)
/// are exactly what triage inherits.
///
/// Attach resolution is off-thread, so this only submits; `install_attach_plan` lands the
/// child. A session that is somehow already connected (the wall holds a tile for it) is
/// reused rather than respawned. Only the immediate next item is prefetched, and leaving that
/// two-item window closes its child, so walking a queue never accumulates live connections.
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
        preload_triage_next(ui);
        return;
    }
    submit_attach(ui, TargetRequest::from(&session));
    preload_triage_next(ui);
}

/// Resolve and start exactly the next triage item while the current one is on screen. The
/// result lands as a hidden PTY, then becomes visible immediately when the user advances.
pub(crate) fn preload_triage_next(ui: &mut Ui) {
    let Some(item) = (match &ui.mode {
        Mode::Triage(state) => state.next().cloned(),
        _ => None,
    }) else {
        return;
    };
    let key = item.key();
    if ui.attached.contains_key(&key) {
        return;
    }
    let Some(session) = ui.app.session_for(&key).cloned() else {
        return;
    };
    let executor = ui.attach_executor.clone();
    let runner_key = format!("triage-preload:{}:{}", key.0.name(), key.1);
    ui.attaches.submit(runner_key, move || {
        Ok(crate::AttachOutcome::TriagePrefetch {
            key,
            plan: executor(TargetRequest::from(&session)),
        })
    });
}

/// `Ctrl+N` inside the modal — step to the next item; running off the end closes the modal.
pub(crate) fn skip_triage_item(ui: &mut Ui) {
    if !matches!(ui.mode, Mode::Triage(_)) {
        return;
    }
    let Some(leaving) = triage_step(ui, TriageState::advance) else {
        close_triage(ui);
        return;
    };
    release_triage_attachment(ui, leaving);
    attach_triage_item(ui);
}

/// `Ctrl+P` inside the modal — step back to the previous item. A no-op on the first.
pub(crate) fn back_triage_item(ui: &mut Ui) {
    let Some(leaving) = triage_step(ui, TriageState::back) else {
        return;
    };
    release_triage_attachment(ui, leaving);
    attach_triage_item(ui);
}

/// Move the queue cursor with `step`, returning the item it left when it actually moved.
/// `None` means the queue did not move (already at an end).
fn triage_step(ui: &mut Ui, step: fn(&mut TriageState) -> bool) -> Option<Option<Key>> {
    let Mode::Triage(state) = &mut ui.mode else {
        return None;
    };
    let leaving = state.current().map(|item| item.key());
    step(state).then_some(leaving)
}

/// Close the child of an item that just went off screen. A triage visit is exactly as long as
/// the item is in the panel: keeping every visited child alive accumulates invisible processes
/// and reader threads across a long queue, and a retained codex resume client keeps a finished
/// session reading idle instead of done.
///
/// A wall tile is the one exception, as it is everywhere else: the wall owns that connection
/// and closes it when it closes.
fn release_triage_attachment(ui: &mut Ui, key: Option<Key>) {
    let Some(key) = key else {
        return;
    };
    if ui.wall.owns(&key) {
        return;
    }
    ui.remove_pty(&key);
}

/// Leave the queue for the list, closing the child that was in the panel. The session itself
/// keeps running — detaching has never meant stopping — but nothing stays connected once it is
/// off screen, exactly as the attach view and the wall behave.
pub(crate) fn close_triage(ui: &mut Ui) {
    let preload = match &ui.mode {
        Mode::Triage(state) => state.next().map(|item| item.key()),
        _ => None,
    };
    let showing = ui.focused.take();
    release_triage_attachment(ui, showing);
    release_triage_attachment(ui, preload);
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
/// immediate "<verb>… <title>" notice. A duplicate keypress while the first is still pending
/// says so rather than looking like a dead key.
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
    } else {
        ui.set_notice(format!("still {verb} {title}"));
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
        // The failure rides INSIDE the outcome rather than surfacing as a runner-level `Err`,
        // so it carries the key it was submitted for and the landing guard can drop a failure
        // whose row the user has already walked off - exactly as the wall's joins do.
        Ok(crate::AttachOutcome::Focus {
            key: outcome_key,
            triage,
            plan: executor(request),
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
    let capture_on_attach = matches!(
        session.backend,
        BackendKind::Codex | BackendKind::Claude | BackendKind::Grok
    );
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
        // Triage scrolls the viewer-owned viewport for every backend. Full-screen Claude and
        // Grok can delegate wheel input to their child, but the modal cannot rely on that
        // protocol, so its PTY must retain history just like Codex does.
        if triage || session.backend == BackendKind::Codex {
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
    // Every concrete backend scrolls immediately.
    set_mouse_capture(ui, capture_on_attach);
    Ok(true)
}

/// Spawn the composed task into the current spawn target, record it for pinning, and
/// clear the composer. The spawn itself is detached (fast); only its record persists.
///
/// Returns whether the task was actually submitted. Every refusal below leaves the composer
/// holding the draft, and the wall's overlay stays open on a `false` rather than dropping the
/// user back onto the grid with text they can no longer see.
pub(crate) fn spawn_from_composer(
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
) -> bool {
    // Defense-in-depth: never spawn the /model meta-command as a task (Enter routing already
    // avoids this, but keep the spawn path safe).
    if ui.composer.is_model_command() {
        return false;
    }
    let Some(target) = ui.app.spawn_target() else {
        ui.set_notice("no target directory".to_string());
        return false;
    };
    let target_directory = Some(target.displayed_directory().to_path_buf());
    if matches!(ui.composer.text().chars().next(), Some('/' | '$'))
        && command_discovery_pending(ui, &target_directory)
    {
        ui.set_notice("discovering commands…".to_string());
        return false;
    }
    let selected_command = ui.composer.command_for_submission().cloned();
    if let Some(command) = selected_command.as_ref()
        && command.owner() == Some(BackendKind::Codex)
        && command.kind() == agent_viewer_tui::composer::CommandKind::Skill
        && let Some(path) = command.codex_skill_path()
    {
        let Some(backend) = backend_of(backends, BackendKind::Codex) else {
            return false;
        };
        if !backend.capabilities().spawn {
            ui.set_notice("codex does not support spawn".to_string());
            return false;
        }
        let task = ui.composer.text().to_string();
        let model = if ui.composer.is_auto() || ui.composer.model() == "default" {
            None
        } else {
            Some(ui.composer.model().to_string())
        };
        let skill = agent_viewer_core::codex::app_server::CodexSkill {
            name: command.name().to_string(),
            path: path.to_path_buf(),
        };
        let key = format!("{}:{task}:spawn", BackendKind::Codex.name());
        let mutation = Mutation::spawn_codex_skill(
            &ui.app,
            target,
            task,
            model,
            skill,
            now_ms(),
            "spawned on codex skill".to_string(),
        );
        let executor = ui.mutation_executor.clone();
        if !ui.mutations.submit(key, move || executor(mutation)) {
            return false;
        }
        ui.set_notice("spawning… on codex skill".to_string());
        ui.composer.clear();
        refresher.force();
        return true;
    }
    let route = ui.composer.spawn_route(
        ui.composer.backend() == BackendKind::Codex
            && agent_viewer_core::codex::exec_spawn_opt_in(),
    );
    if ui.composer.is_auto() {
        let provider = selected_command.as_ref().and_then(CommandEntry::owner);
        return spawn_through_router(refresher, ui, target, provider, None);
    }
    let backend_kind = ui.composer.backend();
    let Some(backend) = backend_of(backends, backend_kind) else {
        return false;
    };
    if !backend.capabilities().spawn {
        ui.set_notice(format!("{} does not support spawn", backend_kind.name()));
        return false;
    }
    let task = ui.composer.text().to_string();
    // Codex "default" and the shared automatic choice pass no model flag. Owned rather than
    // borrowed so the composer's borrow ends before the routed path takes `ui` mutably.
    let model = {
        let model_str = ui.composer.model();
        (model_str != "default" && model_str != AUTO_MODEL).then(|| model_str.to_string())
    };
    // A named provider still goes through the router, pinned with `--provider`: the backend that
    // runs the job is the one the user picked either way, and routing is what earns it a derived
    // job name and a row in the router's decision log. The direct backend call below is the
    // fallback for a box with no router installed.
    //
    if route == SpawnRoute::Router {
        return spawn_through_router(refresher, ui, target, Some(backend_kind), model);
    }
    let notice = match model.as_deref() {
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
        model,
        now_ms(),
        notice,
    );
    let executor = ui.mutation_executor.clone();
    if !ui.mutations.submit(key, move || executor(mutation)) {
        return false;
    }
    ui.set_notice(format!("spawning… on {}", backend_kind.name()));
    ui.composer.clear();
    // Hasten the next listing so the spawned row (and its bloom) appears promptly; the
    // notice survives until the 1s clear cadence since apply_snapshot preserves it.
    refresher.force();
    true
}

/// Hand the task to `agent-router`, which dispatches it and reports back what it did. No backend
/// is consulted here: on the Auto path (`provider` None) there is no backend to consult until the
/// router classifies the task, and on a pinned route the router runs the chosen one itself.
///
/// `model` rides along only on a pinned route, since `--model` needs an explicit `--provider`;
/// Auto passes none, because the router owns model and effort selection there.
fn spawn_through_router(
    refresher: &Refresher,
    ui: &mut Ui,
    target: SpawnTarget,
    provider: Option<BackendKind>,
    model: Option<String>,
) -> bool {
    let task = ui.composer.text().to_string();
    // Same dedup shape as a backend spawn, so a double Enter cannot route the same task twice
    // while the first router call is still out. Keyed by the pinned provider (or `auto`) so the
    // same task aimed at a different backend is still a distinct submission.
    let key = format!(
        "{}:{task}:spawn",
        provider.map_or("auto", BackendKind::name)
    );
    // No submission timestamp: the router's own return is what stamps the routed spawn, since
    // classification plus dispatch can outlive the 30s row-matching window from here.
    let mutation = Mutation::spawn_routed(&ui.app, target, task, provider, model);
    let executor = ui.mutation_executor.clone();
    if !ui.mutations.submit(key, move || executor(mutation)) {
        return false;
    }
    ui.set_notice(match provider {
        Some(kind) => format!("routing… via agent-router on {}", kind.name()),
        None => "routing… via agent-router".to_string(),
    });
    ui.composer.clear();
    refresher.force();
    true
}

#[cfg(test)]
mod tests {
    use super::{
        hide_request, install_attach_plan, install_commands, kill_request, spawn_from_composer,
    };
    use crate::Refresher;
    use crate::keys::handle_paste;
    use crate::keys::tests::{SpawnBackend, sess, test_ui_with};
    use crate::ops::{Mutation, resolve_attach_with_backend};
    use agent_viewer_core::codex::app_server::CodexSkill;
    use agent_viewer_core::pty::TerminalPalette;
    use agent_viewer_core::{AttachRefusal, BackendKind, Capabilities, Session, Status};
    use agent_viewer_tui::app::KillStage;
    use agent_viewer_tui::composer::CommandEntry;
    use agent_viewer_tui::mutations::MutationOutcome;
    use agent_viewer_tui::shared_listing::TargetRequest;
    use agent_viewer_tui::ui::{ATTACHED_CHROME_ROWS, Mode};
    use std::path::PathBuf;
    use std::sync::{
        Arc, Mutex,
        mpsc::{TryRecvError, channel},
    };
    use std::time::{Duration, Instant};

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

    struct CommandSpawnBackend {
        kind: BackendKind,
    }

    impl agent_viewer_core::Backend for CommandSpawnBackend {
        fn kind(&self) -> BackendKind {
            self.kind
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                spawn: true,
                ..Capabilities::none()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            unreachable!("listing is not exercised by command submission")
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("the mutation recorder intercepts command submission")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, AttachRefusal> {
            unreachable!("attach is not exercised by command submission")
        }
    }

    fn select_auto_command(ui: &mut crate::Ui, command: CommandEntry, typed_prefix: &str) {
        ui.composer.set_auto_available(true);
        ui.composer.default_to_auto();
        ui.composer
            .set_commands(vec![command], (BackendKind::Claude, None));
        ui.composer.push_str(typed_prefix);
        assert!(ui.composer.accept_suggestion());
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

    /// A second press while the first archive is still out is deduplicated, and the footer has
    /// to say so: a silent no-op reads as a dead key, and it is also the only symptom a row
    /// whose worker died would ever show.
    #[test]
    fn a_repeated_archive_while_one_is_pending_reports_that_it_is_still_working() {
        let session = sess("dedup_hide", "/tmp/agentviewer_dedup_hide", 100);
        let request = TargetRequest::from(&session);
        let mut ui = test_ui_with(vec![session]);
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        ui.mutation_executor = Arc::new(move |mutation| {
            let Mutation::Hide(_) = mutation else {
                panic!("archive must only ever hide");
            };
            started_tx.send(()).expect("report archive start");
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .expect("release archive");
            Ok(MutationOutcome {
                notice: "archived".to_string(),
                spawned: None,
            })
        });

        hide_request(&mut ui, request.clone(), "dedup hide".to_string(), true);
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("archive started");
        assert_eq!(ui.notice.text, "archiving… dedup hide");

        hide_request(&mut ui, request, "dedup hide".to_string(), true);

        assert_eq!(ui.notice.text, "still archiving dedup hide");
        release_tx.send(()).expect("finish archive");
        assert_eq!(
            poll_mutation(&mut ui),
            Ok(MutationOutcome {
                notice: "archived".to_string(),
                spawned: None,
            })
        );
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
            vec![Box::new(SpawnBackend { spawn: true })];
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
            let Mutation::SpawnRouted {
                task,
                provider,
                model,
                preexisting_ids,
                ..
            } = mutation
            else {
                panic!("an auto submission must never reach a backend spawn");
            };
            // Auto pins nothing: the router classifies, and `--model` is refused without an
            // explicit `--provider` anyway.
            assert_eq!(provider, None);
            assert_eq!(model, None);
            // The row selection needs the winning provider's preexisting ids, and which
            // provider wins is unknown until the router answers, so both are captured.
            assert!(preexisting_ids.contains_key(&BackendKind::Claude));
            assert!(preexisting_ids.contains_key(&BackendKind::Codex));
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
    fn owned_slash_entries_under_auto_route_with_their_selected_provider() {
        for (command, prefix, provider) in [
            (
                CommandEntry::claude_skill("deploy"),
                "/de",
                BackendKind::Claude,
            ),
            (
                CommandEntry::codex_prompt("review"),
                "/re",
                BackendKind::Codex,
            ),
        ] {
            let routed = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&routed);
            let mut ui = test_ui_with(vec![sess(
                "router_target",
                "/tmp/agentviewer_owned_command",
                100,
            )]);
            ui.mutation_executor = Arc::new(move |mutation| {
                let Mutation::SpawnRouted {
                    task,
                    provider,
                    model,
                    ..
                } = mutation
                else {
                    panic!("an owned slash entry must use the routed spawn")
                };
                recorded.lock().unwrap().push((task, provider, model));
                Ok(MutationOutcome {
                    notice: "recorded".to_string(),
                    spawned: None,
                })
            });
            select_auto_command(&mut ui, command.clone(), prefix);
            ui.composer.push_str("ship it");

            assert!(spawn_from_composer(&[], &inert_refresher(), &mut ui));
            assert_eq!(poll_mutation(&mut ui).unwrap().notice, "recorded");
            assert_eq!(
                *routed.lock().unwrap(),
                vec![(
                    format!("{}ship it", command.insertion()),
                    Some(provider),
                    None,
                )]
            );
        }
    }

    #[test]
    fn manual_auto_slash_inference_routes_only_an_unambiguous_owner() {
        for (commands, provider) in [
            (
                vec![CommandEntry::claude_skill("implement")],
                Some(BackendKind::Claude),
            ),
            (
                vec![
                    CommandEntry::claude_skill("review"),
                    CommandEntry::codex_prompt("review"),
                ],
                None,
            ),
        ] {
            let routed = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&routed);
            let mut ui = test_ui_with(vec![sess(
                "router_target",
                "/tmp/agentviewer_manual_command",
                100,
            )]);
            ui.mutation_executor = Arc::new(move |mutation| {
                let Mutation::SpawnRouted { task, provider, .. } = mutation else {
                    panic!("manual slash insertion must use the routed spawn")
                };
                recorded.lock().unwrap().push((task, provider));
                Ok(MutationOutcome {
                    notice: "recorded".to_string(),
                    spawned: None,
                })
            });
            ui.composer.set_auto_available(true);
            ui.composer.default_to_auto();
            ui.composer
                .set_commands(commands, (BackendKind::Claude, None));
            let task = if provider.is_some() {
                "/implement fix it"
            } else {
                "/review this"
            };
            ui.composer.push_str(task);
            assert_eq!(ui.composer.pinned_command(), None);

            assert!(spawn_from_composer(&[], &inert_refresher(), &mut ui));
            assert_eq!(poll_mutation(&mut ui).unwrap().notice, "recorded");
            assert_eq!(*routed.lock().unwrap(), vec![(task.to_string(), provider)]);
        }
    }

    #[test]
    fn a_codex_skill_uses_a_direct_structured_mutation_from_auto_and_codex() {
        let skill_path = PathBuf::from("/tmp/agentviewer_codex_skill/SKILL.md");
        let expected_skill = CodexSkill {
            name: "diagnose".to_string(),
            path: skill_path.clone(),
        };

        for auto in [true, false] {
            let direct = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&direct);
            let mut ui = test_ui_with(vec![sess(
                "router_target",
                "/tmp/agentviewer_codex_skill_target",
                100,
            )]);
            ui.mutation_executor = Arc::new(move |mutation| {
                let Mutation::Spawn {
                    backend,
                    task,
                    codex_skill,
                    ..
                } = mutation
                else {
                    panic!("a Codex skill must bypass the routed spawn")
                };
                recorded.lock().unwrap().push((backend, task, codex_skill));
                Ok(MutationOutcome {
                    notice: "recorded".to_string(),
                    spawned: None,
                })
            });
            ui.composer.set_auto_available(true);
            if auto {
                ui.composer.default_to_auto();
            } else {
                ui.composer.select_backend(BackendKind::Codex);
            }
            let command = CommandEntry::codex_skill("diagnose", skill_path.clone());
            ui.composer
                .set_commands(vec![command], (BackendKind::Codex, None));
            ui.composer.push_str("$di");
            assert!(ui.composer.accept_suggestion());
            ui.composer.push_str("investigate this");

            let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
                vec![Box::new(CommandSpawnBackend {
                    kind: BackendKind::Codex,
                })];
            assert!(spawn_from_composer(&backends, &inert_refresher(), &mut ui));
            assert_eq!(poll_mutation(&mut ui).unwrap().notice, "recorded");
            assert_eq!(
                *direct.lock().unwrap(),
                vec![(
                    BackendKind::Codex,
                    "$diagnose investigate this".to_string(),
                    Some(expected_skill.clone()),
                )],
                "the structured skill must survive the {auto} Auto state"
            );
        }
    }

    #[test]
    fn delayed_command_discovery_blocks_command_text_but_not_ordinary_text() {
        let target_directory = PathBuf::from("/tmp/agentviewer_delayed_commands");
        let skill_path = PathBuf::from("/skills/diagnose/SKILL.md");
        let mut ui = test_ui_with(vec![sess(
            "delayed_target",
            target_directory.to_str().expect("utf8 target"),
            100,
        )]);
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&submitted);
        ui.mutation_executor = Arc::new(move |mutation| {
            match mutation {
                Mutation::SpawnRouted { task, .. } => {
                    recorded.lock().unwrap().push((task, None));
                }
                Mutation::Spawn {
                    task, codex_skill, ..
                } => {
                    recorded.lock().unwrap().push((task, codex_skill));
                }
                _ => panic!("unexpected mutation"),
            }
            Ok(MutationOutcome {
                notice: "recorded".to_string(),
                spawned: None,
            })
        });
        ui.composer.set_auto_available(true);
        ui.composer.default_to_auto();

        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let discovered_path = skill_path.clone();
        let discovery_key = (BackendKind::Codex, Some(target_directory));
        assert!(ui.models.request_commands_with_codex_discovery(
            discovery_key.clone(),
            move |_| {
                started_tx.send(()).expect("record discovery start");
                release_rx.recv().expect("release discovery");
                vec![CodexSkill {
                    name: "diagnose".to_string(),
                    path: discovered_path,
                }]
            },
        ));
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("discovery starts");
        assert!(ui.models.commands_pending(&discovery_key));

        ui.composer.push_str("$diagnose investigate");
        assert!(!spawn_from_composer(&[], &inert_refresher(), &mut ui));
        assert_eq!(ui.composer.text(), "$diagnose investigate");
        assert!(submitted.lock().unwrap().is_empty());

        ui.composer.clear();
        ui.composer.push_str("ordinary text");
        assert!(spawn_from_composer(&[], &inert_refresher(), &mut ui));
        assert_eq!(poll_mutation(&mut ui).unwrap().notice, "recorded");
        assert_eq!(submitted.lock().unwrap()[0].0, "ordinary text");

        release_tx.send(()).expect("release discovery");
        let deadline = Instant::now() + Duration::from_secs(1);
        while ui.models.commands_pending(&discovery_key) {
            install_commands(&mut ui);
            assert!(Instant::now() < deadline, "discovery did not land");
            std::thread::yield_now();
        }
        install_commands(&mut ui);
        ui.composer.push_str("$diagnose investigate");

        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(CommandSpawnBackend {
                kind: BackendKind::Codex,
            })];
        assert!(spawn_from_composer(&backends, &inert_refresher(), &mut ui));
        assert_eq!(poll_mutation(&mut ui).unwrap().notice, "recorded");
        let submissions = submitted.lock().unwrap();
        assert_eq!(submissions.len(), 2);
        assert_eq!(
            submissions[1],
            (
                "$diagnose investigate".to_string(),
                Some(CodexSkill {
                    name: "diagnose".to_string(),
                    path: skill_path,
                }),
            )
        );
    }

    /// A concrete backend is a choice of provider, not a choice to bypass the router: the job
    /// still routes, pinned with that provider and the composer's model, so it gets the router's
    /// derived name and its decision-log row like every other spawn.
    #[test]
    fn a_named_backend_submission_routes_with_the_provider_pinned() {
        let pinned = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&pinned);
        let mut ui = test_ui_with(vec![sess("router_target", "/tmp/agentviewer_pinned", 100)]);
        ui.mutation_executor = Arc::new(move |mutation| {
            let Mutation::SpawnRouted {
                task,
                provider,
                model,
                ..
            } = mutation
            else {
                panic!("with a router installed a named backend must still route");
            };
            recorded.lock().unwrap().push((task, provider, model));
            Ok(MutationOutcome {
                notice: "spawned on claude opus[1m] job Add A Test (codex weekly 3%, claude 47%)"
                    .to_string(),
                spawned: None,
            })
        });
        ui.composer.set_auto_available(true);
        // Sitting on the concrete backend, not Auto: this is the case that used to bypass the
        // router entirely and call the backend's own spawn.
        assert!(!ui.composer.is_auto());
        ui.composer
            .set_models(vec!["opus[1m]".to_string()], BackendKind::Claude);
        ui.composer.cycle_model();
        assert_eq!(ui.composer.model(), "opus[1m]");
        ui.composer.push_str("route this on claude");

        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(SpawnBackend { spawn: true })];
        spawn_from_composer(&backends, &inert_refresher(), &mut ui);

        let deadline = Instant::now() + Duration::from_secs(1);
        while ui.mutations.poll().is_none() {
            assert!(Instant::now() < deadline, "pinned mutation did not finish");
            std::thread::yield_now();
        }
        assert_eq!(
            *pinned.lock().unwrap(),
            vec![(
                "route this on claude".to_string(),
                Some(BackendKind::Claude),
                Some("opus[1m]".to_string()),
            )]
        );
    }

    #[test]
    fn a_named_backend_defaults_to_router_model() {
        let pinned = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&pinned);
        let mut ui = test_ui_with(vec![sess(
            "router_target",
            "/tmp/agentviewer_pinned_auto",
            100,
        )]);
        ui.mutation_executor = Arc::new(move |mutation| {
            let Mutation::SpawnRouted {
                task,
                provider,
                model,
                ..
            } = mutation
            else {
                panic!("a concrete automatic provider must route");
            };
            recorded.lock().unwrap().push((task, provider, model));
            Ok(MutationOutcome {
                notice: "spawned on claude".to_string(),
                spawned: None,
            })
        });
        ui.composer.set_auto_available(true);
        ui.composer
            .set_models(vec!["opus[1m]".to_string()], BackendKind::Claude);
        ui.composer.push_str("route this on automatic claude");

        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(SpawnBackend { spawn: true })];
        spawn_from_composer(&backends, &inert_refresher(), &mut ui);

        let deadline = Instant::now() + Duration::from_secs(1);
        while ui.mutations.poll().is_none() {
            assert!(
                Instant::now() < deadline,
                "automatic provider mutation did not finish"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            *pinned.lock().unwrap(),
            vec![(
                "route this on automatic claude".to_string(),
                Some(BackendKind::Claude),
                None,
            )]
        );
    }

    /// Without a router on PATH the viewer must still spawn: the backend's own path is the
    /// fallback, not an error, matching the backends-appear-when-present posture.
    #[test]
    fn a_named_backend_submission_falls_back_to_the_backend_without_a_router() {
        let direct = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&direct);
        let mut ui = test_ui_with(vec![sess(
            "router_target",
            "/tmp/agentviewer_norouter",
            100,
        )]);
        ui.mutation_executor = Arc::new(move |mutation| {
            let Mutation::Spawn { task, backend, .. } = mutation else {
                panic!("without a router a named backend must spawn directly");
            };
            recorded.lock().unwrap().push((task, backend));
            Ok(MutationOutcome {
                notice: "spawned on claude".to_string(),
                spawned: None,
            })
        });
        ui.composer.set_auto_available(false);
        ui.composer.push_str("spawn this directly");

        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(SpawnBackend { spawn: true })];
        spawn_from_composer(&backends, &inert_refresher(), &mut ui);

        let deadline = Instant::now() + Duration::from_secs(1);
        while ui.mutations.poll().is_none() {
            assert!(Instant::now() < deadline, "direct mutation did not finish");
            std::thread::yield_now();
        }
        assert_eq!(
            *direct.lock().unwrap(),
            vec![("spawn this directly".to_string(), BackendKind::Claude)]
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

    /// The failure a landed focus attach carries, if any. A resolution failure rides inside
    /// the outcome (keyed, so the landing guard can drop a stale one), not as a runner `Err`.
    fn focus_error(result: &Result<crate::AttachOutcome, String>) -> Option<&str> {
        match result {
            Ok(crate::AttachOutcome::Focus { plan, .. }) => plan.as_ref().err().map(String::as_str),
            Ok(crate::AttachOutcome::Wall { .. }) => panic!("expected a focus attach"),
            Ok(crate::AttachOutcome::TriagePrefetch { .. }) => panic!("expected a focus attach"),
            Err(error) => Some(error.as_str()),
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
        assert_eq!(focus_error(&result), Some("fresh authority refused attach"));
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
            focus_error(&missing),
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

    /// A row activation remains intentional while the backend resolves. Moving the list cursor
    /// must not cancel the original request or redirect it to the row now under the cursor.
    #[test]
    fn a_landed_focus_attach_opens_its_original_row_after_the_selection_moves() {
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

        let key = (BackendKind::Claude, first.id.clone());
        assert!(ui.attached.contains_key(&key));
        assert_eq!(ui.focused.as_ref(), Some(&key));
        assert!(matches!(ui.mode, Mode::Attached));
        ui.attached.get_mut(&key).expect("attached child").kill();
    }

    /// The control for the error-path drop test below: a FAILED resolution that lands while
    /// its row is still the selected one is still the user's answer, so it is shown.
    #[test]
    fn a_landed_attach_failure_is_shown_while_its_row_is_still_selected() {
        let first = sess("failing_selected", "/tmp/agentviewer_failing_selected", 100);
        let other = sess("elsewhere_ok", "/tmp/agentviewer_elsewhere_ok", 200);
        let mut ui = test_ui_with(vec![first.clone(), other]);
        ui.attach_executor = Arc::new(|_| Err("codex session is no longer available".to_string()));
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Claude, first.id.clone()))
        );

        assert!(submit_attach(&mut ui, TargetRequest::from(&first)));
        let result = poll_attach(&mut ui);
        land_attach(&mut ui, result);

        assert_eq!(ui.notice.text, "codex session is no longer available");
        assert!(ui.attached.is_empty());
        assert!(matches!(ui.mode, Mode::Normal));
    }

    /// An activation remains intentional even when the cursor moves, so its failure remains
    /// visible instead of being reported as a cancelled request.
    #[test]
    fn a_landed_attach_failure_is_shown_when_the_selection_moves_on() {
        let first = sess("failing_left", "/tmp/agentviewer_failing_left", 100);
        let second = sess("failing_moved", "/tmp/agentviewer_failing_moved", 200);
        let mut ui = test_ui_with(vec![first.clone(), second.clone()]);
        ui.attach_executor = Arc::new(|_| Err("codex session is no longer available".to_string()));
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

        assert_eq!(ui.notice.text, "codex session is no longer available");
        assert!(ui.attached.is_empty());
        assert!(matches!(ui.mode, Mode::Normal));
    }

    /// The same guard for a triage failure: the queue closed, so the failure belongs to a view
    /// the user has already left.
    #[test]
    fn a_landed_triage_attach_failure_is_dropped_after_the_queue_closed() {
        let waiting = blocked("failing_queue", 100);
        let mut ui = test_ui_with(vec![waiting.clone()]);
        ui.attach_executor = Arc::new(|_| Err("codex session is no longer available".to_string()));

        open_triage(&mut ui);
        assert!(matches!(ui.mode, Mode::Triage(_)));
        let result = poll_attach(&mut ui);
        close_triage(&mut ui);
        land_attach(&mut ui, result);

        assert_eq!(
            ui.notice.text,
            "attach cancelled: failing_queue is no longer in focus"
        );
        assert!(ui.attached.is_empty());
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
