//! Key routing for every input mode, plus the actions those keys trigger (attach, spawn,
//! rename, stop/remove, hide). Everything here mutates the shared `Ui` state owned by the
//! run loop in `main.rs`.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Session, Status};
use agent_viewer_tui::app::{Row, Section, file_stems, subdir_names};
use agent_viewer_tui::attach::key_to_bytes;
use agent_viewer_tui::shared_listing::TargetRequest;
use agent_viewer_tui::ui::{
    Mode, PaletteAction, PaletteGroup, PaletteItem, PaletteState, PaletteTarget, SpriteKind,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::actions::{
    activate_selected, apply_rename, attach_selected, ensure_completions, ensure_models,
    hide_request, hide_selected, kill_request, kill_selected, open_filter, open_rename,
    open_rename_request, open_reply, send_reply, spawn_from_composer, submit_attach,
    toggle_group_if_header,
};
use crate::{Refresher, Ui};

const ATTACHED_CODEX_WHEEL_ROWS: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MouseAction {
    None,
    ActivateSelected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MouseTarget {
    StateHeader(Section),
    ProjectHeader(PathBuf),
    Session(BackendKind, String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MousePress {
    target: MouseTarget,
    column: u16,
    row: u16,
}

/// Returns `true` when the app should quit.
pub(crate) fn handle_key<B: ratatui::backend::Backend>(
    key: KeyEvent,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
) -> io::Result<bool> {
    ui.mouse_press = None;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl+C kills the whole viewer (like `claude agents`) from every mode except an active
    // attach — there Ctrl+C must reach the child as an interrupt (0x03) so a runaway agent can
    // be stopped without tearing down the viewer. (macOS Cmd+C is swallowed by the terminal as
    // copy and never reaches us; Ctrl+C is the portable interrupt on macOS and Windows alike.)
    if is_quit_chord(key, ctrl, &ui.mode) {
        ui.attached.clear(); // drop = kill owned children during viewer teardown
        return Ok(true);
    }
    // Ctrl+T toggles mouse capture in every mode. When capture is on, it provides an escape
    // hatch for terminal text selection. The child does not get this chord.
    if is_mouse_toggle_chord(key, ctrl) {
        set_mouse_capture(ui, !ui.mouse_capture);
        return Ok(false);
    }
    match &mut ui.mode {
        Mode::Attached => handle_attached_key(key, ui),
        Mode::Normal => return handle_normal_key(key, ctrl, backends, refresher, ui, terminal),
        Mode::Palette(_) => {
            handle_palette_key(key, backends, ui, terminal)?;
        }
        Mode::Filter => handle_filter_key(key.code, ui),
        Mode::Help => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                ui.mode = Mode::Normal;
            }
        }
        Mode::Rename(_) => handle_rename_key(key.code, ui),
        Mode::Reply(_) => handle_reply_key(key.code, backends, ui, terminal)?,
    }
    Ok(false)
}

/// Route a mouse event. While attached, Codex wheel events scroll the local viewport because
/// Codex discards native wheel reports. Other Codex pointer events and all Claude and external
/// opencode events stay native. In the list, click or hover selects the row under the cursor
/// and the wheel walks the selection using geometry recorded by the last draw. Modals own
/// their surface, so mouse is inert there.
pub(crate) fn handle_mouse(me: MouseEvent, ui: &mut Ui) -> MouseAction {
    // Text-select mode: the terminal owns the mouse, so any report still in flight (or sent
    // by a terminal that ignored the disable sequence) must not steer the selection.
    if !ui.mouse_capture {
        ui.mouse_press = None;
        return MouseAction::None;
    }
    if !matches!(ui.mode, Mode::Normal) {
        ui.mouse_press = None;
    }
    match &ui.mode {
        Mode::Attached => {
            let Some(fkey) = ui.focused.clone() else {
                return MouseAction::None;
            };
            let Some(pty) = ui.attached.get_mut(&fkey) else {
                return MouseAction::None;
            };
            if fkey.0 == BackendKind::Codex {
                match me.kind {
                    MouseEventKind::ScrollUp => {
                        pty.scroll_viewport_up(ATTACHED_CODEX_WHEEL_ROWS);
                        return MouseAction::None;
                    }
                    MouseEventKind::ScrollDown => {
                        pty.scroll_viewport_down(ATTACHED_CODEX_WHEEL_ROWS);
                        return MouseAction::None;
                    }
                    _ => {}
                }
            }
            let (mode, encoding) =
                pty.with_screen(|s| (s.mouse_protocol_mode(), s.mouse_protocol_encoding()));
            // draw_attach draws a one-row header above the child screen, so offset by 1.
            if let Some(bytes) = agent_viewer_tui::mouse::encode_mouse_report(me, mode, encoding, 1)
            {
                let _ = pty.write_input(&bytes);
            }
            MouseAction::None
        }
        Mode::Normal => match me.kind {
            MouseEventKind::Moved => {
                ui.mouse_press = None;
                if let Some(idx) = ui.list_hit.borrow().row_at(me.column, me.row) {
                    ui.app.select_visible_index(idx);
                }
                MouseAction::None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = ui.list_hit.borrow().row_at(me.column, me.row);
                let target = hit.and_then(|idx| mouse_target(ui, idx));
                ui.mouse_press = target.map(|target| MousePress {
                    target,
                    column: me.column,
                    row: me.row,
                });
                if let Some(idx) = hit
                    && ui.mouse_press.is_some()
                {
                    ui.app.select_visible_index(idx);
                }
                MouseAction::None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(pressed) = ui.mouse_press.take() else {
                    return MouseAction::None;
                };
                let hit = ui.list_hit.borrow().row_at(me.column, me.row);
                let released = hit.and_then(|idx| mouse_target(ui, idx));
                if released.as_ref() == Some(&pressed.target) {
                    ui.app
                        .select_visible_index(hit.expect("released target has an index"));
                } else if me.column != pressed.column
                    || me.row != pressed.row
                    || mouse_target(ui, ui.app.selected_index()).as_ref() != Some(&pressed.target)
                {
                    return MouseAction::None;
                }
                MouseAction::ActivateSelected
            }
            // The wheel nudges the selection one selectable row at a time (same as arrows).
            MouseEventKind::ScrollDown => {
                ui.mouse_press = None;
                ui.app.move_selection(1);
                MouseAction::None
            }
            MouseEventKind::ScrollUp => {
                ui.mouse_press = None;
                ui.app.move_selection(-1);
                MouseAction::None
            }
            MouseEventKind::Drag(MouseButton::Left) => MouseAction::None,
            _ => {
                ui.mouse_press = None;
                MouseAction::None
            }
        },
        _ => MouseAction::None,
    }
}

fn mouse_target(ui: &Ui, index: usize) -> Option<MouseTarget> {
    match ui.app.visible().get(index)? {
        Row::SectionHeader { section, .. } => Some(MouseTarget::StateHeader(*section)),
        Row::ProjectHeader { root, .. } => Some(MouseTarget::ProjectHeader(root.clone())),
        Row::Session { backend, id, .. } => Some(MouseTarget::Session(*backend, id.clone())),
        Row::Spacer => None,
    }
}

pub(crate) fn handle_mouse_event<B: ratatui::backend::Backend>(
    me: MouseEvent,
    _backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    _terminal: &mut ratatui::Terminal<B>,
) -> io::Result<()> {
    match handle_mouse(me, ui) {
        MouseAction::None => Ok(()),
        MouseAction::ActivateSelected => {
            activate_selected(ui);
            Ok(())
        }
    }
}

/// Route one terminal paste without interpreting embedded newlines as key presses.
pub(crate) fn handle_paste(text: &str, ui: &mut Ui) {
    match &mut ui.mode {
        Mode::Normal => ui.composer.push_str(text),
        Mode::Attached => {
            ui.pending_reply = None;
            let Some(fkey) = ui.focused.clone() else {
                return;
            };
            let bracketed_paste = ui
                .attached
                .get(&fkey)
                .is_some_and(|pty| pty.with_screen(|screen| screen.bracketed_paste()));
            if let Some(tracker) = ui.detach_trackers.get_mut(&fkey) {
                if bracketed_paste {
                    for _ in text.chars() {
                        tracker.on_char();
                    }
                } else {
                    let mut chars = text.chars().peekable();
                    while let Some(c) = chars.next() {
                        match c {
                            '\r' => {
                                tracker.on_enter();
                                if chars.peek() == Some(&'\n') {
                                    chars.next();
                                }
                            }
                            '\n' => tracker.on_enter(),
                            '\u{8}' | '\u{7f}' => tracker.on_backspace(),
                            _ => tracker.on_char(),
                        }
                    }
                }
            }
            if let Some(pty) = ui.attached.get_mut(&fkey) {
                if bracketed_paste {
                    let mut bytes = b"\x1b[200~".to_vec();
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    let _ = pty.write_input(&bytes);
                } else {
                    let _ = pty.write_input(text.as_bytes());
                }
            }
        }
        Mode::Filter => {
            let mut filter = ui.app.filter().to_string();
            filter.push_str(&single_line_paste(text));
            ui.app.set_filter(filter);
        }
        Mode::Rename(modal) => modal.buffer.push_str(&single_line_paste(text)),
        Mode::Reply(modal) => modal.buffer.push_str(&normalize_paste(text)),
        Mode::Palette(palette) => palette.push_str(&single_line_paste(text)),
        Mode::Help => {}
    }
}

fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn single_line_paste(text: &str) -> String {
    normalize_paste(text).replace('\n', " ")
}

/// Ctrl+T is the app-wide mouse-capture toggle. Pure predicate so the chord is unit-testable
/// without a live terminal; a bare `t` must stay composer text.
fn is_mouse_toggle_chord(key: KeyEvent, ctrl: bool) -> bool {
    ctrl && matches!(key.code, KeyCode::Char('t'))
}

/// Record the requested mouse reporting state and tell the user which mode they are in. The
/// run loop writes the matching terminal mode sequence after the input action returns; keeping
/// this state-only makes the shared attach and detach paths safe to exercise in unit tests.
pub(crate) fn set_mouse_capture(ui: &mut Ui, on: bool) {
    apply_mouse_capture_state(ui, on);
}

/// Write one mouse-reporting mode sequence. This stays separate from state changes so tests
/// can use an in-memory writer rather than changing the invoking terminal.
pub(crate) fn write_mouse_capture<W: io::Write>(writer: &mut W, on: bool) -> io::Result<()> {
    use crossterm::execute;

    if on {
        execute!(writer, crossterm::event::EnableMouseCapture)
    } else {
        execute!(writer, crossterm::event::DisableMouseCapture)
    }
}

/// The state half of `set_mouse_capture`: flip the flag and set the footer notice, with no
/// terminal I/O. Split out so tests can exercise it without writing real mode sequences to
/// the developer's stdout — a test that enabled capture and exited would leave the invoking
/// shell unable to drag-select, which is the very bug this toggle exists to fix.
fn apply_mouse_capture_state(ui: &mut Ui, on: bool) {
    ui.mouse_capture = on;
    ui.mouse_press = None;
    ui.set_notice(
        match (&ui.mode, on) {
            (Mode::Attached, true) => "mouse on: wheel scrolls (ctrl+t to select text)",
            (Mode::Attached, false) => {
                "mouse off: drag to select and copy (ctrl+t to restore scrolling)"
            }
            (_, true) => "mouse on: click/hover selects, wheel scrolls (ctrl+t to select text)",
            (_, false) => "mouse off: drag to select and copy (ctrl+t to restore list mouse)",
        }
        .to_string(),
    );
}

/// Ctrl+G advances the header mascot; the palette picks one directly. Both land here so the
/// choice is announced and persisted the same way.
fn cycle_sprite(ui: &mut Ui) {
    set_sprite(ui, ui.sprite.next());
}

fn set_sprite(ui: &mut Ui, sprite: SpriteKind) {
    ui.sprite = sprite;
    if let Some(db) = &ui.db
        && let Err(error) = db.set_header_sprite(sprite.name())
    {
        ui.set_notice(format!("sprite: {} (not saved: {error})", sprite.name()));
        return;
    }
    ui.set_notice(format!("sprite: {} · ⌃G for the next", sprite.name()));
}

/// Ctrl+C is the app-wide "kill the viewer" chord, except while attached — there it is
/// forwarded to the child as a raw interrupt instead. Kept as a pure predicate so the quit
/// decision is unit-testable without a live terminal.
fn is_quit_chord(key: KeyEvent, ctrl: bool, mode: &Mode) -> bool {
    ctrl && matches!(key.code, KeyCode::Char('c')) && !matches!(mode, Mode::Attached)
}

fn handle_normal_key<B: ratatui::backend::Backend>(
    key: KeyEvent,
    ctrl: bool,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
    _terminal: &mut ratatui::Terminal<B>,
) -> io::Result<bool> {
    // Refresh the slash-command list up front (keyed on backend+target, so a no-op unless
    // they changed) BEFORE anything reads `suggestions_active` — otherwise a Ctrl+S regroup
    // or a background snapshot that moved the selected session/target could leave `suggesting`
    // (and a subsequent Tab accept) reading commands scanned for the PREVIOUS target.
    ensure_completions(ui);
    ensure_models(ui);

    // Ctrl-chords always act, regardless of composer state.
    if ctrl {
        match key.code {
            KeyCode::Char('a') => ui.app.toggle_show_all(),
            KeyCode::Char('d') => hide_selected(backends, ui, true),
            KeyCode::Char('u') => hide_selected(backends, ui, false),
            KeyCode::Char('s') => ui.app.toggle_group_mode(),
            KeyCode::Char('r') => open_rename(backends, ui),
            KeyCode::Char('e') => open_reply(backends, ui),
            KeyCode::Char('x') => kill_selected(backends, ui),
            KeyCode::Char('f') => open_filter(ui),
            KeyCode::Char('k') => open_palette(backends, ui),
            KeyCode::Char('g') => cycle_sprite(ui),
            _ => {}
        }
        return Ok(false);
    }

    // While either popup (slash-command or /model picker) is open, Up/Down/Tab/Esc drive it
    // instead of the list. For /model the guard is `is_model_command()` (not `model_picking`):
    // an active `/model <no-match>` command must still capture these keys, otherwise Tab would
    // cycle the backend and Up/Down would move the session selection mid-command. The
    // underlying composer ops (`move_suggestion`, `accept_model`) are safe no-ops when the
    // active list is empty.
    let suggesting = ui.composer.suggestions_active();
    let model_cmd = ui.composer.is_model_command();
    let theme_cmd = ui.composer.is_theme_command();
    match key.code {
        KeyCode::Down if theme_cmd => ui.themes.move_preview(1),
        KeyCode::Up if theme_cmd => ui.themes.move_preview(-1),
        KeyCode::Down if suggesting || model_cmd => ui.composer.move_suggestion(1),
        KeyCode::Up if suggesting || model_cmd => ui.composer.move_suggestion(-1),
        // Arrows navigate or act at all times.
        KeyCode::Down => ui.app.move_selection(1),
        KeyCode::Up => ui.app.move_selection(-1),
        KeyCode::Right => {
            attach_selected(ui);
        }
        // Tab accepts the highlighted suggestion/model while a popup is open, else cycles the
        // target backend; Shift+Tab cycles that backend's model.
        KeyCode::Tab if suggesting => {
            ui.composer.accept_suggestion();
        }
        KeyCode::Tab if model_cmd => {
            // No-op when there is nothing to pick (a `/model <no-match>` command), but still
            // captures Tab so it does not fall through to `cycle_backend`.
            ui.composer.accept_model();
        }
        KeyCode::Tab if theme_cmd => {}
        KeyCode::Tab => ui.composer.cycle_backend(),
        KeyCode::BackTab => ui.composer.cycle_model(),
        KeyCode::Backspace => ui.composer.backspace(),
        // Esc dismisses an open popup first; a second Esc clears the composer as before.
        KeyCode::Esc if theme_cmd => {
            ui.themes.cancel_picker();
            ui.composer.clear();
        }
        KeyCode::Esc if suggesting => ui.composer.dismiss_suggestions(),
        KeyCode::Esc => ui.composer.clear(),
        KeyCode::Enter => {
            if theme_cmd {
                let theme_id = ui.themes.commit_picker().to_string();
                if let Some(db) = &ui.db
                    && let Err(error) = agent_viewer_tui::ui::theme::persist_theme(db, &theme_id)
                {
                    ui.set_notice(format!("could not persist theme: {error}"));
                }
                ui.composer.clear();
            } else if ui.composer.model_picking() {
                // A /model picker is up: Enter picks the highlighted model.
                ui.composer.accept_model();
            } else if ui.composer.is_model_command() {
                // A /model command with no matches: a meta-command, nothing to spawn.
            } else if ui.composer.is_empty() {
                // On a group header, Enter collapses/expands the group (and persists) instead
                // of attaching; on a session it attaches as before.
                activate_selected(ui);
            } else {
                spawn_from_composer(backends, refresher, ui);
            }
        }
        KeyCode::Char(c) => {
            if ui.composer.is_empty() {
                // Empty composer: punctuation hotkeys still fire; every bare letter and
                // number starts a task.
                match c {
                    '?' => ui.mode = Mode::Help,
                    // On a header, Space toggles the group and persists it. On a session it
                    // does nothing. With no selected session or header it is composer text.
                    ' ' => {
                        if !toggle_group_if_header(ui) && ui.app.selected().is_none() {
                            ui.composer.push_char(' ');
                        }
                    }
                    // '/' is no longer a filter hotkey — it types into the composer so a
                    // slash command like `/implement RS-123` spawns as the task prompt.
                    _ => ui.composer.push_char(c),
                }
            } else {
                // Non-empty composer: every printable (and space) is task text.
                ui.composer.push_char(c);
            }
        }
        _ => {}
    }
    if ui.composer.is_theme_command() {
        ui.themes.open_picker();
    } else if ui.themes.picker_open() {
        ui.themes.cancel_picker();
    }
    // Refresh the slash-command list for the (possibly new) backend/target and text.
    ensure_completions(ui);
    ensure_models(ui);
    Ok(false)
}

fn open_palette(backends: &[Box<dyn Backend>], ui: &mut Ui) {
    let items = palette_items(backends, ui);
    ui.mode = Mode::Palette(PaletteState::new(items));
}

fn palette_items(backends: &[Box<dyn Backend>], ui: &Ui) -> Vec<PaletteItem> {
    let selected = ui.app.selected();
    let capabilities = selected
        .and_then(|session| {
            backend_of(backends, session.backend).map(|b| b.capabilities_for(session))
        })
        .unwrap_or_else(Capabilities::none);
    let mut items = palette_action_items(selected, capabilities);
    items.extend(SpriteKind::ALL.into_iter().map(|sprite| {
        let active = sprite == ui.sprite;
        PaletteItem::new(
            PaletteGroup::Sprites,
            if active { "◉" } else { "○" },
            sprite.name(),
            if active {
                format!("{} · showing now", sprite.detail())
            } else {
                sprite.detail().to_string()
            },
            (sprite == ui.sprite.next()).then_some("⌃G"),
            true,
            None,
            PaletteTarget::Sprite(sprite),
        )
    }));

    let mut seen = HashSet::new();
    let mut sessions = ui
        .app
        .visible()
        .iter()
        .filter_map(|row| {
            let Row::Session {
                backend,
                id,
                updated_at_ms,
                ..
            } = row
            else {
                return None;
            };
            let key = (*backend, id.clone());
            if !seen.insert(key.clone()) {
                return None;
            }
            let session = ui.app.session_for(&key)?;
            Some((*updated_at_ms, palette_session_item(session)))
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|(left_time, left), (right_time, right)| {
        right_time
            .cmp(left_time)
            .then_with(|| left.name.cmp(&right.name))
    });
    items.extend(sessions.into_iter().map(|(_, item)| item));

    for backend in [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Opencode,
    ] {
        let mut models = ui
            .models
            .models(backend)
            .map(<[String]>::to_vec)
            .unwrap_or_else(|| vec![backend.default_model().to_string()]);
        if backend == ui.composer.backend()
            && !models.iter().any(|model| model == ui.composer.model())
        {
            models.push(ui.composer.model().to_string());
        }
        models.sort();
        models.dedup();
        items.extend(models.into_iter().map(|name| {
            PaletteItem::new(
                PaletteGroup::Models,
                "◇",
                name.clone(),
                format!("{} · set composer model", backend.name()),
                None,
                true,
                None,
                PaletteTarget::Model { backend, name },
            )
        }));
    }

    let spawn_target = ui.app.spawn_target();
    items.extend(
        palette_commands(
            ui.composer.backend(),
            spawn_target
                .as_ref()
                .map(|target| target.displayed_directory()),
        )
        .into_iter()
        .map(|command| {
            PaletteItem::new(
                PaletteGroup::Commands,
                "/",
                format!("/{command}"),
                format!("{} slash command", ui.composer.backend().name()),
                None,
                true,
                None,
                PaletteTarget::Command(command),
            )
        }),
    );
    items
}

fn palette_action_items(
    selected: Option<&Session>,
    capabilities: Capabilities,
) -> Vec<PaletteItem> {
    [
        action_item(
            PaletteAction::Attach,
            "Attach session",
            "open the selected session",
            Some("⏎"),
            capability_reason(selected, capabilities.attach, "attach"),
        ),
        action_item(
            PaletteAction::Archive,
            "Archive session",
            "hide the selected session",
            Some("⌃D"),
            selected
                .filter(|session| session.hidden)
                .map(|_| "unavailable · session is already archived".to_string())
                .or_else(|| capability_reason(selected, capabilities.archive, "archive")),
        ),
        action_item(
            PaletteAction::Unarchive,
            "Unarchive session",
            "restore the selected session",
            Some("⌃U"),
            selected
                .filter(|session| !session.hidden)
                .map(|_| "unavailable · session is not archived".to_string())
                .or_else(|| capability_reason(selected, capabilities.archive, "unarchive")),
        ),
        action_item(
            PaletteAction::Rename,
            "Rename session",
            "rename the selected session",
            Some("⌃R"),
            capability_reason(selected, capabilities.rename, "rename"),
        ),
        action_item(
            PaletteAction::Reply,
            "Reply to session",
            "answer a session that needs input",
            Some("⌃E"),
            Some("unavailable · reply is not supported".to_string()),
        ),
        action_item(
            PaletteAction::StopOrRemove,
            "Stop or remove session",
            "stop once, remove on the next press",
            Some("⌃X"),
            capability_reason(
                selected,
                capabilities.stop || capabilities.delete,
                "stop or remove",
            ),
        ),
        action_item(
            PaletteAction::ShowAll,
            "Show all sessions",
            "toggle archived and companion sessions",
            Some("⌃A"),
            None,
        ),
        action_item(
            PaletteAction::Group,
            "Group sessions",
            "toggle state and project grouping",
            Some("⌃S"),
            None,
        ),
        action_item(
            PaletteAction::Filter,
            "Filter sessions",
            "open the list filter",
            Some("⌃F"),
            None,
        ),
    ]
    .into_iter()
    .collect()
}

fn action_item(
    action: PaletteAction,
    name: &str,
    detail: &str,
    key_hint: Option<&str>,
    disabled_reason: Option<String>,
) -> PaletteItem {
    PaletteItem::new(
        PaletteGroup::Actions,
        "▸",
        name,
        detail,
        key_hint,
        disabled_reason.is_none(),
        disabled_reason,
        PaletteTarget::Action(action),
    )
}

fn capability_reason(selected: Option<&Session>, supported: bool, action: &str) -> Option<String> {
    match selected {
        None => Some("unavailable · no session selected".to_string()),
        Some(_) if supported => None,
        Some(session) => Some(format!(
            "unavailable · {} does not support {action}",
            session.backend.name()
        )),
    }
}

fn palette_session_item(session: &Session) -> PaletteItem {
    PaletteItem::new(
        PaletteGroup::Sessions,
        status_icon(&session.status),
        session.title.clone(),
        format!(
            "{} · {} · {}",
            session.backend.name(),
            session.cwd.display(),
            status_word(&session.status)
        ),
        Some("⏎"),
        true,
        None,
        PaletteTarget::Session {
            backend: session.backend,
            id: session.id.clone(),
        },
    )
}

fn status_icon(status: &Status) -> &'static str {
    match status {
        Status::Working => "✽",
        Status::NeedsInput { .. } => "◐",
        Status::Idle => "∙",
        Status::Done => "●",
        Status::Error => "✗",
        Status::Unknown => "?",
    }
}

fn status_word(status: &Status) -> &'static str {
    match status {
        Status::Working => "working",
        Status::NeedsInput { .. } => "needs input",
        Status::Idle => "idle",
        Status::Done => "done",
        Status::Error => "error",
        Status::Unknown => "unknown",
    }
}

fn palette_commands(backend: BackendKind, target: Option<&std::path::Path>) -> Vec<String> {
    let home = agent_viewer_core::home_dir();
    let mut commands = match backend {
        BackendKind::Claude => {
            let mut commands = subdir_names(&home.join(".claude/skills"));
            if let Some(target) = target {
                commands.extend(subdir_names(&target.join(".claude/skills")));
            }
            commands
        }
        BackendKind::Opencode => file_stems(&home.join(".config/opencode/command")),
        BackendKind::Codex => file_stems(&home.join(".codex/prompts")),
    };
    commands.extend(["model".to_string(), "theme".to_string()]);
    commands.retain(|command| !matches!(command.as_str(), "wall" | "tail"));
    commands.sort();
    commands.dedup();
    commands
}

fn backend_of(backends: &[Box<dyn Backend>], kind: BackendKind) -> Option<&dyn Backend> {
    backends
        .iter()
        .find(|backend| backend.kind() == kind)
        .map(|backend| backend.as_ref())
}

fn handle_palette_key<B: ratatui::backend::Backend>(
    key: KeyEvent,
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
) -> io::Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut close = false;
    let mut execute = false;
    {
        let Mode::Palette(palette) = &mut ui.mode else {
            return Ok(());
        };
        match key.code {
            KeyCode::Down => palette.move_highlight(1),
            KeyCode::Up => palette.move_highlight(-1),
            KeyCode::Tab => palette.scope_highlighted(),
            KeyCode::Backspace => palette.backspace(),
            KeyCode::Esc if palette.escape() => close = true,
            KeyCode::Esc => {}
            KeyCode::Char(character) if !ctrl => palette.push(character),
            KeyCode::Enter => execute = true,
            _ => {}
        }
    }
    if close {
        ui.mode = Mode::Normal;
    } else if execute {
        execute_palette_selection(backends, ui, terminal)?;
    }
    Ok(())
}

fn cached_palette_target(
    ui: &Ui,
    target: &PaletteTarget,
) -> Option<(TargetRequest, String, PaletteAction)> {
    match target {
        PaletteTarget::Action(action) => {
            let session = ui.app.selected()?;
            let action = match action {
                PaletteAction::Attach
                | PaletteAction::Archive
                | PaletteAction::Unarchive
                | PaletteAction::Rename
                | PaletteAction::StopOrRemove => *action,
                _ => return None,
            };
            Some((TargetRequest::from(session), session.title.clone(), action))
        }
        PaletteTarget::Session { backend, id } => {
            let title = ui
                .app
                .session_for(&(*backend, id.clone()))
                .map(|session| session.title.clone())
                .unwrap_or_else(|| id.clone());
            Some((
                TargetRequest::new(*backend, id.clone()),
                title,
                PaletteAction::Attach,
            ))
        }
        _ => None,
    }
}

fn execute_cached_palette_action(
    ui: &mut Ui,
    request: TargetRequest,
    title: String,
    action: PaletteAction,
) {
    match action {
        PaletteAction::Attach => {
            submit_attach(ui, request);
        }
        PaletteAction::Archive => {
            hide_request(ui, request, title, true);
        }
        PaletteAction::Unarchive => {
            hide_request(ui, request, title, false);
        }
        PaletteAction::Rename => {
            open_rename_request(ui, request);
        }
        PaletteAction::StopOrRemove => {
            let stage = ui.app.kill_stage(agent_viewer_core::spawn::now_ms());
            kill_request(ui, request, title, stage);
        }
        _ => {}
    }
}

fn execute_palette_selection<B: ratatui::backend::Backend>(
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    _terminal: &mut ratatui::Terminal<B>,
) -> io::Result<()> {
    let Some(item) = (match &ui.mode {
        Mode::Palette(palette) => palette.highlighted().cloned(),
        _ => None,
    }) else {
        return Ok(());
    };
    if let Some((request, title, action)) = cached_palette_target(ui, &item.target) {
        if let PaletteTarget::Session { backend, id } = &item.target {
            let _ = ui.app.select_by_key(&(*backend, id.clone()));
        }
        ui.mode = Mode::Normal;
        execute_cached_palette_action(ui, request, title, action);
        return Ok(());
    }
    if !item.enabled {
        if let Some(reason) = item.disabled_reason {
            ui.set_notice(reason);
        }
        return Ok(());
    }

    ui.mode = Mode::Normal;
    match item.target {
        PaletteTarget::Action(action) => match action {
            PaletteAction::Attach => {
                attach_selected(ui);
            }
            PaletteAction::Archive => hide_selected(backends, ui, true),
            PaletteAction::Unarchive => hide_selected(backends, ui, false),
            PaletteAction::Rename => open_rename(backends, ui),
            PaletteAction::Reply => open_reply(backends, ui),
            PaletteAction::StopOrRemove => kill_selected(backends, ui),
            PaletteAction::ShowAll => ui.app.toggle_show_all(),
            PaletteAction::Group => ui.app.toggle_group_mode(),
            PaletteAction::Filter => open_filter(ui),
        },
        PaletteTarget::Session { backend, id } => {
            if ui.app.select_by_key(&(backend, id)) {
                attach_selected(ui);
            } else {
                ui.set_notice("session is no longer visible".to_string());
            }
        }
        PaletteTarget::Model { backend, name } => {
            select_palette_model(ui, backend, name);
        }
        PaletteTarget::Sprite(sprite) => set_sprite(ui, sprite),
        PaletteTarget::Command(command) => {
            ui.composer.clear();
            ui.composer.push_str(&format!("/{command} "));
            if command == "theme" {
                ui.themes.open_picker();
            }
            ensure_completions(ui);
        }
    }
    Ok(())
}

fn select_palette_model(ui: &mut Ui, backend: BackendKind, name: String) {
    while ui.composer.backend() != backend {
        ui.composer.cycle_backend();
    }
    let mut models = ui
        .models
        .models(backend)
        .map(<[String]>::to_vec)
        .unwrap_or_else(|| vec![backend.default_model().to_string()]);
    if !models.contains(&name) {
        models.push(name.clone());
    }
    ui.composer.set_models(vec![name], backend);
    ui.composer.set_models(models, backend);
}

/// While attached: Ctrl+] always detaches; Left detaches when the input line is empty
/// (else it is forwarded); a dead child detaches on any key; everything else is encoded
/// to bytes and written to the PTY.
fn handle_attached_key(key: KeyEvent, ui: &mut Ui) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(fkey) = ui.focused.clone() else {
        detach_to_list(ui);
        return;
    };

    // Any key here is the user taking over, so cancel a pending reply injection on this
    // attach (do not type our queued reply in behind the user's own input).
    ui.pending_reply = None;

    // Ctrl+] always detaches (PTY lives on in the map). Terminals send Ctrl+] as raw byte
    // 0x1D, which crossterm's legacy unix parser maps to Char('5')+CTRL (it folds 0x1C..=0x1F
    // onto Ctrl+'4'..'7'); the kitty keyboard protocol delivers the literal Char(']')+CTRL.
    // Match both so the header/help "ctrl+]" is honored under either encoding.
    if ctrl && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5')) {
        detach_to_list(ui);
        return;
    }

    // If the child has exited, any key drops the dead PTY (and its tracker) and returns.
    let exited = ui
        .attached
        .get_mut(&fkey)
        .map(|pty| pty.is_exited())
        .unwrap_or(true);
    if exited {
        ui.remove_pty(&fkey);
        detach_to_list(ui);
        return;
    }

    // Left detaches only when this PTY's input line is empty; otherwise forward it as cursor
    // motion. The tracker is per-PTY so a re-attach preserves any half-typed line.
    if matches!(key.code, KeyCode::Left)
        && ui
            .detach_trackers
            .get(&fkey)
            .is_none_or(|t| t.detach_on_left())
    {
        detach_to_list(ui);
        return;
    }

    // Track pending input so the Left-gate knows whether the line is mid-edit.
    if let Some(tracker) = ui.detach_trackers.get_mut(&fkey) {
        match key.code {
            KeyCode::Char(_) => tracker.on_char(),
            KeyCode::Backspace => tracker.on_backspace(),
            KeyCode::Enter => tracker.on_enter(),
            _ => {}
        }
    }

    if let Some(bytes) = key_to_bytes(key)
        && let Some(pty) = ui.attached.get_mut(&fkey)
    {
        let _ = pty.write_input(&bytes);
    }
}

/// Detach the focused PTY back to the list (the PTY keeps running in the map).
fn detach_to_list(ui: &mut Ui) {
    ui.mode = Mode::Normal;
    ui.focused = None;
    set_mouse_capture(ui, true);
}

fn handle_filter_key(code: KeyCode, ui: &mut Ui) {
    match code {
        KeyCode::Esc => {
            ui.app.set_filter(String::new());
            ui.mode = Mode::Normal;
        }
        KeyCode::Enter => ui.mode = Mode::Normal,
        KeyCode::Backspace => {
            let mut f = ui.app.filter().to_string();
            f.pop();
            ui.app.set_filter(f);
        }
        KeyCode::Char(c) => {
            let mut f = ui.app.filter().to_string();
            f.push(c);
            ui.app.set_filter(f);
        }
        _ => {}
    }
}

fn handle_rename_key(code: KeyCode, ui: &mut Ui) {
    let Mode::Rename(modal) = &mut ui.mode else {
        return;
    };
    match code {
        KeyCode::Esc => ui.mode = Mode::Normal,
        KeyCode::Backspace => {
            modal.buffer.pop();
        }
        KeyCode::Char(c) => modal.buffer.push(c),
        KeyCode::Enter => {
            apply_rename(ui);
            ui.mode = Mode::Normal;
        }
        _ => {}
    }
}

/// Reply-compose key handling: Enter delivers (and attaches); every other key edits the
/// buffer or cancels. Enter is split out because delivery needs the terminal + backends.
fn handle_reply_key<B: ratatui::backend::Backend>(
    code: KeyCode,
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::Terminal<B>,
) -> io::Result<()> {
    match code {
        KeyCode::Enter => {
            send_reply(backends, ui, terminal)?;
            // send_reply attaches on success (Mode::Attached); if it bailed, drop to Normal.
            if !matches!(ui.mode, Mode::Attached) {
                ui.mode = Mode::Normal;
            }
        }
        other => edit_reply(other, ui),
    }
    Ok(())
}

/// The pure reply-compose state machine: Esc cancels, Backspace/Char edit the buffer.
fn edit_reply(code: KeyCode, ui: &mut Ui) {
    let Mode::Reply(modal) = &mut ui.mode else {
        return;
    };
    match code {
        KeyCode::Esc => ui.mode = Mode::Normal,
        KeyCode::Backspace => {
            modal.buffer.pop();
        }
        KeyCode::Char(c) => modal.buffer.push(c),
        _ => {}
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        ATTACHED_CODEX_WHEEL_ROWS, MouseAction, MouseTarget, ensure_completions,
        handle_attached_key, handle_mouse_event, handle_normal_key, handle_palette_key,
        handle_paste, handle_rename_key, is_quit_chord, open_filter, open_palette,
        set_mouse_capture,
    };
    use crate::{NoticeState, Ui};
    use agent_viewer_core::pty::{PtySession, PtySpec, VIEWPORT_SCROLLBACK_ROWS};
    use agent_viewer_core::{BackendKind, Session, Status};
    use agent_viewer_tui::app::{App, Composer, DetachTracker, GroupKey, GroupMode, Row, Section};
    use agent_viewer_tui::mutations::{AttachRunner, MutationOutcome, MutationRunner};
    use agent_viewer_tui::ui::{AttachView, Draw, Mode, Pulses};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn press_normal_key(
        ui: &mut Ui,
        backends: &[Box<dyn agent_viewer_core::Backend>],
        c: char,
        modifiers: KeyModifiers,
    ) -> bool {
        press_normal_code(ui, backends, KeyCode::Char(c), modifiers)
    }

    fn press_normal_code(
        ui: &mut Ui,
        backends: &[Box<dyn agent_viewer_core::Backend>],
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> bool {
        ui.models.seed(
            BackendKind::Claude,
            vec![ui.composer.model().to_string()],
            true,
        );
        let (_snapshot_tx, snapshots) = std::sync::mpsc::channel::<(Vec<Session>, String, usize)>();
        let (wake, _wake_rx) = std::sync::mpsc::channel();
        let refresher = crate::Refresher { snapshots, wake };
        let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("fixed terminal");

        handle_normal_key(
            key(code, modifiers),
            modifiers.contains(KeyModifiers::CONTROL),
            backends,
            &refresher,
            ui,
            &mut terminal,
        )
        .expect("normal key routing")
    }

    pub(crate) fn test_ui_with(sessions: Vec<Session>) -> Ui {
        Ui {
            app: App::new(sessions),
            workspace: PathBuf::from("/tmp"),
            mode: Mode::Normal,
            notice: NoticeState::default(),
            db: None,
            composer: Composer::new(),
            themes: agent_viewer_tui::ui::ThemeState::default(),
            detach_trackers: HashMap::new(),
            last_backend_error: String::new(),
            mutations: MutationRunner::new(),
            mutation_executor: std::sync::Arc::new(|_| {
                Ok(MutationOutcome {
                    notice: String::new(),
                    spawned: None,
                })
            }),
            attaches: AttachRunner::new(),
            attach_executor: std::sync::Arc::new(|_| {
                Err("attach is not configured in this test".to_string())
            }),
            models: agent_viewer_tui::model_cache::ModelCache::new(),
            pulses: Pulses::new(),
            pr_status: agent_viewer_tui::pr_cache::PrStatusCache::new(),
            pending_spawn: None,
            pending_reply: None,
            attached: HashMap::new(),
            terminal_palette: None,
            focused: None,
            focused_session: None,
            focused_exited: false,
            logos: None,
            list_hit: std::cell::RefCell::new(agent_viewer_tui::ui::ListHit::default()),
            mouse_capture: true,
            mouse_press: None,
            sprite: Default::default(),
        }
    }

    type TestTerminal = ratatui::Terminal<ratatui::backend::TestBackend>;

    fn test_terminal() -> TestTerminal {
        const WIDTH: u16 = 80;
        const HEIGHT: u16 = 24;
        let backend = ratatui::backend::TestBackend::new(WIDTH, HEIGHT);
        ratatui::Terminal::new(backend).expect("test terminal")
    }

    fn render_attached_frame(
        ui: &Ui,
        key: &(BackendKind, String),
        terminal: &mut TestTerminal,
    ) -> String {
        let session = ui.focused_session.as_ref().expect("focused session");
        let pty = ui.attached.get(key).expect("attached child");
        terminal
            .draw(|frame| {
                agent_viewer_tui::ui::draw(
                    frame,
                    Draw {
                        app: &ui.app,
                        workspace: &ui.workspace,
                        mode: &ui.mode,
                        notice: ui.notice.text(),
                        composer: &ui.composer,
                        pulses: &ui.pulses,
                        now_ms: 0,
                        attach: Some(AttachView {
                            session,
                            pty,
                            exited: false,
                        }),
                        pr_status: &ui.pr_status,
                        logos: None,
                        list_hit: &ui.list_hit,
                        themes: &ui.themes,
                        sprite: ui.sprite,
                    },
                );
            })
            .expect("draw attached frame");

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn point_for_visible_row(ui: &Ui, terminal: &mut TestTerminal, row_idx: usize) -> (u16, u16) {
        let size = terminal.size().expect("test terminal size");
        terminal
            .draw(|frame| {
                agent_viewer_tui::ui::draw(
                    frame,
                    agent_viewer_tui::ui::Draw {
                        app: &ui.app,
                        workspace: &ui.workspace,
                        mode: &ui.mode,
                        notice: ui.notice.text(),
                        composer: &ui.composer,
                        pulses: &ui.pulses,
                        now_ms: 0,
                        attach: None,
                        pr_status: &ui.pr_status,
                        logos: None,
                        list_hit: &ui.list_hit,
                        themes: &ui.themes,
                        sprite: ui.sprite,
                    },
                );
            })
            .expect("draw list geometry");

        let hit = ui.list_hit.borrow();
        for y in 0..size.height {
            for x in 0..size.width {
                if hit.row_at(x, y) == Some(row_idx) {
                    return (x, y);
                }
            }
        }
        panic!("visible row {row_idx} has no mouse hit");
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn visible_session_index(ui: &Ui, id: &str) -> usize {
        ui.app
            .visible()
            .iter()
            .position(|row| matches!(row, Row::Session { id: row_id, .. } if row_id == id))
            .expect("session row present")
    }

    fn wait_for_pty_screen(ui: &Ui, key: &(BackendKind, String), needle: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if ui
                .attached
                .get(key)
                .is_some_and(|pty| pty.with_screen(|screen| screen.contents().contains(needle)))
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "attached child screen did not contain {needle:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn mouse_recording_pty() -> agent_viewer_core::pty::PtySession {
        agent_viewer_core::pty::PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "stty raw -echo; ",
                    "printf '\\033[?1000h\\033[?1006hREADY\\r\\n'; ",
                    "bytes=$(timeout 0.5 dd bs=1 count=9 2>/dev/null | od -An -tx1); ",
                    "if [ -n \"$bytes\" ]; then ",
                    "printf 'BYTES:%s\\r\\n' \"$bytes\"; ",
                    "else printf 'CLEAN\\r\\n'; fi; ",
                    "sleep 30"
                )
                .to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 24,
            cols: 80,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("mouse recording child")
    }

    fn mouse_forwarding_pty() -> agent_viewer_core::pty::PtySession {
        agent_viewer_core::pty::PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "stty raw -echo; ",
                    "printf '\\033[?1000h\\033[?1006hREADY\\r\\n'; ",
                    "dd bs=1 count=9 2>/dev/null | od -An -tx1; ",
                    "sleep 30"
                )
                .to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 24,
            cols: 80,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("mouse forwarding child")
    }

    fn mouse_scroll_forwarding_pty() -> agent_viewer_core::pty::PtySession {
        agent_viewer_core::pty::PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "stty raw -echo; ",
                    "printf '\\033[?1000h\\033[?1006hREADY\\r\\n'; ",
                    "dd bs=1 count=10 2>/dev/null | od -An -tx1; ",
                    "sleep 30"
                )
                .to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 24,
            cols: 80,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("scroll forwarding child")
    }

    fn codex_viewport_mouse_pty() -> agent_viewer_core::pty::PtySession {
        agent_viewer_core::pty::PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "stty raw -echo; ",
                    "index=0; ",
                    "while [ \"$index\" -lt 12 ]; do ",
                    "printf 'codex-history-%02d\\r\\n' \"$index\"; ",
                    "index=$((index + 1)); ",
                    "done; ",
                    "printf '\\033[?1000h\\033[?1006hREADY'; ",
                    "bytes=$(dd bs=1 count=9 2>/dev/null | od -An -tx1); ",
                    "printf '\\r\\nBYTES:%s\\r\\n' \"$bytes\"; ",
                    "sleep 30"
                )
                .to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 4,
            cols: 80,
            palette: None,
            scrollback_rows: VIEWPORT_SCROLLBACK_ROWS,
        })
        .expect("codex viewport mouse child")
    }

    fn codex_restricted_viewport_mouse_pty() -> agent_viewer_core::pty::PtySession {
        agent_viewer_core::pty::PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "stty raw -echo; ",
                    "printf '\\033[1;4r\\033[1;1H'; ",
                    "index=0; ",
                    "while [ \"$index\" -lt 8 ]; do ",
                    "printf 'codex-region-%04d\\r\\n' \"$index\"; ",
                    "index=$((index + 1)); ",
                    "done; ",
                    "printf 'READY'; ",
                    "sleep 30"
                )
                .to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 6,
            cols: 40,
            palette: None,
            scrollback_rows: VIEWPORT_SCROLLBACK_ROWS,
        })
        .expect("Codex restricted viewport child")
    }

    pub(crate) fn sess(id: &str, cwd: &str, updated_at_ms: i64) -> Session {
        Session {
            backend: BackendKind::Claude,
            id: id.into(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: id.into(),
            cwd: std::path::PathBuf::from(cwd),
            git_branch: None,
            status: Status::Done,
            created_at_ms: updated_at_ms,
            updated_at_ms,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    struct AttachingBackend;

    impl agent_viewer_core::Backend for AttachingBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Opencode
        }

        fn capabilities(&self) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities {
                attach: true,
                ..agent_viewer_core::Capabilities::none()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            Ok(["a", "b", "c", "attached"]
                .into_iter()
                .map(|id| {
                    let mut session = sess(id, "/tmp/agentviewer-attach", 100);
                    session.backend = BackendKind::Opencode;
                    session
                })
                .collect())
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawn is not exercised by mouse activation")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> std::result::Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 30"]);
            Ok(command)
        }
    }

    struct AnyAttachingBackend(BackendKind);

    impl agent_viewer_core::Backend for AnyAttachingBackend {
        fn kind(&self) -> BackendKind {
            self.0
        }

        fn capabilities(&self) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities {
                attach: true,
                ..agent_viewer_core::Capabilities::none()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            Ok(["shared-attach", "a"]
                .into_iter()
                .map(|id| {
                    let mut session = sess(id, "/tmp/agentviewer-attach", 100);
                    session.backend = self.0;
                    session.short_id = Some("short".to_string());
                    session
                })
                .collect())
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawn is not exercised by attach selection")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> std::result::Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            let mut command = std::process::Command::new("sh");
            let script = match self.0 {
                BackendKind::Codex => concat!(
                    "index=0; ",
                    "while [ \"$index\" -lt 40 ]; do ",
                    "printf 'codex-history-%02d\\r\\n' \"$index\"; ",
                    "index=$((index + 1)); ",
                    "done; ",
                    "printf 'READY'; ",
                    "sleep 30"
                ),
                BackendKind::Claude | BackendKind::Opencode => concat!(
                    "stty raw -echo; ",
                    "printf '\\033[?1000h\\033[?1006hREADY\\r\\n'; ",
                    "index=1; ",
                    "while [ \"$index\" -le 2 ]; do ",
                    "bytes=$(dd bs=1 count=10 2>/dev/null | od -An -tx1); ",
                    "printf 'WHEEL-%s:%s\\r\\n' \"$index\" \"$bytes\"; ",
                    "index=$((index + 1)); ",
                    "done; ",
                    "sleep 30"
                ),
            };
            command.args(["-c", script]);
            Ok(command)
        }
    }

    #[test]
    fn ensure_models_fills_the_picker_from_the_cached_catalog() {
        // A catalog seeded from the viewer DB must reach the `/model` picker on the key path,
        // without waiting on (or spawning) the multi-second CLI probe behind discovery.
        use super::ensure_models;
        let mut ui = test_ui_with(Vec::new());
        ui.models.seed(
            BackendKind::Claude,
            vec!["opus[1m]".to_string(), "sonnet-5".to_string()],
            true,
        );

        ensure_models(&mut ui);
        for c in "/model son".chars() {
            ui.composer.push_char(c);
        }

        assert_eq!(ui.composer.models_key(), Some(BackendKind::Claude));
        assert_eq!(
            ui.composer.model_suggestions(),
            vec!["sonnet-5".to_string()]
        );
    }

    #[test]
    fn install_models_lands_a_discovered_catalog_into_the_picker() {
        // Nothing cached: the picker starts at the backend default and fills in when the
        // background probe lands, which is the whole point of moving discovery off-thread.
        use super::ensure_models;
        use crate::actions::install_models;
        use std::time::{Duration, Instant};
        let mut ui = test_ui_with(Vec::new());
        ui.models.request_with(BackendKind::Claude, || {
            vec!["opus[1m]".to_string(), "kimi-k3".to_string()]
        });

        ensure_models(&mut ui);
        for c in "/model kimi".chars() {
            ui.composer.push_char(c);
        }
        assert_eq!(ui.composer.model(), "opus[1m]");
        assert!(ui.composer.model_suggestions().is_empty());

        let start = Instant::now();
        while ui.composer.model_suggestions().is_empty() && start.elapsed() < Duration::from_secs(5)
        {
            install_models(&mut ui);
        }

        assert_eq!(ui.composer.model_suggestions(), vec!["kimi-k3".to_string()]);
    }

    #[test]
    fn ctrl_f_open_filter_enters_filter_mode() {
        let mut ui = test_ui_with(Vec::new());
        assert!(matches!(ui.mode, Mode::Normal));
        open_filter(&mut ui);
        assert!(matches!(ui.mode, Mode::Filter));
        assert_eq!(ui.app.filter(), ""); // opens with a fresh, empty query
    }

    #[test]
    fn multiline_paste_updates_the_draft_without_requesting_a_spawn() {
        let mut ui = test_ui_with(Vec::new());

        handle_paste("first\nsecond", &mut ui);

        assert_eq!(ui.composer.text(), "first\nsecond");
        assert!(matches!(ui.mode, Mode::Normal));
        assert_eq!(ui.notice.text(), "");
        assert!(ui.pulses.is_empty());
        assert!(
            !ui.mutations.in_flight("claude:first\nsecond:spawn"),
            "paste must not submit the composed task"
        );
    }

    #[test]
    fn attached_multiline_paste_is_bracketed_on_a_real_pty() {
        use std::time::{Duration, Instant};

        let payload = "one\ntwo";
        let expected_hex = "1b5b3230307e6f6e650a74776f1b5b3230317e";
        let script = concat!(
            "stty raw -echo; ",
            "printf '\\033[?2004hREADY\\r\\n'; ",
            "captured=$(dd bs=1 count=19 2>/dev/null | od -An -v -tx1 | tr -d ' \\n'); ",
            "printf '\\033[?2004lCAPTURE:%s\\r\\n' \"$captured\"; ",
            "sleep 30"
        );
        let pty = PtySession::spawn(PtySpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            envs: Vec::new(),
            rows: 24,
            cols: 80,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("spawn real pty");

        let mut ui = test_ui_with(Vec::new());
        let key = (BackendKind::Codex, "attached-session".to_string());
        ui.mode = Mode::Attached;
        ui.focused = Some(key.clone());
        ui.detach_trackers.insert(key.clone(), DetachTracker::new());
        ui.attached.insert(key.clone(), pty);

        let ready_start = Instant::now();
        while ready_start.elapsed() < Duration::from_secs(5)
            && !ui.attached.get(&key).unwrap().with_screen(|screen| {
                screen.bracketed_paste() && screen.contents().contains("READY")
            })
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ui.attached.get(&key).unwrap().with_screen(|screen| {
                screen.bracketed_paste() && screen.contents().contains("READY")
            }),
            "child never enabled bracketed paste"
        );

        handle_paste(payload, &mut ui);

        let capture_start = Instant::now();
        while capture_start.elapsed() < Duration::from_secs(5)
            && !ui
                .attached
                .get(&key)
                .unwrap()
                .with_screen(|screen| screen.contents().contains(expected_hex))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let contents = ui
            .attached
            .get(&key)
            .unwrap()
            .with_screen(|screen| screen.contents());
        assert!(
            contents.contains(&format!("CAPTURE:{expected_hex}")),
            "child observed unexpected paste bytes: {contents:?}"
        );
    }

    #[test]
    fn attached_raw_multiline_paste_keeps_only_the_final_line_before_left_detaches() {
        use std::time::{Duration, Instant};

        let script = concat!(
            "stty raw -echo; ",
            "printf 'READY\\r\\n'; ",
            "captured=$(dd bs=1 count=7 2>/dev/null | od -An -v -tx1 | tr -d ' \\n'); ",
            "printf 'CAPTURE:%s\\r\\n' \"$captured\"; ",
            "sleep 30"
        );
        let pty = PtySession::spawn(PtySpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            envs: Vec::new(),
            rows: 24,
            cols: 80,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("spawn real pty");

        let mut ui = test_ui_with(Vec::new());
        let session_key = (BackendKind::Codex, "raw-paste-session".to_string());
        ui.mode = Mode::Attached;
        ui.focused = Some(session_key.clone());
        ui.detach_trackers
            .insert(session_key.clone(), DetachTracker::new());
        ui.attached.insert(session_key.clone(), pty);

        let ready_start = Instant::now();
        while ready_start.elapsed() < Duration::from_secs(5)
            && !ui
                .attached
                .get(&session_key)
                .unwrap()
                .with_screen(|screen| screen.contents().contains("READY"))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ui.attached
                .get(&session_key)
                .unwrap()
                .with_screen(|screen| {
                    !screen.bracketed_paste() && screen.contents().contains("READY")
                }),
            "child unexpectedly enabled bracketed paste"
        );

        handle_paste("foo\nbar", &mut ui);

        let capture_start = Instant::now();
        while capture_start.elapsed() < Duration::from_secs(5)
            && !ui
                .attached
                .get(&session_key)
                .unwrap()
                .with_screen(|screen| screen.contents().contains("CAPTURE:666f6f0a626172"))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ui.attached
                .get(&session_key)
                .unwrap()
                .with_screen(|screen| { screen.contents().contains("CAPTURE:666f6f0a626172") })
        );

        handle_attached_key(key(KeyCode::Backspace, KeyModifiers::NONE), &mut ui);
        handle_attached_key(key(KeyCode::Backspace, KeyModifiers::NONE), &mut ui);
        handle_attached_key(key(KeyCode::Left, KeyModifiers::NONE), &mut ui);
        assert!(matches!(ui.mode, Mode::Attached));

        handle_attached_key(key(KeyCode::Backspace, KeyModifiers::NONE), &mut ui);
        handle_attached_key(key(KeyCode::Left, KeyModifiers::NONE), &mut ui);
        assert!(matches!(ui.mode, Mode::Normal));
    }

    #[test]
    fn attached_raw_crlf_paste_submits_once_and_leaves_left_clear_to_detach() {
        use std::time::{Duration, Instant};

        let script = concat!(
            "stty raw -echo; ",
            "printf 'READY\\r\\n'; ",
            "captured=$(dd bs=1 count=5 2>/dev/null | od -An -v -tx1 | tr -d ' \\n'); ",
            "printf 'CAPTURE:%s\\r\\n' \"$captured\"; ",
            "sleep 30"
        );
        let pty = PtySession::spawn(PtySpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            envs: Vec::new(),
            rows: 24,
            cols: 80,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("spawn real pty");

        let mut ui = test_ui_with(Vec::new());
        ui.mouse_capture = false;
        let session_key = (BackendKind::Codex, "raw-crlf-session".to_string());
        ui.mode = Mode::Attached;
        ui.focused = Some(session_key.clone());
        ui.detach_trackers
            .insert(session_key.clone(), DetachTracker::new());
        ui.attached.insert(session_key.clone(), pty);

        let ready_start = Instant::now();
        while ready_start.elapsed() < Duration::from_secs(5)
            && !ui
                .attached
                .get(&session_key)
                .unwrap()
                .with_screen(|screen| screen.contents().contains("READY"))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ui.attached
                .get(&session_key)
                .unwrap()
                .with_screen(|screen| {
                    !screen.bracketed_paste() && screen.contents().contains("READY")
                }),
            "child unexpectedly enabled bracketed paste"
        );

        handle_paste("foo\r\n", &mut ui);

        let capture_start = Instant::now();
        while capture_start.elapsed() < Duration::from_secs(5)
            && !ui
                .attached
                .get(&session_key)
                .unwrap()
                .with_screen(|screen| screen.contents().contains("CAPTURE:666f6f0d0a"))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ui.attached
                .get(&session_key)
                .unwrap()
                .with_screen(|screen| { screen.contents().contains("CAPTURE:666f6f0d0a") })
        );

        handle_attached_key(key(KeyCode::Left, KeyModifiers::NONE), &mut ui);
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(
            ui.mouse_capture,
            "an empty Left detach must restore list mouse capture"
        );
    }

    #[test]
    fn attached_unicode_paste_requires_matching_backspaces_before_left_can_detach() {
        let mut ui = test_ui_with(Vec::new());
        let key = (BackendKind::Codex, "attached-session".to_string());
        ui.mode = Mode::Attached;
        ui.focused = Some(key.clone());
        ui.detach_trackers.insert(key.clone(), DetachTracker::new());

        handle_paste("λ🦀é", &mut ui);

        let tracker = ui.detach_trackers.get_mut(&key).unwrap();
        tracker.on_backspace();
        assert!(!tracker.detach_on_left());
        tracker.on_backspace();
        assert!(!tracker.detach_on_left());
        tracker.on_backspace();
        assert!(tracker.detach_on_left());
    }

    #[test]
    fn paste_does_not_leak_into_the_composer_outside_normal_mode() {
        let mut ui = test_ui_with(Vec::new());
        ui.composer.push_char('x');
        ui.mode = Mode::Help;

        handle_paste("first\nsecond", &mut ui);

        assert_eq!(ui.composer.text(), "x");
        assert!(matches!(ui.mode, Mode::Help));
    }

    #[test]
    fn filter_paste_appends_as_one_normalized_line() {
        let mut ui = test_ui_with(Vec::new());
        ui.app.set_filter("before ".to_string());
        ui.mode = Mode::Filter;

        handle_paste("one\r\ntwo\rthree", &mut ui);

        assert_eq!(ui.app.filter(), "before one two three");
        assert!(matches!(ui.mode, Mode::Filter));
    }

    #[test]
    fn rename_paste_appends_as_one_normalized_line() {
        use agent_viewer_tui::ui::RenameModal;

        let mut ui = test_ui_with(Vec::new());
        ui.mode = Mode::Rename(RenameModal {
            backend: BackendKind::Claude,
            id: "s1".to_string(),
            buffer: "before ".to_string(),
        });

        handle_paste("one\r\ntwo\rthree", &mut ui);

        match &ui.mode {
            Mode::Rename(modal) => assert_eq!(modal.buffer, "before one two three"),
            _ => panic!("expected rename mode"),
        }
    }

    #[test]
    fn reply_paste_preserves_normalized_line_breaks() {
        use agent_viewer_tui::ui::ReplyModal;

        let mut ui = test_ui_with(Vec::new());
        ui.mode = Mode::Reply(ReplyModal {
            backend: BackendKind::Claude,
            id: "s1".to_string(),
            buffer: "before\n".to_string(),
        });

        handle_paste("one\r\ntwo\rthree", &mut ui);

        match &ui.mode {
            Mode::Reply(modal) => assert_eq!(modal.buffer, "before\none\ntwo\nthree"),
            _ => panic!("expected reply mode"),
        }
    }

    /// A bg row: it carries the short id that names its job dir, so rename applies to it.
    fn bg_sess(id: &str, cwd: &str, updated_at_ms: i64) -> Session {
        Session {
            short_id: Some(id.to_string()),
            origin: agent_viewer_core::SessionOrigin::Background,
            ..sess(id, cwd, updated_at_ms)
        }
    }

    /// Move the selection onto the session row for `id` (row 0 is a section header).
    pub(crate) fn select_session_row(ui: &mut Ui, id: &str) {
        let idx = ui
            .app
            .visible()
            .iter()
            .position(
                |r| matches!(r, agent_viewer_tui::app::Row::Session { id: rid, .. } if rid == id),
            )
            .expect("session row present");
        assert!(ui.app.select_visible_index(idx));
    }

    /// A backend that advertises rename so `open_rename` gets past the capability gate.
    /// Every other method is unreachable from these tests and panics if it is ever called,
    /// so a test that accidentally reaches the real mutation path fails loudly.
    struct RenamingBackend;

    impl agent_viewer_core::Backend for RenamingBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }
        fn capabilities(&self) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities {
                rename: true,
                ..agent_viewer_core::Capabilities::none()
            }
        }
        /// Mirrors `ClaudeBackend`: rename needs the short id that names the job dir, so an
        /// interactive row is unsupported even though the backend advertises rename.
        fn capabilities_for(&self, session: &Session) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities {
                rename: session.short_id.as_deref().is_some_and(|s| !s.is_empty()),
                ..self.capabilities()
            }
        }
        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            unreachable!("list is not exercised by the rename key tests")
        }
        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawn is not exercised by the rename key tests")
        }
        fn attach_command(
            &self,
            _session: &Session,
        ) -> std::result::Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attach is not exercised by the rename key tests")
        }
    }

    struct ArchivingBackend;

    impl agent_viewer_core::Backend for ArchivingBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }
        fn capabilities(&self) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities {
                archive: true,
                ..agent_viewer_core::Capabilities::none()
            }
        }
        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            unreachable!("list is not exercised by the archive key tests")
        }
        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawn is not exercised by the archive key tests")
        }
        fn attach_command(
            &self,
            _session: &Session,
        ) -> std::result::Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attach is not exercised by the archive key tests")
        }
    }

    struct RowScopedArchivingBackend;

    impl agent_viewer_core::Backend for RowScopedArchivingBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Opencode
        }

        fn capabilities(&self) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities {
                archive: true,
                ..agent_viewer_core::Capabilities::none()
            }
        }

        fn capabilities_for(&self, session: &Session) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities {
                archive: session.daemon_hosted,
                ..agent_viewer_core::Capabilities::none()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            unreachable!("list is not exercised by row scoped archive tests")
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
        ) -> agent_viewer_core::Result<agent_viewer_core::SpawnResult> {
            unreachable!("spawn is not exercised by row scoped archive tests")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> std::result::Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attach is not exercised by row scoped archive tests")
        }
    }

    #[test]
    fn ctrl_k_escape_restores_selection_and_preserves_the_composer() {
        let mut ui = test_ui_with(vec![
            sess("first", "/tmp/agentviewer-palette-first", 100),
            sess("second", "/tmp/agentviewer-palette-second", 200),
        ]);
        select_session_row(&mut ui, "first");
        ui.composer.push_str("draft stays");
        let selected = ui.app.selected_index();

        assert!(!press_normal_key(&mut ui, &[], 'k', KeyModifiers::CONTROL));
        assert!(matches!(ui.mode, Mode::Palette(_)));
        let mut terminal = test_terminal();
        handle_palette_key(
            key(KeyCode::Esc, KeyModifiers::NONE),
            &[],
            &mut ui,
            &mut terminal,
        )
        .expect("escape palette");

        assert!(matches!(ui.mode, Mode::Normal));
        assert_eq!(ui.app.selected_index(), selected);
        assert_eq!(ui.composer.text(), "draft stays");
    }

    #[test]
    fn cached_palette_action_dispatches_before_fresh_authorization() {
        let mut ui = test_ui_with(vec![sess(
            "external",
            "/tmp/agentviewer-palette-disabled",
            100,
        )]);
        select_session_row(&mut ui, "external");
        assert!(!press_normal_key(&mut ui, &[], 'k', KeyModifiers::CONTROL));
        let mut terminal = test_terminal();
        for character in "arch".chars() {
            handle_palette_key(
                key(KeyCode::Char(character), KeyModifiers::NONE),
                &[],
                &mut ui,
                &mut terminal,
            )
            .expect("type palette query");
        }
        handle_palette_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &[],
            &mut ui,
            &mut terminal,
        )
        .expect("accept disabled action");

        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.mutations.in_flight("claude:external:hide"));
    }

    #[test]
    fn palette_model_accept_sets_the_model_without_touching_the_draft() {
        let mut ui = test_ui_with(Vec::new());
        ui.composer.set_models(
            vec!["sonnet".to_string(), "opus[1m]".to_string()],
            BackendKind::Claude,
        );
        ui.models.seed(
            BackendKind::Claude,
            vec!["sonnet".to_string(), "opus[1m]".to_string()],
            true,
        );
        ui.composer.push_str("existing draft");
        open_palette(&[], &mut ui);
        let mut terminal = test_terminal();
        for character in "opus".chars() {
            handle_palette_key(
                key(KeyCode::Char(character), KeyModifiers::NONE),
                &[],
                &mut ui,
                &mut terminal,
            )
            .expect("type palette query");
        }
        handle_palette_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &[],
            &mut ui,
            &mut terminal,
        )
        .expect("accept palette model");

        assert!(matches!(ui.mode, Mode::Normal));
        assert_eq!(ui.composer.model(), "opus[1m]");
        assert_eq!(ui.composer.text(), "existing draft");
    }

    /// The palette is the discoverable way to switch mascots, so searching one by name and
    /// pressing Enter must actually swap the header sprite (not merely list it).
    #[test]
    fn palette_picks_a_header_sprite_by_name() {
        let mut ui = test_ui_with(Vec::new());
        ui.composer.push_str("existing draft");
        open_palette(&[], &mut ui);
        let mut terminal = test_terminal();
        for character in "turbine".chars() {
            handle_palette_key(
                key(KeyCode::Char(character), KeyModifiers::NONE),
                &[],
                &mut ui,
                &mut terminal,
            )
            .expect("type palette query");
        }
        handle_palette_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &[],
            &mut ui,
            &mut terminal,
        )
        .expect("accept palette sprite");

        assert!(matches!(ui.mode, Mode::Normal));
        assert_eq!(ui.sprite, super::SpriteKind::Turbine);
        assert!(ui.notice.text().contains("turbine"));
        assert_eq!(ui.composer.text(), "existing draft");
    }

    #[test]
    fn ctrl_g_cycles_the_header_sprite() {
        let mut ui = test_ui_with(Vec::new());
        let start = ui.sprite;

        press_normal_key(&mut ui, &[], 'g', KeyModifiers::CONTROL);
        assert_eq!(ui.sprite, start.next());

        press_normal_key(&mut ui, &[], 'g', KeyModifiers::CONTROL);
        assert_eq!(ui.sprite, start.next().next());
        assert_ne!(ui.sprite, start);
        assert!(ui.notice.text().contains(ui.sprite.name()));
    }

    #[test]
    fn ctrl_r_opens_rename_with_an_empty_buffer() {
        // DELIBERATE DIVERGENCE from Fleet View, which prefills Ctrl+R with the current
        // name. Renaming here always means typing a new name from scratch, so a prefill is
        // only text to clear first.
        let mut ui = test_ui_with(vec![bg_sess("s1", "/tmp/agentviewer-rename", 100)]);
        select_session_row(&mut ui, "s1");
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(RenamingBackend)];

        crate::actions::open_rename(&backends, &mut ui);

        match &ui.mode {
            Mode::Rename(m) => {
                assert_eq!(m.id, "s1");
                assert_eq!(m.buffer, "", "rename opens blank, never prefilled");
            }
            _ => panic!("expected rename mode"),
        }
    }

    #[test]
    fn ctrl_r_defers_row_authorization_to_the_fresh_runner() {
        let mut ui = test_ui_with(vec![sess("s1", "/tmp/agentviewer-rename", 100)]);
        select_session_row(&mut ui, "s1"); // sess() builds rows with no short id
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(RenamingBackend)];

        crate::actions::open_rename(&backends, &mut ui);

        assert!(matches!(ui.mode, Mode::Rename(_)));
    }

    #[test]
    fn enter_on_a_blank_rename_buffer_cancels_instead_of_renaming() {
        // The blank-open above makes an accidental bare Enter easy, and an empty name is
        // never a rename any backend should be asked to perform.
        let mut ui = test_ui_with(vec![sess("s1", "/tmp/agentviewer-rename", 100)]);
        ui.mode = Mode::Rename(agent_viewer_tui::ui::RenameModal {
            backend: BackendKind::Claude,
            id: "s1".to_string(),
            buffer: "   ".to_string(),
        });

        // No backend is registered, so a submitted mutation would panic in the worker; the
        // observable contract is that Enter leaves no "renaming…" notice behind.
        handle_rename_key(KeyCode::Enter, &mut ui);

        assert!(matches!(ui.mode, Mode::Normal));
        assert!(
            !ui.notice.text.starts_with("renaming"),
            "blank rename must not submit a mutation, got notice {:?}",
            ui.notice.text
        );
    }

    #[test]
    fn ctrl_t_is_the_mouse_toggle_chord_in_every_mode() {
        use super::is_mouse_toggle_chord;
        // Ctrl+T is claimed app-wide, including while attached — the attached transcript is
        // exactly where text most needs selecting, so the child does not get this chord.
        assert!(is_mouse_toggle_chord(
            key(KeyCode::Char('t'), KeyModifiers::CONTROL),
            true
        ));
        // A bare `t` must still type into the composer, not toggle the mouse.
        assert!(!is_mouse_toggle_chord(
            key(KeyCode::Char('t'), KeyModifiers::NONE),
            false
        ));
        assert!(!is_mouse_toggle_chord(
            key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            true
        ));
    }

    #[test]
    fn set_mouse_capture_names_controls_for_the_active_surface() {
        use super::apply_mouse_capture_state as set_capture;
        let mut ui = test_ui_with(Vec::new());
        assert!(ui.mouse_capture, "capture starts on");

        // Off: the flag drops and the footer tells the user both what changed and how to undo
        // it, because the mode is otherwise invisible on screen.
        set_capture(&mut ui, false);
        assert!(!ui.mouse_capture);
        let off = ui.notice.text().to_string();
        assert!(off.contains("drag to select"), "notice was {off:?}");
        assert!(
            off.contains("ctrl+t"),
            "notice must name the way back: {off:?}"
        );

        // Back on: flag restored, and the notice again names the escape hatch.
        set_capture(&mut ui, true);
        assert!(ui.mouse_capture);
        let on = ui.notice.text().to_string();
        assert!(on.contains("click/hover"), "notice was {on:?}");
        assert!(
            on.contains("ctrl+t"),
            "notice must name the way back: {on:?}"
        );

        ui.mode = Mode::Attached;
        set_capture(&mut ui, true);
        let attached_on = ui.notice.text().to_string();
        assert!(
            attached_on.contains("wheel scrolls"),
            "notice was {attached_on:?}"
        );
        assert!(
            !attached_on.contains("click/hover"),
            "attached notice must not promise list selection: {attached_on:?}"
        );

        set_capture(&mut ui, false);
        let attached_off = ui.notice.text().to_string();
        assert!(
            attached_off.contains("restore scrolling"),
            "notice was {attached_off:?}"
        );
    }

    #[test]
    fn attach_capture_defaults_are_backend_specific() {
        let mut capture_states = Vec::new();
        for backend in [
            BackendKind::Codex,
            BackendKind::Claude,
            BackendKind::Opencode,
        ] {
            let mut session = sess("shared-attach", "/tmp/agentviewer-shared-attach", 100);
            session.backend = backend;
            // Avoid Claude's real trust preflight; this fake backend only exercises the shared
            // attach success transition after its command is accepted.
            session.short_id = Some("short".to_string());
            let mut ui = test_ui_with(vec![session]);
            ui.mouse_capture = backend == BackendKind::Opencode;
            select_session_row(&mut ui, "shared-attach");
            ui.attach_executor = std::sync::Arc::new(move |request| {
                let mut authority = AnyAttachingBackend(backend);
                crate::ops::resolve_attach_with_backend(&mut authority, request)
            });
            let mut terminal = test_terminal();

            assert!(crate::actions::attach_selected(&mut ui));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            let plan = loop {
                if let Some(result) = ui.attaches.poll() {
                    break result.expect("resolve selected session");
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "selected attach did not resolve"
                );
                std::thread::yield_now();
            };
            assert!(
                crate::actions::install_attach_plan(&mut ui, &mut terminal, plan)
                    .expect("install selected session")
            );

            assert!(matches!(ui.mode, Mode::Attached), "{backend:?} must attach");
            capture_states.push((backend, ui.mouse_capture));
        }

        // `sess` is interactive, so the Opencode row represents the external attachable
        // path. Managed Opencode is deliberately absent because it is not attachable.
        assert_eq!(
            capture_states,
            vec![
                (BackendKind::Codex, true),
                (BackendKind::Claude, true),
                (BackendKind::Opencode, false),
            ],
            "attach capture defaults must match each backend"
        );
    }

    #[test]
    fn ctrl_bracket_and_missing_focused_pty_restore_list_mouse_capture() {
        let mut ctrl_detach = test_ui_with(Vec::new());
        ctrl_detach.mode = Mode::Attached;
        ctrl_detach.focused = Some((BackendKind::Codex, "ctrl-detach".to_string()));
        ctrl_detach.mouse_capture = false;

        handle_attached_key(
            key(KeyCode::Char(']'), KeyModifiers::CONTROL),
            &mut ctrl_detach,
        );

        assert!(matches!(ctrl_detach.mode, Mode::Normal));
        assert!(
            ctrl_detach.mouse_capture,
            "Ctrl+] must restore list mouse capture"
        );

        let mut missing_pty = test_ui_with(Vec::new());
        missing_pty.mode = Mode::Attached;
        missing_pty.focused = Some((BackendKind::Codex, "missing-pty".to_string()));
        missing_pty.mouse_capture = false;

        handle_attached_key(
            key(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut missing_pty,
        );

        assert!(matches!(missing_pty.mode, Mode::Normal));
        assert!(
            missing_pty.mouse_capture,
            "a missing focused PTY must restore list mouse capture"
        );
    }

    #[test]
    fn mouse_events_are_ignored_while_capture_is_off() {
        let mut sessions = vec![
            sess("a", "/tmp/agentviewer-mouse-a", 200),
            sess("b", "/tmp/agentviewer-mouse-b", 100),
        ];
        for session in &mut sessions {
            session.backend = BackendKind::Opencode;
        }
        let mut ui = test_ui_with(sessions);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];
        let mut terminal = test_terminal();
        // The wheel walks the selection and needs no drawn geometry, unlike a click.
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };

        // Capture on: the wheel walks the selection down one row.
        let start = ui.app.selected_index();
        ui.mouse_capture = true;
        handle_mouse_event(wheel, &backends, &mut ui, &mut terminal).expect("wheel event");
        let moved = ui.app.selected_index();
        assert_ne!(
            moved, start,
            "with capture on the wheel must move selection"
        );
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.attached.is_empty());

        // Capture off (text-select mode): the same wheel event changes nothing. While the
        // terminal owns the mouse, a stray report must not steer the selection.
        let before = ui.app.selected_index();
        ui.mouse_capture = false;
        handle_mouse_event(wheel, &backends, &mut ui, &mut terminal).expect("wheel event");
        assert_eq!(
            ui.app.selected_index(),
            before,
            "the wheel must be inert while mouse capture is off"
        );
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.attached.is_empty());
    }

    #[test]
    fn left_click_on_header_toggles_and_persists_group() {
        use agent_viewer_core::state::ViewerDb;

        let temp = tempfile::tempdir().expect("temp dir");
        let mut ui = test_ui_with(vec![sess(
            "a",
            temp.path().to_str().expect("utf8 temp path"),
            100,
        )]);
        ui.db = Some(ViewerDb::open(&temp.path().join("viewer.db")).expect("viewer db"));
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        let mut terminal = test_terminal();

        let (header_idx, group_key) = ui
            .app
            .visible()
            .iter()
            .enumerate()
            .find_map(|(idx, row)| match row {
                Row::ProjectHeader { root, .. } => Some((idx, GroupKey::Project(root.clone()))),
                _ => None,
            })
            .expect("project header");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, header_idx);

        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("header button down");

        assert_eq!(ui.app.selected_index(), header_idx);
        assert!(!ui.app.is_group_collapsed(&group_key));
        assert!(
            !ui.db
                .as_ref()
                .expect("viewer db")
                .collapsed_groups()
                .expect("collapsed groups")
                .contains(&group_key.to_storage()),
            "button down must not persist a compact state"
        );

        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("header button up");

        assert!(ui.app.is_group_collapsed(&group_key));
        assert!(
            ui.db
                .as_ref()
                .expect("viewer db")
                .collapsed_groups()
                .expect("collapsed groups")
                .contains(&group_key.to_storage()),
            "button up must persist the compact state"
        );

        let header_idx = ui
            .app
            .visible()
            .iter()
            .position(|row| matches!(row, Row::ProjectHeader { .. }))
            .expect("collapsed project header");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, header_idx);
        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("open header button down");

        assert!(ui.app.is_group_collapsed(&group_key));
        assert!(
            ui.db
                .as_ref()
                .expect("viewer db")
                .collapsed_groups()
                .expect("collapsed groups")
                .contains(&group_key.to_storage()),
            "button down must not persist an open state"
        );

        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("open header button up");

        assert!(!ui.app.is_group_collapsed(&group_key));
        assert!(
            !ui.db
                .as_ref()
                .expect("viewer db")
                .collapsed_groups()
                .expect("collapsed groups")
                .contains(&group_key.to_storage()),
            "button up must persist the open state"
        );
    }

    #[test]
    fn left_click_on_state_header_toggles_and_persists_group() {
        use agent_viewer_core::state::ViewerDb;

        let temp = tempfile::tempdir().expect("temp dir");
        let mut ui = test_ui_with(vec![sess(
            "a",
            temp.path().to_str().expect("utf8 temp path"),
            100,
        )]);
        ui.app.toggle_group_mode();
        assert_eq!(ui.app.group_mode(), GroupMode::ByState);
        ui.db = Some(ViewerDb::open(&temp.path().join("viewer.db")).expect("viewer db"));
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        let mut terminal = test_terminal();
        let group_key = GroupKey::State(Section::Done);
        let header_idx = ui
            .app
            .visible()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    Row::SectionHeader {
                        section: Section::Done,
                        ..
                    }
                )
            })
            .expect("done section header");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, header_idx);

        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("state header button down");

        assert_eq!(ui.app.selected_index(), header_idx);
        assert!(!ui.app.is_group_collapsed(&group_key));
        assert!(
            !ui.db
                .as_ref()
                .expect("viewer db")
                .collapsed_groups()
                .expect("collapsed groups")
                .contains(&group_key.to_storage()),
            "button down must not persist a compact state"
        );

        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("state header button up");

        assert!(ui.app.is_group_collapsed(&group_key));
        assert!(
            ui.db
                .as_ref()
                .expect("viewer db")
                .collapsed_groups()
                .expect("collapsed groups")
                .contains(&group_key.to_storage()),
            "button up must persist the compact state"
        );
    }

    #[test]
    fn left_click_on_session_attaches_the_clicked_row() {
        let mut sessions = vec![
            sess("a", "/tmp/agentviewer-mouse-activate", 200),
            sess("b", "/tmp/agentviewer-mouse-activate", 100),
        ];
        for session in &mut sessions {
            session.backend = BackendKind::Opencode;
        }
        let mut ui = test_ui_with(sessions);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];
        ui.attach_executor = std::sync::Arc::new(|request| {
            let mut authority = AttachingBackend;
            crate::ops::resolve_attach_with_backend(&mut authority, request)
        });
        let mut terminal = test_terminal();
        let target_idx = visible_session_index(&ui, "b");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, target_idx);

        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("session button down");

        assert_eq!(
            ui.app.selected().map(|session| session.id.as_str()),
            Some("b"),
            "button down must select the clicked session"
        );
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.focused_session.is_none());
        assert!(
            ui.attached.is_empty(),
            "button down must not attach before release"
        );

        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("session button up");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let plan = loop {
            if let Some(result) = ui.attaches.poll() {
                break result.expect("resolve clicked session");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "clicked session attach did not resolve"
            );
            std::thread::yield_now();
        };
        assert!(
            crate::actions::install_attach_plan(&mut ui, &mut terminal, plan)
                .expect("install clicked session")
        );
        assert!(matches!(ui.mode, Mode::Attached));
        assert_eq!(
            ui.focused_session
                .as_ref()
                .map(|session| session.id.as_str()),
            Some("b"),
            "the clicked session must become focused"
        );
        assert!(
            ui.attached
                .contains_key(&(BackendKind::Opencode, "b".to_string())),
            "the clicked session must own an attached child"
        );
    }

    /// The background refresh rebuilds the list every 1-2s, so rows can be inserted above the
    /// pressed one while the physical cursor never moves: by the time the button comes up, the
    /// pressed coordinate resolves to a different session. The click must still activate the row
    /// the user actually pressed on.
    #[test]
    fn left_click_activates_the_pressed_row_when_the_list_reflows_between_button_events() {
        let mut sessions = vec![
            sess("a", "/tmp/agentviewer-mouse-reflow", 100),
            sess("b", "/tmp/agentviewer-mouse-reflow", 300),
        ];
        for session in &mut sessions {
            session.backend = BackendKind::Opencode;
        }
        let mut ui = test_ui_with(sessions);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];
        ui.attach_executor = std::sync::Arc::new(|request| {
            let mut authority = AttachingBackend;
            crate::ops::resolve_attach_with_backend(&mut authority, request)
        });
        let mut terminal = test_terminal();
        let pressed_idx = visible_session_index(&ui, "b");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, pressed_idx);
        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("session button down");

        assert_eq!(
            ui.app.selected().map(|session| session.id.as_str()),
            Some("b")
        );
        assert_eq!(
            ui.mouse_press.as_ref().map(|press| &press.target),
            Some(&MouseTarget::Session(
                BackendKind::Opencode,
                "b".to_string()
            ))
        );

        // A refresh lands a newer session between "a" and "b", pushing "b" one row down.
        let mut refreshed = vec![
            sess("a", "/tmp/agentviewer-mouse-reflow", 100),
            sess("c", "/tmp/agentviewer-mouse-reflow", 200),
            sess("b", "/tmp/agentviewer-mouse-reflow", 300),
        ];
        for session in &mut refreshed {
            session.backend = BackendKind::Opencode;
        }
        ui.app.set_sessions(refreshed);
        let reflowed_idx = visible_session_index(&ui, "b");
        assert_ne!(
            reflowed_idx, pressed_idx,
            "the reflow must move the pressed session to a different row"
        );
        // Re-render so list_hit carries the new geometry, exactly as the event loop does.
        point_for_visible_row(&ui, &mut terminal, reflowed_idx);
        assert_eq!(
            ui.list_hit.borrow().row_at(x, y),
            Some(visible_session_index(&ui, "c")),
            "the pressed point must now resolve to the inserted row"
        );
        assert_eq!(
            ui.app.selected().map(|session| session.id.as_str()),
            Some("b"),
            "the refresh must keep the selection anchored to the pressed session"
        );

        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("session button up");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let plan = loop {
            if let Some(result) = ui.attaches.poll() {
                break result.expect("resolve pressed session after reflow");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "reflowed session attach did not resolve"
            );
            std::thread::yield_now();
        };
        assert!(
            crate::actions::install_attach_plan(&mut ui, &mut terminal, plan)
                .expect("install pressed session after reflow")
        );
        assert!(matches!(ui.mode, Mode::Attached));
        assert_eq!(
            ui.focused_session
                .as_ref()
                .map(|session| session.id.as_str()),
            Some("b"),
            "the pressed session, not the row now under the cursor, must be activated"
        );
        assert!(
            ui.attached
                .contains_key(&(BackendKind::Opencode, "b".to_string()))
        );
        assert!(
            !ui.attached
                .contains_key(&(BackendKind::Opencode, "c".to_string())),
            "the row that slid under the cursor must never be attached"
        );
    }

    /// The same refresh with a harsher outcome: the pressed session leaves the list and another
    /// row slides under the unmoved cursor. Activating whatever is selected now would attach a
    /// session the user never pressed on, so the release must be inert.
    #[test]
    fn left_click_release_is_inert_when_a_reflow_drops_the_pressed_row() {
        let mut sessions = vec![
            sess("a", "/tmp/agentviewer-mouse-reflow-drop", 300),
            sess("b", "/tmp/agentviewer-mouse-reflow-drop", 100),
        ];
        for session in &mut sessions {
            session.backend = BackendKind::Opencode;
        }
        let mut ui = test_ui_with(sessions);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];
        let mut terminal = test_terminal();
        let pressed_idx = visible_session_index(&ui, "b");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, pressed_idx);
        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("session button down");

        // The refresh archives "b" out of the list and adds "c" in its place.
        let mut refreshed = vec![
            sess("a", "/tmp/agentviewer-mouse-reflow-drop", 300),
            sess("c", "/tmp/agentviewer-mouse-reflow-drop", 200),
        ];
        for session in &mut refreshed {
            session.backend = BackendKind::Opencode;
        }
        ui.app.set_sessions(refreshed);
        let newcomer_idx = visible_session_index(&ui, "c");
        // Re-render so list_hit carries the new geometry, exactly as the event loop does.
        point_for_visible_row(&ui, &mut terminal, newcomer_idx);
        assert_eq!(
            ui.list_hit.borrow().row_at(x, y),
            Some(newcomer_idx),
            "the pressed point must now resolve to the replacement row"
        );
        assert_ne!(
            ui.app.selected().map(|session| session.id.as_str()),
            Some("b"),
            "the pressed session is gone, so the selection cannot still be anchored to it"
        );

        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("session button up");

        assert!(matches!(ui.mode, Mode::Normal));
        assert!(
            ui.focused_session.is_none(),
            "a release whose pressed row vanished must not activate anything"
        );
        assert!(ui.attached.is_empty());
    }

    #[test]
    fn left_release_without_a_matching_press_is_inert() {
        let mut sessions = vec![
            sess("a", "/tmp/agentviewer-mouse-unmatched", 200),
            sess("b", "/tmp/agentviewer-mouse-unmatched", 100),
        ];
        for session in &mut sessions {
            session.backend = BackendKind::Opencode;
        }
        let mut ui = test_ui_with(sessions);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];
        let mut terminal = test_terminal();
        let target_idx = visible_session_index(&ui, "b");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, target_idx);
        let selected_before = ui.app.selected().map(|session| session.id.clone());

        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("unmatched release");

        assert_eq!(
            ui.app.selected().map(|session| session.id.clone()),
            selected_before
        );
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.focused_session.is_none());
        assert!(ui.attached.is_empty());
    }

    #[test]
    fn left_press_and_release_on_different_rows_is_inert() {
        let mut sessions = vec![
            sess("a", "/tmp/agentviewer-mouse-mismatch", 200),
            sess("b", "/tmp/agentviewer-mouse-mismatch", 100),
        ];
        for session in &mut sessions {
            session.backend = BackendKind::Opencode;
        }
        let mut ui = test_ui_with(sessions);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];
        let mut terminal = test_terminal();
        let pressed_idx = visible_session_index(&ui, "b");
        let released_idx = visible_session_index(&ui, "a");
        let (down_x, down_y) = point_for_visible_row(&ui, &mut terminal, pressed_idx);
        let (up_x, up_y) = point_for_visible_row(&ui, &mut terminal, released_idx);

        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), down_x, down_y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("press first row");
        assert_eq!(
            ui.app.selected().map(|session| session.id.as_str()),
            Some("b")
        );

        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), up_x, up_y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("release second row");

        assert_eq!(
            ui.app.selected().map(|session| session.id.as_str()),
            Some("b")
        );
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.focused_session.is_none());
        assert!(ui.attached.is_empty());
    }

    #[test]
    fn left_press_off_list_then_release_on_session_is_inert() {
        let mut sessions = vec![
            sess("a", "/tmp/agentviewer-mouse-off-list", 200),
            sess("b", "/tmp/agentviewer-mouse-off-list", 100),
        ];
        for session in &mut sessions {
            session.backend = BackendKind::Opencode;
        }
        let mut ui = test_ui_with(sessions);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];
        let mut terminal = test_terminal();
        let target_idx = visible_session_index(&ui, "b");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, target_idx);
        let selected_before = ui.app.selected_index();

        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), u16::MAX, u16::MAX),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("off list button down");
        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("session button up");

        assert_eq!(ui.app.selected_index(), selected_before);
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.focused_session.is_none());
        assert!(ui.attached.is_empty());
    }

    #[test]
    fn activating_release_is_not_forwarded_to_the_reused_session_pty() {
        let mut sessions = vec![
            sess("a", "/tmp/agentviewer-mouse-consumed", 200),
            sess("b", "/tmp/agentviewer-mouse-consumed", 100),
        ];
        for session in &mut sessions {
            session.backend = BackendKind::Opencode;
        }
        let mut ui = test_ui_with(sessions);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];
        ui.attach_executor = std::sync::Arc::new(|request| {
            let mut authority = AttachingBackend;
            crate::ops::resolve_attach_with_backend(&mut authority, request)
        });
        let mut terminal = test_terminal();
        let target_idx = visible_session_index(&ui, "b");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, target_idx);

        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("session button down");
        assert!(ui.attached.is_empty());

        let key = (BackendKind::Opencode, "b".to_string());
        ui.attached.insert(key.clone(), mouse_recording_pty());
        wait_for_pty_screen(&ui, &key, "READY");

        handle_mouse_event(
            mouse(MouseEventKind::Up(MouseButton::Left), x, y),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("session button up");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let plan = loop {
            if let Some(result) = ui.attaches.poll() {
                break result.expect("resolve reused session");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "reused session attach did not resolve"
            );
            std::thread::yield_now();
        };
        assert!(
            crate::actions::install_attach_plan(&mut ui, &mut terminal, plan)
                .expect("install reused session")
        );
        assert!(matches!(ui.mode, Mode::Attached));
        wait_for_pty_screen(&ui, &key, "CLEAN");
    }

    #[test]
    fn mouse_move_over_session_only_selects() {
        use super::handle_mouse;

        let mut ui = test_ui_with(vec![
            sess("a", "/tmp/agentviewer-mouse-hover", 200),
            sess("b", "/tmp/agentviewer-mouse-hover", 100),
        ]);
        let mut terminal = test_terminal();
        let target_idx = visible_session_index(&ui, "b");
        let (x, y) = point_for_visible_row(&ui, &mut terminal, target_idx);

        let action = handle_mouse(mouse(MouseEventKind::Moved, x, y), &mut ui);

        assert_eq!(action, MouseAction::None);
        assert_eq!(
            ui.app.selected().map(|session| session.id.as_str()),
            Some("b")
        );
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.focused_session.is_none());
        assert!(ui.attached.is_empty());
    }

    #[test]
    fn left_click_without_a_list_hit_is_inert() {
        use super::handle_mouse;

        let mut ui = test_ui_with(vec![
            sess("a", "/tmp/agentviewer-mouse-miss", 200),
            sess("b", "/tmp/agentviewer-mouse-miss", 100),
        ]);
        let selected_before = ui.app.selected().map(|session| session.id.clone());

        let action = handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), u16::MAX, u16::MAX),
            &mut ui,
        );

        assert_eq!(action, MouseAction::None);
        assert_eq!(
            ui.app.selected().map(|session| session.id.clone()),
            selected_before
        );
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.focused_session.is_none());
    }

    #[test]
    fn right_and_middle_clicks_do_not_activate() {
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];

        for button in [MouseButton::Right, MouseButton::Middle] {
            let mut session = sess("a", "/tmp/agentviewer-mouse-other-button", 100);
            session.backend = BackendKind::Opencode;
            let mut ui = test_ui_with(vec![session]);
            let mut terminal = test_terminal();
            let target_idx = visible_session_index(&ui, "a");
            let (x, y) = point_for_visible_row(&ui, &mut terminal, target_idx);

            handle_mouse_event(
                mouse(MouseEventKind::Down(button), x, y),
                &backends,
                &mut ui,
                &mut terminal,
            )
            .expect("nonleft click");
            handle_mouse_event(
                mouse(MouseEventKind::Up(button), x, y),
                &backends,
                &mut ui,
                &mut terminal,
            )
            .expect("nonleft release");

            assert!(matches!(ui.mode, Mode::Normal));
            assert!(ui.focused_session.is_none());
            assert!(ui.attached.is_empty());
        }
    }

    #[test]
    fn mouse_click_is_inert_in_every_modal_mode() {
        use agent_viewer_tui::ui::{RenameModal, ReplyModal};

        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(AttachingBackend)];
        let modes = [
            Mode::Filter,
            Mode::Help,
            Mode::Rename(RenameModal {
                backend: BackendKind::Opencode,
                id: "b".to_string(),
                buffer: String::new(),
            }),
            Mode::Reply(ReplyModal {
                backend: BackendKind::Opencode,
                id: "b".to_string(),
                buffer: String::new(),
            }),
        ];

        for mode in modes {
            let mut sessions = vec![
                sess("a", "/tmp/agentviewer-mouse-modal", 200),
                sess("b", "/tmp/agentviewer-mouse-modal", 100),
            ];
            for session in &mut sessions {
                session.backend = BackendKind::Opencode;
            }
            let mut ui = test_ui_with(sessions);
            let mut terminal = test_terminal();
            let target_idx = visible_session_index(&ui, "b");
            let (x, y) = point_for_visible_row(&ui, &mut terminal, target_idx);

            handle_mouse_event(
                mouse(MouseEventKind::Down(MouseButton::Left), x, y),
                &backends,
                &mut ui,
                &mut terminal,
            )
            .expect("normal button down");
            assert!(ui.mouse_press.is_some());

            let selected_before = ui.app.selected().map(|session| session.id.clone());
            let expected_mode = std::mem::discriminant(&mode);
            ui.mode = mode;
            handle_mouse_event(
                mouse(MouseEventKind::Down(MouseButton::Left), x, y),
                &backends,
                &mut ui,
                &mut terminal,
            )
            .expect("modal button down");
            assert!(
                ui.mouse_press.is_none(),
                "modal button down must clear a stale normal mode press"
            );
            handle_mouse_event(
                mouse(MouseEventKind::Up(MouseButton::Left), x, y),
                &backends,
                &mut ui,
                &mut terminal,
            )
            .expect("modal button up");

            assert_eq!(std::mem::discriminant(&ui.mode), expected_mode);
            assert_eq!(
                ui.app.selected().map(|session| session.id.clone()),
                selected_before
            );
            assert!(ui.focused_session.is_none());
            assert!(ui.attached.is_empty());
            assert!(ui.mouse_press.is_none());
        }
    }

    #[test]
    fn mouse_event_in_attached_mode_reaches_the_child_without_list_activation() {
        let mut ui = test_ui_with(vec![sess("a", "/tmp/agentviewer-mouse-attached", 100)]);
        let key = (BackendKind::Claude, "a".to_string());
        ui.attached.insert(key.clone(), mouse_forwarding_pty());
        ui.focused = Some(key.clone());
        ui.mode = Mode::Attached;
        wait_for_pty_screen(&ui, &key, "READY");
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        let mut terminal = test_terminal();

        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), 5, 5),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("forward attached mouse event");

        wait_for_pty_screen(&ui, &key, "1b 5b 3c 30 3b 36 3b 35 4d");
        assert!(matches!(ui.mode, Mode::Attached));
    }

    #[test]
    fn codex_wheel_scrolls_the_local_viewport_but_later_pointer_input_reaches_the_child() {
        let key = (BackendKind::Codex, "codex-scroll".to_string());
        let mut ui = test_ui_with(Vec::new());
        ui.attached.insert(key.clone(), codex_viewport_mouse_pty());
        ui.focused = Some(key.clone());
        ui.mode = Mode::Attached;
        wait_for_pty_screen(&ui, &key, "READY");
        let live_view = ui
            .attached
            .get(&key)
            .expect("codex child")
            .with_screen(|screen| screen.contents());
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        let mut terminal = test_terminal();

        handle_mouse_event(
            mouse(MouseEventKind::ScrollUp, 5, 5),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("scroll codex viewport");

        let (offset, historical_view) = ui
            .attached
            .get(&key)
            .expect("codex child")
            .with_screen(|screen| (screen.scrollback(), screen.contents()));
        assert_eq!(offset, ATTACHED_CODEX_WHEEL_ROWS);
        assert_ne!(
            historical_view, live_view,
            "Codex wheel input must move the local viewport"
        );

        handle_mouse_event(
            mouse(MouseEventKind::Down(MouseButton::Left), 5, 5),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("forward later Codex pointer event");
        ui.attached
            .get_mut(&key)
            .expect("codex child")
            .scroll_viewport_down(usize::MAX);

        wait_for_pty_screen(&ui, &key, "BYTES: 1b 5b 3c 30 3b 36 3b 35 4d");
        assert!(matches!(ui.mode, Mode::Attached));
    }

    #[test]
    fn codex_wheel_renders_history_from_a_top_anchored_restricted_region() {
        let key = (BackendKind::Codex, "codex-region-scroll".to_string());
        let mut focused_session = sess("codex-region-scroll", "/tmp", 100);
        focused_session.backend = BackendKind::Codex;
        let mut ui = test_ui_with(vec![focused_session.clone()]);
        ui.attached
            .insert(key.clone(), codex_restricted_viewport_mouse_pty());
        ui.focused = Some(key.clone());
        ui.focused_session = Some(focused_session);
        ui.mode = Mode::Attached;
        wait_for_pty_screen(&ui, &key, "READY");
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        let mut terminal = test_terminal();

        let live_frame = render_attached_frame(&ui, &key, &mut terminal);
        assert!(
            !live_frame.contains("codex-region-0002"),
            "the older Codex row was already visible in the live frame: {live_frame:?}"
        );

        handle_mouse_event(
            mouse(MouseEventKind::ScrollUp, 5, 5),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("scroll Codex restricted viewport");

        let historical_frame = render_attached_frame(&ui, &key, &mut terminal);
        assert!(
            historical_frame.contains("codex-region-0002"),
            "the rendered frame did not reveal the older Codex row: {historical_frame:?}"
        );
    }

    #[test]
    fn claude_wheel_reaches_the_child_as_an_xterm_report() {
        let key = (BackendKind::Claude, "claude-scroll".to_string());
        let mut ui = test_ui_with(Vec::new());
        ui.attached
            .insert(key.clone(), mouse_scroll_forwarding_pty());
        ui.focused = Some(key.clone());
        ui.mode = Mode::Attached;
        wait_for_pty_screen(&ui, &key, "READY");
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        let mut terminal = test_terminal();

        handle_mouse_event(
            mouse(MouseEventKind::ScrollUp, 5, 5),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("forward Claude wheel event");

        wait_for_pty_screen(&ui, &key, "1b 5b 3c 36 34 3b 36 3b 35 4d");
        assert!(matches!(ui.mode, Mode::Attached));
    }

    #[test]
    fn codex_and_claude_scroll_immediately_after_attach_without_ctrl_t() {
        for backend in [BackendKind::Codex, BackendKind::Claude] {
            let id = "shared-attach";
            let mut session = sess(id, "/tmp/agentviewer-immediate-scroll", 100);
            session.backend = backend;
            session.short_id = Some("short".to_string());
            let key = (backend, id.to_string());
            let mut ui = test_ui_with(vec![session]);
            ui.mouse_capture = false;
            select_session_row(&mut ui, id);
            ui.attach_executor = std::sync::Arc::new(move |request| {
                let mut authority = AnyAttachingBackend(backend);
                crate::ops::resolve_attach_with_backend(&mut authority, request)
            });
            let mut terminal = test_terminal();

            assert!(crate::actions::attach_selected(&mut ui));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            let plan = loop {
                if let Some(result) = ui.attaches.poll() {
                    break result.expect("resolve selected session");
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "selected attach did not resolve"
                );
                std::thread::yield_now();
            };
            assert!(
                crate::actions::install_attach_plan(&mut ui, &mut terminal, plan)
                    .expect("install selected session")
            );
            assert!(matches!(ui.mode, Mode::Attached));
            assert!(
                ui.mouse_capture,
                "{backend:?} attach must restore capture from selection mode"
            );
            let child_pid = ui
                .attached
                .get(&key)
                .expect("attached child")
                .pid()
                .expect("attached child pid");

            let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
            match backend {
                BackendKind::Codex => {
                    wait_for_pty_screen(&ui, &key, "READY");
                    let live_frame = render_attached_frame(&ui, &key, &mut terminal);

                    handle_mouse_event(
                        mouse(MouseEventKind::ScrollUp, 5, 5),
                        &backends,
                        &mut ui,
                        &mut terminal,
                    )
                    .expect("scroll Codex immediately after attach");

                    let offset = ui
                        .attached
                        .get(&key)
                        .expect("Codex child")
                        .with_screen(|screen| screen.scrollback());
                    let historical_frame = render_attached_frame(&ui, &key, &mut terminal);
                    assert_eq!(offset, ATTACHED_CODEX_WHEEL_ROWS);
                    assert_ne!(
                        historical_frame, live_frame,
                        "Codex wheel input must render historical output immediately after attach"
                    );
                    ui.attached
                        .get_mut(&key)
                        .expect("Codex child")
                        .scroll_viewport_down(usize::MAX);
                }
                BackendKind::Claude => {
                    wait_for_pty_screen(&ui, &key, "READY");

                    handle_mouse_event(
                        mouse(MouseEventKind::ScrollUp, 5, 5),
                        &backends,
                        &mut ui,
                        &mut terminal,
                    )
                    .expect("forward Claude wheel immediately after attach");

                    wait_for_pty_screen(&ui, &key, "WHEEL-1: 1b 5b 3c 36 34 3b 36 3b 35 4d");
                }
                BackendKind::Opencode => unreachable!("only attached scroll backends"),
            }

            set_mouse_capture(&mut ui, false);
            assert!(!ui.mouse_capture, "selection mode must release capture");
            assert!(crate::actions::attach_selected(&mut ui));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            let plan = loop {
                if let Some(result) = ui.attaches.poll() {
                    break result.expect("resolve retained session");
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "retained attach did not resolve"
                );
                std::thread::yield_now();
            };
            assert!(
                crate::actions::install_attach_plan(&mut ui, &mut terminal, plan)
                    .expect("install retained session")
            );
            assert!(
                ui.mouse_capture,
                "{backend:?} reattach must restore capture from selection mode"
            );
            assert_eq!(
                ui.attached.get(&key).expect("retained child").pid(),
                Some(child_pid),
                "reattach must retain the existing PTY child"
            );

            match backend {
                BackendKind::Codex => {
                    let live_frame = render_attached_frame(&ui, &key, &mut terminal);
                    handle_mouse_event(
                        mouse(MouseEventKind::ScrollUp, 5, 5),
                        &backends,
                        &mut ui,
                        &mut terminal,
                    )
                    .expect("scroll retained Codex session");
                    let offset = ui
                        .attached
                        .get(&key)
                        .expect("retained Codex child")
                        .with_screen(|screen| screen.scrollback());
                    let historical_frame = render_attached_frame(&ui, &key, &mut terminal);
                    assert_eq!(offset, ATTACHED_CODEX_WHEEL_ROWS);
                    assert_ne!(
                        historical_frame, live_frame,
                        "retained Codex wheel input must render historical output"
                    );
                }
                BackendKind::Claude => {
                    handle_mouse_event(
                        mouse(MouseEventKind::ScrollUp, 5, 5),
                        &backends,
                        &mut ui,
                        &mut terminal,
                    )
                    .expect("forward retained Claude wheel");
                    wait_for_pty_screen(&ui, &key, "WHEEL-2: 1b 5b 3c 36 34 3b 36 3b 35 4d");
                }
                BackendKind::Opencode => unreachable!("only attached scroll backends"),
            }
        }
    }

    #[test]
    fn external_opencode_ctrl_t_opts_into_wheel_forwarding() {
        let mut session = sess("a", "/tmp/agentviewer-scroll-attached", 100);
        session.backend = BackendKind::Opencode;
        let session_key = (BackendKind::Opencode, session.id.clone());
        let mut ui = test_ui_with(vec![session]);
        select_session_row(&mut ui, "a");
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(AnyAttachingBackend(BackendKind::Opencode))];
        ui.attach_executor = std::sync::Arc::new(|request| {
            let mut authority = AnyAttachingBackend(BackendKind::Opencode);
            crate::ops::resolve_attach_with_backend(&mut authority, request)
        });
        let mut terminal = test_terminal();

        assert!(crate::actions::attach_selected(&mut ui));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let plan = loop {
            if let Some(result) = ui.attaches.poll() {
                break result.expect("resolve external opencode session");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "external opencode attach did not resolve"
            );
            std::thread::yield_now();
        };
        assert!(
            crate::actions::install_attach_plan(&mut ui, &mut terminal, plan)
                .expect("install external opencode session")
        );
        assert!(matches!(ui.mode, Mode::Attached));
        assert!(
            !ui.mouse_capture,
            "external opencode must keep host text selection after attach"
        );

        let (_snapshot_tx, snapshots) = std::sync::mpsc::channel::<(Vec<Session>, String, usize)>();
        let (wake, _wake_rx) = std::sync::mpsc::channel();
        let refresher = crate::Refresher { snapshots, wake };
        assert!(
            !super::handle_key(
                key(KeyCode::Char('t'), KeyModifiers::CONTROL),
                &backends,
                &refresher,
                &mut ui,
                &mut terminal,
            )
            .expect("opt into attached mouse capture"),
            "Ctrl+T must not quit the viewer"
        );
        assert!(
            ui.mouse_capture,
            "Ctrl+T must opt into external opencode wheel forwarding"
        );

        wait_for_pty_screen(&ui, &session_key, "READY");

        handle_mouse_event(
            mouse(MouseEventKind::ScrollUp, 5, 5),
            &backends,
            &mut ui,
            &mut terminal,
        )
        .expect("forward attached wheel event");

        // The child must receive SGR wheel-up (button 64), not ESC [ A prompt-history input.
        wait_for_pty_screen(&ui, &session_key, "WHEEL-1: 1b 5b 3c 36 34 3b 36 3b 35 4d");
        assert!(matches!(ui.mode, Mode::Attached));
    }

    #[test]
    fn reply_compose_edits_buffer_and_esc_cancels() {
        use super::edit_reply;
        use agent_viewer_core::BackendKind;
        use agent_viewer_tui::ui::ReplyModal;
        use crossterm::event::KeyCode;

        let mut ui = test_ui_with(Vec::new());
        ui.mode = Mode::Reply(ReplyModal {
            backend: BackendKind::Claude,
            id: "s1".to_string(),
            buffer: String::new(),
        });

        // Chars append to the buffer.
        edit_reply(KeyCode::Char('h'), &mut ui);
        edit_reply(KeyCode::Char('i'), &mut ui);
        match &ui.mode {
            Mode::Reply(m) => assert_eq!(m.buffer, "hi"),
            _ => panic!("expected reply mode"),
        }

        // Backspace removes the last char.
        edit_reply(KeyCode::Backspace, &mut ui);
        match &ui.mode {
            Mode::Reply(m) => assert_eq!(m.buffer, "h"),
            _ => panic!("expected reply mode"),
        }

        // Esc cancels back to Normal.
        edit_reply(KeyCode::Esc, &mut ui);
        assert!(matches!(ui.mode, Mode::Normal));
    }

    #[test]
    fn completions_refresh_for_new_target_before_a_stale_accept() {
        // Two claude sessions in DIFFERENT project dirs -> distinct spawn targets.
        let mut ui = test_ui_with(vec![
            sess("a", "/tmp/agentviewer-target-a", 200),
            sess("b", "/tmp/agentviewer-target-b", 100),
        ]);
        // Type a "/…" command and scan completions for the first target.
        for ch in "/x".chars() {
            ui.composer.push_char(ch);
        }
        ensure_completions(&mut ui);
        let key1 = ui.composer.commands_key().cloned();
        assert_eq!(
            key1,
            Some((
                BackendKind::Claude,
                Some("/tmp/agentviewer-target-a".into())
            ))
        );

        // The selected session (and thus spawn target) changes WITHOUT going through the
        // composer — as a Ctrl+S regroup or a background snapshot would. The cache is now
        // stale: a Tab here would accept a suggestion computed for the OLD target.
        ui.app.move_selection(1);
        assert_eq!(ui.composer.commands_key(), key1.as_ref()); // still the old key

        // The top-of-handler refresh re-scans for the NEW target, so `suggestions_active`
        // (and any subsequent Tab accept) reflects it, never the stale previous list.
        ensure_completions(&mut ui);
        assert_eq!(
            ui.composer.commands_key(),
            Some(&(
                BackendKind::Claude,
                Some("/tmp/agentviewer-target-b".into())
            ))
        );
    }

    #[test]
    fn ctrl_c_quits_from_the_list_and_transient_modes() {
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        // The list and every transient modal treat Ctrl+C as "kill the viewer".
        assert!(is_quit_chord(ctrl_c, true, &Mode::Normal));
        assert!(is_quit_chord(ctrl_c, true, &Mode::Filter));
        assert!(is_quit_chord(ctrl_c, true, &Mode::Help));
    }

    #[test]
    fn ctrl_c_does_not_quit_while_attached() {
        // Attached, Ctrl+C must reach the child as an interrupt, not tear down the viewer.
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!is_quit_chord(ctrl_c, true, &Mode::Attached));
    }

    #[test]
    fn bare_letters_and_digits_start_composer_text() {
        for c in ('a'..='z').chain('A'..='Z').chain('0'..='9') {
            let mut ui = test_ui_with(vec![sess("s1", "/tmp/agentviewer-keys", 100)]);
            select_session_row(&mut ui, "s1");

            assert!(
                !press_normal_key(&mut ui, &[], c, KeyModifiers::NONE),
                "{c:?} must not quit"
            );
            assert_eq!(ui.composer.text(), c.to_string(), "{c:?} must compose");
            assert!(!ui.app.show_all(), "{c:?} must not change list scope");
            assert!(
                !ui.mutations.in_flight("claude:s1:hide")
                    && !ui.mutations.in_flight("claude:s1:unhide"),
                "{c:?} must not submit a mutation"
            );
            assert_eq!(
                ui.app.selected().map(|session| session.id.as_str()),
                Some("s1"),
                "{c:?} must not move the selection"
            );
        }
    }

    #[test]
    fn bare_question_mark_opens_help() {
        let mut ui = test_ui_with(vec![sess("s1", "/tmp/agentviewer-help", 100)]);
        select_session_row(&mut ui, "s1");

        assert!(!press_normal_key(&mut ui, &[], '?', KeyModifiers::NONE));
        assert!(matches!(ui.mode, Mode::Help));
        assert!(ui.composer.is_empty(), "help must not become composer text");
    }

    #[test]
    fn bare_space_on_a_session_does_nothing() {
        let mut ui = test_ui_with(vec![sess("s1", "/tmp/agentviewer-space", 100)]);
        select_session_row(&mut ui, "s1");
        let selected = ui.app.selected_index();
        let visible_rows = ui.app.visible().len();

        assert!(!press_normal_key(&mut ui, &[], ' ', KeyModifiers::NONE));
        assert!(
            ui.composer.is_empty(),
            "space must not become composer text"
        );
        assert_eq!(ui.app.selected_index(), selected);
        assert_eq!(ui.app.visible().len(), visible_rows);
    }

    #[test]
    fn bare_space_on_a_group_header_collapses_the_group() {
        let mut ui = test_ui_with(vec![sess("s1", "/tmp/agentviewer-group", 100)]);
        let header = ui
            .app
            .visible()
            .iter()
            .position(|row| matches!(row, agent_viewer_tui::app::Row::ProjectHeader { .. }))
            .expect("project header present");
        assert!(ui.app.select_visible_index(header));

        assert!(!press_normal_key(&mut ui, &[], ' ', KeyModifiers::NONE));
        assert!(matches!(
            ui.app.visible().get(ui.app.selected_index()),
            Some(agent_viewer_tui::app::Row::ProjectHeader {
                collapsed: true,
                ..
            })
        ));
        assert!(
            ui.composer.is_empty(),
            "collapse must not become composer text"
        );
    }

    #[test]
    fn bare_slash_starts_composer_input() {
        let mut ui = test_ui_with(vec![sess("s1", "/tmp/agentviewer-slash", 100)]);
        select_session_row(&mut ui, "s1");

        assert!(!press_normal_key(&mut ui, &[], '/', KeyModifiers::NONE));
        assert_eq!(ui.composer.text(), "/");
        assert!(matches!(ui.mode, Mode::Normal));
    }

    #[test]
    fn ctrl_a_toggles_show_all() {
        let mut ui = test_ui_with(Vec::new());

        assert!(!ui.app.show_all());
        assert!(!press_normal_key(&mut ui, &[], 'a', KeyModifiers::CONTROL));
        assert!(ui.app.show_all());
        assert!(ui.composer.is_empty());
    }

    #[test]
    fn ctrl_d_and_ctrl_u_submit_their_archive_operations() {
        for (c, operation) in [('d', "hide"), ('u', "unhide")] {
            let mut ui = test_ui_with(vec![sess("s1", "/tmp/agentviewer-archive", 100)]);
            select_session_row(&mut ui, "s1");
            let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
                vec![Box::new(ArchivingBackend)];

            assert!(!press_normal_key(
                &mut ui,
                &backends,
                c,
                KeyModifiers::CONTROL
            ));
            assert!(
                ui.mutations.in_flight(&format!("claude:s1:{operation}")),
                "ctrl+{c} must submit {operation}"
            );
            assert!(ui.composer.is_empty());
        }
    }

    #[test]
    fn archive_action_dispatches_both_rows_for_fresh_resolution() {
        let mut external = sess("external", "/tmp/external", 200);
        external.backend = BackendKind::Opencode;
        let mut managed = sess("managed", "/tmp/managed", 100);
        managed.backend = BackendKind::Opencode;
        managed.daemon_hosted = true;
        let mut ui = test_ui_with(vec![external, managed]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(RowScopedArchivingBackend)];

        select_session_row(&mut ui, "external");
        crate::actions::hide_selected(&backends, &mut ui, true);
        assert!(ui.mutations.in_flight("opencode:external:hide"));

        select_session_row(&mut ui, "managed");
        crate::actions::hide_selected(&backends, &mut ui, true);
        assert!(ui.mutations.in_flight("opencode:managed:hide"));
    }

    #[test]
    fn model_command_with_no_matches_still_captures_picker_keys() {
        // Regression: an active `/model <no-match>` must keep the picker keys captured
        // (Tab/Up/Down) even though zero suggestions match. `handle_normal_key` guards
        // Down/Up/Tab on `is_model_command()`, NOT `model_picking()`, precisely so a
        // zero-match filter does not fall through to backend-cycle (Tab) or list-nav
        // (Up/Down). The full key handler needs a live `DefaultTerminal`, so this asserts
        // the composer-level predicates the routing branches on plus that the ops the
        // handler routes to are safe no-ops in this state.
        let mut composer = Composer::new();
        composer.set_models(
            vec!["default".to_string(), "gpt-5".to_string()],
            BackendKind::Codex,
        );
        for ch in "/model zzzznomatch".chars() {
            composer.push_char(ch);
        }
        // The command is active (so the new `model_cmd` guard captures the keys) but with
        // models installed nothing matches the filter, so the picker itself is empty.
        assert!(composer.is_model_command());
        assert!(composer.model_suggestions().is_empty());
        assert!(!composer.model_picking());
        // The slash-command popup is closed for `/model`, so the handler's `suggesting`
        // guard is false; only `is_model_command()` keeps the keys captured here.
        assert!(!composer.suggestions_active());
        // The routed ops are safe no-ops: accept_model does not change the selected model
        // and Tab therefore never cycles the backend in this state.
        let before_model = composer.model().to_string();
        let before_backend = composer.backend();
        assert!(!composer.accept_model());
        assert_eq!(composer.model(), before_model);
        assert_eq!(composer.backend(), before_backend);
    }

    #[test]
    fn tab_accepts_theme_suggestion_and_opens_picker() {
        let mut ui = test_ui_with(Vec::new());
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();

        for character in "/th".chars() {
            press_normal_key(&mut ui, &backends, character, KeyModifiers::NONE);
        }
        assert!(ui.composer.suggestions().contains(&"theme"));

        press_normal_code(&mut ui, &backends, KeyCode::Tab, KeyModifiers::NONE);

        assert!(ui.composer.is_theme_command());
        assert!(ui.themes.picker_open());
    }

    #[test]
    fn theme_picker_escape_reverts_and_enter_persists() {
        use agent_viewer_core::state::ViewerDb;
        use agent_viewer_tui::ui::theme::{persist_theme, persisted_theme};

        let directory = tempfile::tempdir().expect("viewer state");
        let db = ViewerDb::open(&directory.path().join("viewer.db")).expect("viewer db");
        persist_theme(&db, "amber").expect("seed theme");
        let mut ui = test_ui_with(Vec::new());
        ui.db = Some(db);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();

        for character in "/theme".chars() {
            press_normal_key(&mut ui, &backends, character, KeyModifiers::NONE);
        }
        assert!(ui.themes.picker_open());
        press_normal_code(&mut ui, &backends, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(ui.themes.active().id, "terminal");
        press_normal_code(&mut ui, &backends, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(ui.themes.active().id, "amber");
        assert_eq!(
            persisted_theme(ui.db.as_ref().expect("viewer db")).as_deref(),
            Some("amber")
        );

        for character in "/theme".chars() {
            press_normal_key(&mut ui, &backends, character, KeyModifiers::NONE);
        }
        press_normal_code(&mut ui, &backends, KeyCode::Down, KeyModifiers::NONE);
        press_normal_code(&mut ui, &backends, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(ui.themes.active().id, "terminal");
        assert_eq!(
            persisted_theme(ui.db.as_ref().expect("viewer db")).as_deref(),
            Some("terminal")
        );
    }

    #[test]
    fn plain_c_and_other_ctrl_chords_are_not_quit() {
        // A bare 'c' types into the composer; other Ctrl-chords keep their own actions.
        let plain_c = key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!is_quit_chord(plain_c, false, &Mode::Normal));
        let ctrl_x = key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(!is_quit_chord(ctrl_x, true, &Mode::Normal));
    }
}
