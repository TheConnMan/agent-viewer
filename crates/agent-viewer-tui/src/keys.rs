//! Key routing for every input mode, plus the actions those keys trigger (attach, spawn,
//! rename, stop/remove, hide). Everything here mutates the shared `Ui` state owned by the
//! run loop in `main.rs`.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Session, Status};
use agent_viewer_core::router::AUTO_MODEL;
use agent_viewer_tui::app::{Row, Section, file_stems, subdir_names};
use agent_viewer_tui::attach::key_to_bytes;
use agent_viewer_tui::shared_listing::TargetRequest;
use agent_viewer_tui::ui::{
    Mode, PaletteAction, PaletteGroup, PaletteItem, PaletteSessionTarget, PaletteState,
    PaletteTarget, SpriteKind, TAIL_MIN_TOTAL_WIDTH,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::actions::{
    activate_selected, apply_rename, attach_selected, back_triage_item, close_triage,
    ensure_completions, ensure_models, focus_wall_tile, hide_request, hide_selected, kill_request,
    kill_selected, move_wall_selection, open_filter, open_rename, open_rename_request, open_reply,
    open_triage, scroll_wall_tile, send_reply, skip_triage_item, spawn_from_composer,
    submit_attach, toggle_group_if_header, toggle_wall,
};
use crate::{Key, Refresher, Ui};

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
    // The wall tile that has the keyboard, if it has a live child to give keys to.
    let wall_target = wall_input_target(ui);
    // Ctrl+C kills the whole viewer (like `claude agents`) unless a child is taking our keys —
    // an active attach, or a wall tile with the focus. There Ctrl+C must reach the child as an
    // interrupt (0x03) so a runaway agent can be stopped, and a half-typed line abandoned,
    // without tearing down the viewer. (macOS Cmd+C is swallowed by the terminal as copy and
    // never reaches us; Ctrl+C is the portable interrupt on macOS and Windows alike.)
    // The triage panel is a live child exactly like the attach view is, so Ctrl+C there is an
    // interrupt for the session being answered, not a request to tear the viewer down (and
    // with it every other PTY it owns). Without a child in the panel there is nothing to
    // interrupt, so the chord keeps its global meaning.
    let triage_target = matches!(ui.mode, Mode::Triage(_))
        && ui
            .focused
            .as_ref()
            .is_some_and(|key| ui.attached.contains_key(key));
    // The compose overlay counts for the same reason: the tiles under it are still live children
    // whose Ctrl+C the wall de-armed on purpose, and opening an input box over the grid must
    // not silently re-arm the chord as a teardown that kills every one of them.
    let forwarding = matches!(ui.mode, Mode::Attached)
        || wall_target.is_some()
        || triage_target
        || composing_over_wall(ui);
    if is_quit_chord(key, ctrl, forwarding) {
        ui.attached.clear(); // drop = kill owned children during viewer teardown
        return Ok(true);
    }
    // Ctrl+T toggles mouse capture in every mode. When capture is on, it provides an escape
    // hatch for terminal text selection. The child does not get this chord.
    if is_mouse_toggle_chord(key, ctrl) {
        set_mouse_capture(ui, !ui.mouse_capture);
        return Ok(false);
    }
    if matches!(ui.mode, Mode::Attached) && is_copy_chord(key, ctrl) {
        copy_attached_transcript(ui);
        return Ok(false);
    }
    // The wall is a live input surface, not a viewing one: everything outside its own few
    // reserved chords goes to the focused tile's child, so a session can be answered without
    // leaving the grid.
    if matches!(ui.mode, Mode::Normal) && ui.wall.on {
        handle_wall_key(key, ctrl, wall_target, backends, ui);
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
        Mode::Triage(_) => handle_triage_key(key, ui)?,
        // Reached from the palette while the wall is on: the composer floats over the grid and
        // takes the keyboard back off the focused tile for as long as it is up.
        Mode::Compose => handle_compose_key(key, ctrl, backends, refresher, ui),
    }
    Ok(false)
}

/// Route a mouse event. While attached, Codex wheel events scroll the local viewport because
/// Codex discards native wheel reports. Other Codex pointer events and all Claude events stay
/// native. In the list, click or hover selects the row under the cursor
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
    // On the wall the pointer drives the tile under it: hover or click gives it the keyboard,
    // and the wheel scrolls back through what it has already printed. Pointer events are not
    // forwarded into tile children — clicking around inside an embedded TUI belongs in the
    // zoomed view — so the wheel moves OUR viewport onto the child's retained output, which
    // works the same for every backend regardless of what the child does with mouse reports.
    if matches!(ui.mode, Mode::Normal) && ui.wall.on {
        ui.mouse_press = None;
        let hit = {
            let rects = ui.wall_rects.borrow();
            agent_viewer_tui::ui::wall::tile_at(&rects, me.column, me.row)
                .map(|index| (index, rects[index]))
        };
        let Some((index, rect)) = hit else {
            return MouseAction::None;
        };
        match me.kind {
            MouseEventKind::Moved | MouseEventKind::Down(MouseButton::Left) => {
                focus_wall_tile(ui, index)
            }
            // Scrolling does not steal the focus: the wheel reads, it does not select.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let content = agent_viewer_tui::ui::wall::tile_content(rect);
                scroll_wall_tile(ui, index, me, content);
            }
            _ => {}
        }
        return MouseAction::None;
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
    // A paste on the wall belongs to the focused tile, exactly like a keystroke. Without this
    // it would land in the composer that is not even drawn, so the text would appear to
    // vanish and could then be spawned as a task after leaving the wall.
    if matches!(ui.mode, Mode::Normal) && ui.wall.on {
        if let Some(fkey) = wall_input_target(ui) {
            paste_into_pty(text, &fkey, ui);
        }
        return;
    }
    match &mut ui.mode {
        Mode::Normal => ui.composer.push_str(text),
        Mode::Attached => {
            ui.pending_reply = None;
            let Some(fkey) = ui.focused.clone() else {
                return;
            };
            paste_into_pty(text, &fkey, ui);
        }
        Mode::Filter => {
            let mut filter = ui.app.filter().to_string();
            filter.push_str(&single_line_paste(text));
            ui.app.set_filter(filter);
        }
        Mode::Rename(modal) => modal.buffer.push_str(&single_line_paste(text)),
        Mode::Reply(modal) => modal.buffer.push_str(&normalize_paste(text)),
        Mode::Palette(palette) => palette.push_str(&single_line_paste(text)),
        // A paste in triage belongs to the session in the panel, exactly like a keystroke.
        Mode::Triage(_) => {
            if let Some(focused) = ui.focused.clone()
                && let Some(pty) = ui.attached.get_mut(&focused)
            {
                let mut bytes = b"\x1b[200~".to_vec();
                bytes.extend_from_slice(text.as_bytes());
                bytes.extend_from_slice(b"\x1b[201~");
                let _ = pty.write_input(&bytes);
            }
        }
        Mode::Help => {}
        // The overlay has the keyboard, so a pasted task description belongs in it and not in
        // the tile underneath — the same "text must never vanish" rule the wall guard above
        // enforces in the other direction.
        Mode::Compose => ui.composer.push_str(text),
    }
}

/// Write pasted text into one PTY, keeping its Left-gate tracker in step. Shared by the
/// attach view and the wall so both honour the child's bracketed-paste mode.
fn paste_into_pty(text: &str, fkey: &Key, ui: &mut Ui) {
    {
        let bracketed_paste = ui
            .attached
            .get(fkey)
            .is_some_and(|pty| pty.with_screen(|screen| screen.bracketed_paste()));
        if let Some(tracker) = ui.detach_trackers.get_mut(fkey) {
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
        if let Some(pty) = ui.attached.get_mut(fkey) {
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

fn is_copy_chord(key: KeyEvent, ctrl: bool) -> bool {
    ctrl && matches!(key.code, KeyCode::Char('y'))
}

fn copy_attached_transcript(ui: &mut Ui) {
    ui.pending_copy = None;
    let contents = ui
        .focused
        .as_ref()
        .and_then(|key| ui.attached.get(key))
        .map(|pty| pty.with_screen(|screen| screen.contents()));

    let Some(contents) = contents else {
        ui.set_notice(
            "copy failed: no attached transcript is available; use ctrl+t to select text"
                .to_string(),
        );
        return;
    };
    if contents.trim().is_empty() {
        ui.set_notice(
            "copy failed: the visible transcript is empty; use ctrl+t to select text".to_string(),
        );
        return;
    }

    ui.pending_copy = Some(contents);
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

/// Why the tail pane cannot open right now, if it cannot.
///
/// Below the minimum the half it takes would leave the list unreadable, so opening is
/// refused rather than turning a flag on that renders nothing. A width of 0 means the list
/// has not been drawn yet, which is not evidence of a narrow terminal. Shared by the Ctrl+B
/// chord and the palette entry so both refuse for the same reason, in the same words.
pub(crate) fn tail_refusal(ui: &Ui) -> Option<String> {
    let width = ui.list_hit.borrow().width();
    (width > 0 && width < TAIL_MIN_TOTAL_WIDTH)
        .then(|| format!("tail pane needs {TAIL_MIN_TOTAL_WIDTH} columns and this one is {width}"))
}

/// Ctrl+B opens and closes the tail pane. It is a pure view toggle: the transcript read it
/// implies happens on a background worker and never starts a process.
fn toggle_tail(ui: &mut Ui) {
    if ui.tail_open {
        ui.tail_open = false;
        ui.set_notice("tail pane off".to_string());
        return;
    }
    // An unsupported action is a footer notice, never a silent no-op.
    if let Some(reason) = tail_refusal(ui) {
        ui.set_notice(reason);
        return;
    }
    ui.tail_open = true;
    ui.set_notice("tail pane on · ⌃B to close".to_string());
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

/// Flip the age ramp, persist it, and say what happened. Under a theme with no truecolor
/// endpoint to fade toward the flag still flips (so it takes effect on the next theme change),
/// but the notice says plainly that nothing will look different here.
pub(crate) fn toggle_age_ramp(ui: &mut Ui) {
    ui.age_ramp = !ui.age_ramp;
    let state = if ui.age_ramp { "on" } else { "off" };
    if let Some(db) = &ui.db
        && let Err(error) = db.set_age_ramp(ui.age_ramp)
    {
        ui.set_notice(format!("age ramp: {state} (not saved: {error})"));
        return;
    }
    if ui.age_ramp && !ui.themes.active().supports_age_ramp() {
        ui.set_notice(format!(
            "age ramp: on · no effect under the {} theme",
            ui.themes.active().name
        ));
        return;
    }
    ui.set_notice(format!("age ramp: {state}"));
}

/// Ctrl+C is the app-wide "kill the viewer" chord, except when a child is taking our keys
/// (`forwarding`) — there it is sent on as a raw interrupt instead. Kept as a pure predicate
/// so the quit decision is unit-testable without a live terminal.
fn is_quit_chord(key: KeyEvent, ctrl: bool, forwarding: bool) -> bool {
    ctrl && matches!(key.code, KeyCode::Char('c')) && !forwarding
}

/// Ctrl+] — the chord that backs out of every full-screen surface the viewer puts up.
///
/// It arrives under two encodings: terminals send raw byte 0x1D, which crossterm's legacy unix
/// parser folds onto Char('5')+CTRL (it folds 0x1C..=0x1F onto Ctrl+'4'..'7'), while the kitty
/// keyboard protocol delivers the literal Char(']')+CTRL. Matching only the literal leaves the
/// chord dead in most terminals — which is what a live run found. The caller checks CTRL; this
/// is the key code half.
fn is_leave_chord(code: KeyCode) -> bool {
    matches!(code, KeyCode::Char(']') | KeyCode::Char('5'))
}

/// The wall's ways out, which the compose overlay honors unchanged: Ctrl+] as everywhere else,
/// plus the Ctrl+W the wall documents as its unconditional exit. A surface that swallowed
/// either would turn the grid into the trap they exist to prevent.
fn is_wall_leave_chord(code: KeyCode) -> bool {
    matches!(code, KeyCode::Char('w')) || is_leave_chord(code)
}

/// Whether the spawn composer is floating over the grid with the keyboard.
///
/// `Mode::Compose` is only ever entered from the wall's palette, so the two conditions cannot
/// disagree today. It is one predicate anyway because three separate sites — the quit-chord
/// guard here, the overlay draw, and the footer — each depend on the pair, and an entry point
/// that ever opened the composer off the wall would otherwise break all three independently.
fn composing_over_wall(ui: &Ui) -> bool {
    matches!(ui.mode, Mode::Compose) && ui.wall.on
}

/// The wall tile that should receive keystrokes: the focused one, once it has a live child.
///
/// `None` while the wall is off, when the focused tile is still connecting, and when its
/// child has exited — in all of those the viewer keeps its own keys, so Ctrl+C still quits
/// rather than vanishing into a tile that cannot use it.
fn wall_input_target(ui: &mut Ui) -> Option<(BackendKind, String)> {
    if !ui.wall.on || !matches!(ui.mode, Mode::Normal) {
        return None;
    }
    let keys = agent_viewer_tui::ui::wall::tile_keys(&ui.app, agent_viewer_core::spawn::now_ms());
    let key = keys.get(ui.wall.focus_index(&keys))?.clone();
    let live = ui
        .attached
        .get_mut(&key)
        .is_some_and(|pty| !pty.is_exited());
    live.then_some(key)
}

/// Pin the list selection onto the tile the wall is showing as focused.
///
/// The wall tracks its focus by key, separately from `app.selected()`, and every
/// session-scoped action (`kill_selected`, `palette_items`) reads the selection. Today the two
/// are already in step — `toggle_wall` pins on open, `focus_wall_tile` on hover and click, and
/// `move_wall_selection` on `Shift+arrows` — which is why `Ctrl+O` zooms correctly without
/// this. It is called anyway for the two destructive chords, because `focus_index` falls back
/// to a clamped `last_index` when the focused session stops being tiled, and that fallback
/// resolves to a DIFFERENT session. Zooming the wrong panel is a keystroke to undo; removing
/// the wrong session is not, so these two do not inherit an invariant maintained elsewhere.
fn pin_selection_to_focused_tile(ui: &mut Ui) {
    let keys = agent_viewer_tui::ui::wall::tile_keys(&ui.app, agent_viewer_core::spawn::now_ms());
    focus_wall_tile(ui, ui.wall.focus_index(&keys));
}

/// Keys on the wall. The wall reserves the few chords it needs to be navigable, escapable, and
/// closeable-out, and forwards everything else — plain arrows, Enter, Esc, Ctrl+C — to the
/// focused tile.
///
/// Esc and Ctrl+C reaching the child is the point, not an oversight: Esc interrupts Claude and
/// Ctrl+C abandons a half-typed line, and a wall you cannot interrupt from is worse than no
/// wall. `Ctrl+W` and `Ctrl+]` are the unconditional exits that keep this from being a trap.
///
/// `Ctrl+X` and `Ctrl+K` are reserved for the same reason `Ctrl+O` is: the tile you are
/// watching finish is the one you want to retire, and walking back to the list to do it is the
/// step that makes the wall feel like a viewer instead of a workspace. Both aim at the focused
/// tile, and both cost the child a chord it can no longer receive — an accepted trade, since
/// neither is load-bearing in any backend's input line.
fn handle_wall_key(
    key: KeyEvent,
    ctrl: bool,
    target: Option<(BackendKind, String)>,
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
) {
    // Shift+arrows walk the grid. Ctrl+arrows were tried first and never arrived — the host
    // terminal keeps them — so the modifier that actually reaches us is the one that wins.
    // Bare arrows stay the child's; the wall would be useless if it took them.
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        match key.code {
            KeyCode::Up => {
                move_wall_selection(ui, 0, -1);
                return;
            }
            KeyCode::Down => {
                move_wall_selection(ui, 0, 1);
                return;
            }
            KeyCode::Left => {
                move_wall_selection(ui, -1, 0);
                return;
            }
            KeyCode::Right => {
                move_wall_selection(ui, 1, 0);
                return;
            }
            _ => {}
        }
    }
    if ctrl {
        match key.code {
            code if is_wall_leave_chord(code) => {
                toggle_wall(ui);
                return;
            }
            // Zoom the focused tile to the full attach view. It reuses the wall's live PTY
            // rather than spawning a second child.
            KeyCode::Char('o') => {
                attach_selected(ui);
                return;
            }
            // Retire the focused tile in place: the list's two-stage chord, unchanged. Stop
            // once, then remove on the next press inside the arm window. A finished tile arms
            // silently on the first press, which is why the wall's footer shows the same
            // countdown hint the list does.
            KeyCode::Char('x') => {
                pin_selection_to_focused_tile(ui);
                kill_selected(backends, ui);
                return;
            }
            // The palette, scoped to the focused tile — the wall's menu for everything the
            // grid has no chord for (archive, rename, stop or remove, jumping to another
            // session). It floats over the wall, which keeps rendering underneath.
            KeyCode::Char('k') => {
                pin_selection_to_focused_tile(ui);
                open_palette(backends, ui);
                return;
            }
            _ => {}
        }
    }
    let Some(fkey) = target else {
        return;
    };
    // The user is typing into this tile, so a queued reply injection aimed at it must not be
    // typed in behind them. A reply armed at some other tile is untouched.
    if ui.pending_reply.as_ref().map(|pending| &pending.key) == Some(&fkey) {
        ui.pending_reply = None;
    }
    // Feed the same Left-gate tracker the attach view keeps. Zooming in with Ctrl+O reuses
    // this PTY, and without this the reused session would start with an "empty input line"
    // tracker — so the first Left after typing on the wall would back out instead of moving
    // the cursor, mid-edit.
    let tracker = ui.detach_trackers.entry(fkey.clone()).or_default();
    match key.code {
        KeyCode::Char(_) => tracker.on_char(),
        KeyCode::Backspace => tracker.on_backspace(),
        KeyCode::Enter => tracker.on_enter(),
        _ => {}
    }
    if let Some(bytes) = key_to_bytes(key)
        && let Some(pty) = ui.attached.get_mut(&fkey)
    {
        let _ = pty.write_input(&bytes);
    }
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
            KeyCode::Char('b') => toggle_tail(ui),
            KeyCode::Char('w') => toggle_wall(ui),
            KeyCode::Char('n') => open_triage(ui),
            KeyCode::Char('g') => cycle_sprite(ui),
            _ => {}
        }
        return Ok(false);
    }

    // An open composer popup takes Up/Down/Tab/Enter/Esc before the list ever sees them.
    if !handle_composer_popup_key(key.code, ui) {
        match key.code {
            // Arrows navigate or act at all times.
            KeyCode::Down => ui.app.move_selection(1),
            KeyCode::Up => ui.app.move_selection(-1),
            KeyCode::Right => {
                attach_selected(ui);
            }
            // Tab cycles the target backend once no popup wants it; Shift+Tab cycles that
            // backend's model.
            KeyCode::Tab => ui.composer.cycle_backend(),
            KeyCode::BackTab => ui.composer.cycle_model(),
            KeyCode::Backspace => ui.composer.backspace(),
            // The popup pass already dismissed anything that was open, so this Esc is the
            // second one: it clears the composer.
            KeyCode::Esc => ui.composer.clear(),
            KeyCode::Enter => {
                if ui.composer.is_empty() {
                    // On a group header, Enter collapses/expands the group (and persists)
                    // instead of attaching; on a session it attaches as before.
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
    }
    sync_composer_popups(ui);
    Ok(false)
}

/// Keys while the spawn composer floats over the wall. The list's composer with the list taken
/// away: same popups, same Tab/⇧Tab cycling, but no row to move onto and none to activate.
///
/// Reached from the palette rather than from a chord, and it reserves none of its own: every
/// chord the wall does not take belongs to the focused child permanently, and this overlay is
/// not worth another one.
fn handle_compose_key(
    key: KeyEvent,
    ctrl: bool,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
) {
    // Every ctrl chord below `handle_key`'s own globals (Ctrl+C, handled here, and Ctrl+T,
    // which toggles mouse capture before the mode dispatch) is inert here rather than acting:
    // the wall's chords aim at the focused tile, and firing one from behind an input box that
    // has the keyboard would act on a session the user is no longer typing at. The exceptions
    // are the exits, because every way out of this box has to keep the draft.
    if ctrl {
        match key.code {
            // Leave compose first, so `toggle_wall` closes the wall this box was floating over
            // rather than reopening it a keystroke later.
            code if is_wall_leave_chord(code) => {
                ui.mode = Mode::Normal;
                toggle_wall(ui);
            }
            // Ctrl+C is the focused tile's on the wall, never the viewer's, so it backs out of
            // the box exactly like Esc instead of tearing down every tile's child. One rule
            // for every exit — the draft survives all of them.
            KeyCode::Char('c') => ui.mode = Mode::Normal,
            _ => {}
        }
        return;
    }
    // Same up-front refresh the list does, and for the same reason: nothing may read
    // `suggestions_active` against commands scanned for a target that has since moved. Below
    // the ctrl branch because none of those chords reads either, and the rescan costs a
    // `spawn_target()` and a `PathBuf` every time one is pressed over the grid.
    ensure_completions(ui);
    ensure_models(ui);
    if !handle_composer_popup_key(key.code, ui) {
        match key.code {
            // Up/Down deliberately do nothing with no popup open. The wall pins the list
            // selection to the focused tile and `spawn_target()` reads it, so moving it here
            // would silently repoint the directory this new session lands in.
            KeyCode::Tab => ui.composer.cycle_backend(),
            KeyCode::BackTab => ui.composer.cycle_model(),
            KeyCode::Backspace => ui.composer.backspace(),
            // Nothing left to dismiss, so Esc hands the keyboard back to the grid — and keeps
            // the draft. Losing a long task description to a glance at a tile is the failure
            // this avoids; the text is still there the next time the overlay opens.
            KeyCode::Esc => ui.mode = Mode::Normal,
            // An empty composer has nothing to send and no list row underneath to activate, so
            // Enter simply stays put. A refused spawn (no target directory, a backend that
            // cannot spawn, a deduped double Enter) keeps the box open too: leaving would strand
            // the draft behind the grid, where it is neither drawn nor recoverable.
            KeyCode::Enter if !ui.composer.is_empty() => {
                // Bound rather than tested inline: the spawn is the side effect, and a match
                // guard is the wrong place to hide one.
                let submitted = spawn_from_composer(backends, refresher, ui);
                if submitted {
                    ui.mode = Mode::Normal;
                }
            }
            // Every printable is task text. The list's empty-composer hotkeys ('?' for help,
            // Space on a header) do not apply: there is no list under this box.
            KeyCode::Char(c) => ui.composer.push_char(c),
            _ => {}
        }
    }
    sync_composer_popups(ui);
}

/// Give an open composer popup — the slash-command list, the `/model` picker, the theme picker
/// — first refusal on this key. Returns `true` when it took it.
///
/// Shared by the list and the wall's compose overlay because these guards are an interlock, not
/// a lookup table: the `/model` arms key off `is_model_command()` rather than `model_picking()`
/// so a `/model <no-match>` still swallows Tab and the arrows instead of cycling the backend or
/// moving the session selection mid-command, and the theme arms have to beat both. A second
/// copy would drift the first time one of them changed.
fn handle_composer_popup_key(code: KeyCode, ui: &mut Ui) -> bool {
    let suggesting = ui.composer.suggestions_active();
    let model_cmd = ui.composer.is_model_command();
    let theme_cmd = ui.composer.is_theme_command();
    match code {
        KeyCode::Down if theme_cmd => ui.themes.move_preview(1),
        KeyCode::Up if theme_cmd => ui.themes.move_preview(-1),
        // Safe no-ops when the active list is empty, which is why they can capture the key
        // outright.
        KeyCode::Down if suggesting || model_cmd => ui.composer.move_suggestion(1),
        KeyCode::Up if suggesting || model_cmd => ui.composer.move_suggestion(-1),
        // Tab accepts the highlighted suggestion or model while a popup is open.
        KeyCode::Tab if suggesting => {
            ui.composer.accept_suggestion();
        }
        KeyCode::Tab if model_cmd => {
            ui.composer.accept_model();
        }
        KeyCode::Tab if theme_cmd => {}
        // Esc dismisses the popup; what a second Esc means is the caller's business.
        KeyCode::Esc if theme_cmd => {
            ui.themes.cancel_picker();
            ui.composer.clear();
        }
        KeyCode::Esc if suggesting => ui.composer.dismiss_suggestions(),
        KeyCode::Enter if theme_cmd => {
            let theme_id = ui.themes.commit_picker().to_string();
            if let Some(db) = &ui.db
                && let Err(error) = agent_viewer_tui::ui::theme::persist_theme(db, &theme_id)
            {
                ui.set_notice(format!("could not persist theme: {error}"));
            }
            ui.composer.clear();
        }
        // A /model picker is up: Enter picks the highlighted model.
        KeyCode::Enter if ui.composer.model_picking() => {
            ui.composer.accept_model();
        }
        // A /model command with no matches: a meta-command, nothing to spawn.
        KeyCode::Enter if model_cmd => {}
        _ => return false,
    }
    true
}

/// Open or cancel the theme picker to match what the composer now holds, then re-scan the
/// completion lists for the (possibly new) backend, target, and text. The tail every composer
/// keystroke ends with, wherever it was typed.
fn sync_composer_popups(ui: &mut Ui) {
    if ui.composer.is_theme_command() {
        ui.themes.open_picker();
    } else if ui.themes.picker_open() {
        ui.themes.cancel_picker();
    }
    ensure_completions(ui);
    ensure_models(ui);
}

fn open_palette(backends: &[Box<dyn Backend>], ui: &mut Ui) {
    let items = palette_items(backends, ui);
    // Every ACTION row is built for the row selected right now, so that identity is captured
    // with them: a refresh landing while the palette is up can clamp the selection onto a
    // different session, and Archive/Stop/Delete must never follow it there.
    let action_target = ui.app.selected().map(|session| PaletteSessionTarget {
        backend: session.backend,
        id: session.id.clone(),
        title: session.title.clone(),
    });
    ui.mode = Mode::Palette(PaletteState::new(items).with_action_target(action_target));
}

fn palette_items(backends: &[Box<dyn Backend>], ui: &Ui) -> Vec<PaletteItem> {
    let selected = ui.app.selected();
    let capabilities = selected
        .and_then(|session| {
            backend_of(backends, session.backend).map(|b| b.capabilities_for(session))
        })
        .unwrap_or_else(Capabilities::none);
    let mut items = palette_action_items(selected, capabilities, ui, ui.tail_open);
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

    for backend in ui.composer.available_backends().iter().copied() {
        let mut models = ui
            .models
            .models(backend)
            .map(<[String]>::to_vec)
            .unwrap_or_else(|| vec![backend.default_model().to_string()]);
        if !ui.composer.is_auto()
            && backend == ui.composer.backend()
            && !models.iter().any(|model| model == ui.composer.model())
        {
            models.push(ui.composer.model().to_string());
        }
        if ui.composer.router_available() {
            models.retain(|model| {
                model != AUTO_MODEL
                    && !(backend == BackendKind::Codex && model == backend.default_model())
            });
        }
        models.sort();
        models.dedup();
        if ui.composer.router_available() {
            models.insert(0, AUTO_MODEL.to_string());
        }
        items.extend(models.into_iter().map(|name| {
            let detail = if name == AUTO_MODEL {
                format!("pin {} · keep model and effort automatic", backend.name())
            } else {
                format!("{} · set composer model", backend.name())
            };
            PaletteItem::new(
                PaletteGroup::Models,
                "◇",
                name.clone(),
                detail,
                None,
                true,
                None,
                PaletteTarget::Model { backend, name },
            )
        }));
    }

    let spawn_target = ui.app.spawn_target();
    let command_backend = (!ui.composer.is_auto()).then(|| ui.composer.backend());
    items.extend(
        palette_commands(
            command_backend,
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
                format!("{} slash command", ui.composer.provider_name()),
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
    ui: &Ui,
    tail_open: bool,
) -> Vec<PaletteItem> {
    let is_claude_without_archive = selected
        .is_some_and(|session| session.backend == BackendKind::Claude && !capabilities.archive);
    let mut items = vec![action_item(
        PaletteAction::Attach,
        "Attach session",
        "open the selected session",
        Some("⏎"),
        capability_reason(selected, capabilities.attach, "attach"),
    )];
    if !is_claude_without_archive {
        items.push(action_item(
            PaletteAction::Archive,
            "Archive session",
            "hide the selected session",
            Some("⌃D"),
            selected
                .filter(|session| session.hidden)
                .map(|_| "unavailable · session is already archived".to_string())
                .or_else(|| capability_reason(selected, capabilities.archive, "archive")),
        ));
    }
    items.extend([
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
            PaletteAction::Triage,
            "Triage sessions waiting for input",
            "walk the needs-input queue, longest wait first",
            Some("⌃N"),
            // Not gated on the selected row: triage is a queue over every session, so the
            // only thing that can disable it is an empty queue, which the action reports
            // itself as a footer notice.
            None,
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
        // No chord: the palette is the only entry point for this one, by design.
        action_item(
            PaletteAction::AgeRamp,
            "Age ramp",
            "fade finished sessions as they age",
            None,
            None,
        ),
        action_item(
            PaletteAction::TailPane,
            if tail_open {
                "Hide tail pane"
            } else {
                "Show tail pane"
            },
            if tail_open {
                "showing now · give the columns back to the list"
            } else {
                "tail the selected session's last turns beside the list"
            },
            Some("⌃B"),
            // Only the open direction can be refused; closing always works, even if the
            // terminal was resized narrow while the pane was up.
            if tail_open { None } else { tail_refusal(ui) },
        ),
    ]);
    // Wall only, because starting a task is the one thing the grid otherwise cannot do: the
    // composer is not drawn there, so every spawn used to mean leaving the wall and coming
    // back. Off the wall the composer is already on screen with the keyboard, so the entry
    // would be a row that does nothing. No chord: the palette is its only entry point, which
    // is what keeps it from costing the tiles' children another one. Position here is not
    // rank — `PaletteState::rank` orders the Actions group alphabetically.
    if ui.wall.on {
        items.push(action_item(
            PaletteAction::Spawn,
            "New session",
            "compose a task and start it on this wall",
            None,
            None,
        ));
    }
    items
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

/// The palette's slash commands: the viewer's own `/model` and `/theme` always, plus the
/// backend's scanned commands. `backend` is None under Auto, where the provider is unknown until
/// the router answers, so no backend-specific command is offered (matching the typed `/` popup).
fn palette_commands(backend: Option<BackendKind>, target: Option<&std::path::Path>) -> Vec<String> {
    let home = agent_viewer_core::home_dir();
    let mut commands = match backend {
        Some(BackendKind::Claude) => {
            let mut commands = subdir_names(&home.join(".claude/skills"));
            if let Some(target) = target {
                commands.extend(subdir_names(&target.join(".claude/skills")));
            }
            commands
        }
        Some(BackendKind::Codex) => file_stems(&home.join(".codex/prompts")),
        None => Vec::new(),
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

/// What Enter on a palette row resolves to, once the row's session identity is re-checked
/// against the listing as it stands now.
enum PaletteExecution {
    /// Not a row that acts on a session.
    Other,
    /// Run `action` against this session.
    Session {
        request: TargetRequest,
        title: String,
        action: PaletteAction,
    },
    /// The session the row was built for has left the listing since the palette opened.
    Gone { title: String },
}

fn cached_palette_target(ui: &Ui, target: &PaletteTarget) -> PaletteExecution {
    match target {
        PaletteTarget::Action(action) => {
            let action = match action {
                PaletteAction::Attach
                | PaletteAction::Archive
                | PaletteAction::Unarchive
                | PaletteAction::Rename
                | PaletteAction::StopOrRemove => *action,
                _ => return PaletteExecution::Other,
            };
            // The row the palette was OPENED on, never `selected()` as it stands now: a
            // background refresh can drop that row and clamp the selection onto another
            // session, and archiving or stopping that one is destructive and unasked for.
            let Mode::Palette(state) = &ui.mode else {
                return PaletteExecution::Other;
            };
            let Some(captured) = state.action_target() else {
                return PaletteExecution::Other;
            };
            if ui
                .app
                .session_for(&(captured.backend, captured.id.clone()))
                .is_none()
            {
                return PaletteExecution::Gone {
                    title: captured.title.clone(),
                };
            }
            PaletteExecution::Session {
                request: TargetRequest::new(captured.backend, captured.id.clone()),
                title: captured.title.clone(),
                action,
            }
        }
        PaletteTarget::Session { backend, id } => {
            let title = ui
                .app
                .session_for(&(*backend, id.clone()))
                .map(|session| session.title.clone())
                .unwrap_or_else(|| id.clone());
            PaletteExecution::Session {
                request: TargetRequest::new(*backend, id.clone()),
                title,
                action: PaletteAction::Attach,
            }
        }
        _ => PaletteExecution::Other,
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
        PaletteAction::Archive => hide_request(ui, request, title, true),
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
    match cached_palette_target(ui, &item.target) {
        PaletteExecution::Session {
            request,
            title,
            action,
        } => {
            // Put the cursor back on the row the palette acted on, whichever group it came
            // from: it is where the user's attention is, and the attach path only lands a
            // resolved plan onto the session that is still selected.
            let _ = ui
                .app
                .select_by_key(&(request.backend(), request.id().to_string()));
            ui.mode = Mode::Normal;
            execute_cached_palette_action(ui, request, title, action);
            return Ok(());
        }
        PaletteExecution::Gone { title } => {
            ui.mode = Mode::Normal;
            ui.set_notice(format!("{title} is no longer listed"));
            return Ok(());
        }
        PaletteExecution::Other => {}
    }
    if !item.enabled {
        if let Some(reason) = item.disabled_reason {
            ui.set_notice(reason);
        }
        return Ok(());
    }

    // The one expression that decides Compose, applied after the `Mode::Normal` below that
    // every palette pick passes through. Commands and Models only write composer state, and
    // the wall does not draw the composer, so on the grid they used to land somewhere nothing
    // on screen showed — a pick the user only discovered after leaving the wall. Spawn is here
    // because opening that box is the whole action; it is a wall-only entry, so the `wall.on`
    // gate is what it already implies.
    let opens_composer = ui.wall.on
        && matches!(
            item.target,
            PaletteTarget::Command(_)
                | PaletteTarget::Model { .. }
                | PaletteTarget::Action(PaletteAction::Spawn)
        );

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
            PaletteAction::Triage => open_triage(ui),
            PaletteAction::StopOrRemove => kill_selected(backends, ui),
            PaletteAction::ShowAll => ui.app.toggle_show_all(),
            PaletteAction::Group => ui.app.toggle_group_mode(),
            PaletteAction::Filter => open_filter(ui),
            PaletteAction::AgeRamp => toggle_age_ramp(ui),
            PaletteAction::TailPane => toggle_tail(ui),
            // Nothing to do: `opens_composer` below is what hands over the keyboard.
            PaletteAction::Spawn => {}
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
    if opens_composer {
        ui.mode = Mode::Compose;
    }
    Ok(())
}

fn select_palette_model(ui: &mut Ui, backend: BackendKind, name: String) {
    let mut models = ui
        .models
        .models(backend)
        .map(<[String]>::to_vec)
        .unwrap_or_else(|| vec![backend.default_model().to_string()]);
    if !ui.composer.is_auto()
        && backend == ui.composer.backend()
        && !models.iter().any(|model| model == ui.composer.model())
    {
        models.push(ui.composer.model().to_string());
    }
    // Palette rows outlive an asynchronous catalog refresh, so retain the target the user saw.
    if !models.iter().any(|model| model == &name) {
        models.push(name.clone());
    }
    ui.composer.select_backend(backend);
    ui.composer.set_models(models, backend);
    let _ = ui.composer.select_model(&name);
}

/// While attached: Ctrl+] always leaves; Left leaves when the input line is empty (else it
/// is forwarded); a dead child leaves on any key; everything else is encoded
/// to bytes and written to the PTY.
fn handle_attached_key(key: KeyEvent, ui: &mut Ui) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(fkey) = ui.focused.clone() else {
        close_attached(ui);
        return;
    };

    // Any key here is the user taking over, so cancel a pending reply injection on this
    // attach (do not type our queued reply in behind the user's own input).
    ui.pending_reply = None;

    // Ctrl+] always leaves, closing the connection, so the header/help "ctrl+]" is honored here.
    if ctrl && is_leave_chord(key.code) {
        close_attached(ui);
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
        close_attached(ui);
        return;
    }

    // Left leaves only when this PTY's input line is empty; otherwise forward it as cursor
    // motion. The tracker dies with its PTY, so each visit to a session starts clean.
    if matches!(key.code, KeyCode::Left)
        && ui
            .detach_trackers
            .get(&fkey)
            .is_none_or(|t| t.detach_on_left())
    {
        close_attached(ui);
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

/// Leave the attached session and close its connection.
///
/// A session is connected exactly while it is on screen; there is no "running but not shown"
/// state to reason about. The one exception is a wall tile you zoomed into: the wall still
/// owns that connection and is about to draw it again, so it stays open and the wall closes
/// it with the rest when it closes.
fn close_attached(ui: &mut Ui) {
    if let Some(key) = ui.focused.take() {
        if ui.wall.owns(&key) {
            // Zooming resized this child to the full screen. Forget the recorded tile size so
            // the wall's next frame resizes it back down instead of matching a stale entry.
            ui.wall.sized.remove(&key);
        } else {
            ui.remove_pty(&key);
        }
    }
    ui.mode = Mode::Normal;
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

/// Triage-inbox key handling. Digits submit their quick option outright, Enter submits the
/// typed answer (or the highlighted option), the arrows walk the queue and the option list, and
/// Esc leaves the queue — exactly one level, back to the list.
///
/// Nothing here touches `ui.composer` or the list selection: walking the inbox is not walking
/// the list, and the composer must hold whatever was in it when Ctrl+N was pressed.
/// Triage key routing: three reserved chords drive the queue, EVERY other key is written to
/// the attached child.
///
/// The panel is a real session, so the agent's own input handling is the answer path — there
/// is no payload, no option parsing, and no second delivery shape. That is also why the
/// reserved set is this small: every chord taken here is a chord the session can never see,
/// and the session is the thing you came to talk to.
fn handle_triage_key(key: KeyEvent, ui: &mut Ui) -> io::Result<()> {
    match triage_command(key) {
        Some(TriageCommand::Next) => skip_triage_item(ui),
        Some(TriageCommand::Previous) => back_triage_item(ui),
        Some(TriageCommand::Leave) => close_triage(ui),
        None => forward_to_triage_child(key, ui),
    }
    Ok(())
}

/// The three chords triage keeps for itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TriageCommand {
    Next,
    Previous,
    Leave,
}

/// PURE: the queue command a keystroke means, or None when it belongs to the session.
///
/// Ctrl+] already means "detach" in the full-screen attach view, so it means "leave" here too.
/// Ctrl+N is the chord that opened the modal, which makes it the obvious "next". Bare Esc,
/// Enter, arrows and digits are all deliberately NOT reserved: they are how you answer a
/// prompt, and stealing them would break the thing the panel exists for.
fn triage_command(key: KeyEvent) -> Option<TriageCommand> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Char('n') => Some(TriageCommand::Next),
        KeyCode::Char('p') => Some(TriageCommand::Previous),
        code if is_leave_chord(code) => Some(TriageCommand::Leave),
        _ => None,
    }
}

/// Write a keystroke to the child in the panel, using the same encoder the full-screen attach
/// view uses. No child (still attaching, or the attach failed) drops the keystroke rather than
/// buffering it: replaying a burst of typing into a session that appears later would deliver
/// an answer the user could not see themselves typing.
fn forward_to_triage_child(key: KeyEvent, ui: &mut Ui) {
    let Some(bytes) = key_to_bytes(key) else {
        return;
    };
    let Some(focused) = ui.focused.clone() else {
        return;
    };
    if let Some(pty) = ui.attached.get_mut(&focused) {
        let _ = pty.write_input(&bytes);
    }
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
        handle_attached_key, handle_key, handle_mouse, handle_mouse_event, handle_palette_key,
        handle_paste, handle_rename_key, handle_triage_key, is_quit_chord, open_filter,
        open_palette, palette_items, set_mouse_capture, wall_input_target,
    };
    use crate::{NoticeState, Ui};
    use agent_viewer_core::pty::{PtySession, PtySpec, VIEWPORT_SCROLLBACK_ROWS};
    use agent_viewer_core::router::AUTO_MODEL;
    use agent_viewer_core::{BackendKind, Session, Status};
    use agent_viewer_tui::app::{App, Composer, DetachTracker, GroupKey, GroupMode, Row, Section};
    use agent_viewer_tui::mutations::{AttachRunner, MutationOutcome, MutationRunner};
    use agent_viewer_tui::ui::TriageState;
    use agent_viewer_tui::ui::{AttachView, Draw, Mode, PaletteAction, PaletteTarget, Pulses};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use std::collections::{HashMap, HashSet};
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

        // Routes through `handle_key`, not `handle_normal_key`: the wall and the quit chord
        // are decided before mode dispatch, so a harness that skipped it would test a path
        // no keystroke ever takes.
        handle_key(
            key(code, modifiers),
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
            pending_copy: None,
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
            age_ramp: false,
            tail_open: false,
            tail: None,
            tail_pending: None,
            wall: agent_viewer_tui::ui::WallState::default(),
            wall_rects: std::cell::RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn seed_mouse_press_for_reconciliation(ui: &mut Ui) {
        ui.mouse_press = Some(super::MousePress {
            target: MouseTarget::StateHeader(Section::Done),
            column: 1,
            row: 1,
        });
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
                        age_ramp: ui.age_ramp,
                        tail: None,
                        wall: None,
                        wall_rects: &ui.wall_rects,
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
                        age_ramp: ui.age_ramp,
                        tail: None,
                        wall: None,
                        wall_rects: &ui.wall_rects,
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

    /// A child that turns on SGR mouse tracking and then echoes every byte it receives,
    /// with escapes visible (`cat -v` renders ESC as `^[`). Unlike `mouse_recording_pty`
    /// there is no capture window to race, so an assertion here is about routing rather
    /// than timing.
    fn mouse_echoing_pty() -> agent_viewer_core::pty::PtySession {
        agent_viewer_core::pty::PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "stty raw -echo; printf '\\033[?1000h\\033[?1006hREADY\\r\\n'; cat -v".to_string(),
            ],
            cwd: None,
            envs: Vec::new(),
            rows: 24,
            cols: 80,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("mouse echoing child")
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

    /// Unwrap a drained attach result as a user attach. Every attach these tests submit goes
    /// through `submit_attach`, so a `Wall` outcome here would mean the runner crossed wires.
    fn focus_plan(outcome: crate::AttachOutcome) -> crate::ops::AttachPlan {
        match outcome {
            crate::AttachOutcome::Focus { plan, .. } => plan.expect("resolved attach plan"),
            crate::AttachOutcome::Wall { .. } => panic!("expected a focus attach, got a wall join"),
        }
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
            subagent: false,
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
            BackendKind::Codex
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
                    session.backend = BackendKind::Codex;
                    session
                })
                .collect())
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<&str>,
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
            _effort: Option<&str>,
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
                BackendKind::Claude => concat!(
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
        ui.composer.set_auto_available(true);
        ui.models.seed(
            BackendKind::Claude,
            vec!["opus[1m]".to_string(), "sonnet-5".to_string()],
            true,
        );

        ensure_models(&mut ui);
        for c in "/model".chars() {
            ui.composer.push_char(c);
        }

        assert_eq!(ui.composer.models_key(), Some(BackendKind::Claude));
        assert_eq!(ui.composer.model(), AUTO_MODEL);
        assert_eq!(
            ui.composer.model_suggestions(),
            vec![
                AUTO_MODEL.to_string(),
                "opus[1m]".to_string(),
                "sonnet-5".to_string(),
            ]
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
        ui.composer.set_auto_available(true);
        ui.models.request_with(BackendKind::Claude, || {
            vec!["opus[1m]".to_string(), "kimi-k3".to_string()]
        });

        ensure_models(&mut ui);
        for c in "/model kimi".chars() {
            ui.composer.push_char(c);
        }
        assert_eq!(ui.composer.model(), AUTO_MODEL);
        assert!(ui.composer.model_suggestions().is_empty());

        let start = Instant::now();
        while ui.composer.model_suggestions().is_empty() && start.elapsed() < Duration::from_secs(5)
        {
            install_models(&mut ui);
        }

        assert_eq!(ui.composer.model_suggestions(), vec!["kimi-k3".to_string()]);
        assert_eq!(ui.composer.model(), AUTO_MODEL);
        ui.composer.clear();
        ui.composer.push_str("/model");
        assert_eq!(
            ui.composer.model_suggestions(),
            vec![
                AUTO_MODEL.to_string(),
                "opus[1m]".to_string(),
                "kimi-k3".to_string(),
            ]
        );
    }

    /// A live working session with a real PTY already in `attached` — the only shape that
    /// earns a wall tile.
    fn wall_tile_pty() -> PtySession {
        PtySession::spawn(PtySpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 30".to_string()],
            cwd: None,
            envs: Vec::new(),
            rows: 6,
            cols: 40,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("wall tile child")
    }

    /// The same tile, but with a child that echoes. `cat` sends every byte it receives back
    /// through the tty line discipline, so what is on the child's screen is direct evidence of
    /// what actually reached its pty — which is what lets a test assert routing rather than
    /// intent, in both directions.
    fn echoing_wall_tile_pty() -> PtySession {
        PtySession::spawn(PtySpec {
            program: "cat".to_string(),
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            rows: 6,
            cols: 40,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("echoing tile child")
    }

    fn kill_attached(ui: &mut Ui) {
        for (_, mut pty) in std::mem::take(&mut ui.attached) {
            pty.kill();
        }
    }

    /// The index the wall's focus currently resolves to. The focus is stored by key, so a
    /// test must ask the same question the input path does rather than reading a field.
    fn wall_focus(ui: &Ui) -> usize {
        let keys =
            agent_viewer_tui::ui::wall::tile_keys(&ui.app, agent_viewer_core::spawn::now_ms());
        ui.wall.focus_index(&keys)
    }

    fn wall_ui_with_one_live_tile() -> Ui {
        let mut session = sess("live-tile", "/tmp/agentviewer-wall", 100);
        session.status = Status::Working;
        let key = (session.backend, session.id.clone());
        let mut ui = test_ui_with(vec![session]);
        ui.attached.insert(key, wall_tile_pty());
        ui
    }

    /// The whole point of the rework: what you type on the wall reaches the session you are
    /// looking at. `cat` echoes through the tty line discipline, so the bytes showing up on
    /// the child's screen is proof they were written to the child's pty and not somewhere
    /// else (the composer, a notice, the void).
    #[test]
    fn typing_on_the_wall_reaches_the_focused_tile() {
        let mut session = sess("typed-into", "/tmp/agentviewer-wall", 100);
        session.status = Status::Working;
        let target = (session.backend, session.id.clone());
        let mut ui = test_ui_with(vec![session]);
        ui.attached.insert(target.clone(), echoing_wall_tile_pty());
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        assert!(ui.wall.on);

        for c in "hello".chars() {
            press_normal_key(&mut ui, &[], c, KeyModifiers::NONE);
        }

        wait_for_pty_screen(&ui, &target, "hello");
        assert_eq!(
            ui.composer.text(),
            "",
            "the keystrokes must not have gone to the composer as well"
        );
        kill_attached(&mut ui);
    }

    /// Hover puts the keyboard on whatever is under the pointer, which is what makes "point
    /// at a session and type into it" work.
    #[test]
    fn hovering_a_tile_focuses_it() {
        let mut first = sess("tile-a", "/tmp/agentviewer-wall", 100);
        first.status = Status::Working;
        let mut second = sess("tile-b", "/tmp/agentviewer-wall", 200);
        second.status = Status::Working;
        let keys = [
            (first.backend, first.id.clone()),
            (second.backend, second.id.clone()),
        ];
        let mut ui = test_ui_with(vec![first, second]);
        for key in &keys {
            ui.attached.insert(key.clone(), wall_tile_pty());
        }
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        // Two tiles side by side, as the last frame would have published them.
        *ui.wall_rects.borrow_mut() = vec![
            ratatui::layout::Rect::new(0, 0, 40, 10),
            ratatui::layout::Rect::new(40, 0, 40, 10),
        ];
        assert_eq!(wall_focus(&ui), 0);

        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 55,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            &mut ui,
        );
        assert_eq!(wall_focus(&ui), 1, "hover must focus the tile under it");

        // A click on the first tile focuses it back, for terminals that report no motion.
        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            &mut ui,
        );
        assert_eq!(wall_focus(&ui), 0);

        // Off every tile, the focus stays where it was rather than jumping to a default.
        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 5,
                row: 40,
                modifiers: KeyModifiers::NONE,
            },
            &mut ui,
        );
        assert_eq!(wall_focus(&ui), 0);
        kill_attached(&mut ui);
    }

    /// A tile that ages out of the recency window must take its connection with it. Left
    /// alone, the child sits there invisible until the wall closes, and the MAX_TILES process
    /// budget drifts upward as expired slots are refilled.
    #[test]
    fn a_tile_leaving_the_wall_closes_its_connection() {
        let mut staying = sess("staying", "/tmp/agentviewer-wall", 100);
        staying.status = Status::Working;
        let mut leaving = sess("leaving", "/tmp/agentviewer-wall", 200);
        leaving.status = Status::Working;
        let leaving_key = (leaving.backend, leaving.id.clone());
        let mut ui = test_ui_with(vec![staying, leaving.clone()]);
        for key in [
            (BackendKind::Claude, "staying".to_string()),
            leaving_key.clone(),
        ] {
            ui.attached.insert(key.clone(), wall_tile_pty());
            ui.wall.requested.insert(key);
        }
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        ui.wall.requested.insert(leaving_key.clone());
        assert!(ui.attached.contains_key(&leaving_key));

        // The session stops and its last activity ages past the window, so it is no longer
        // tiled. Everything else about the wall is unchanged.
        let mut stopped = leaving;
        stopped.status = Status::Done;
        stopped.updated_at_ms =
            agent_viewer_core::spawn::now_ms() - agent_viewer_tui::ui::wall::RECENT_MS - 60_000;
        let staying_session = ui
            .app
            .session_for(&(BackendKind::Claude, "staying".to_string()))
            .cloned()
            .expect("staying session");
        ui.app.set_sessions(vec![staying_session, stopped]);
        crate::prune_wall_tiles(&mut ui, agent_viewer_core::spawn::now_ms());

        assert!(
            !ui.attached.contains_key(&leaving_key),
            "an expired tile kept its child alive"
        );
        assert!(
            !ui.wall.requested.contains(&leaving_key),
            "an expired tile stayed in the ownership set, so a join could still land"
        );
        assert!(
            ui.attached
                .contains_key(&(BackendKind::Claude, "staying".to_string())),
            "pruning must not touch a tile that is still on the wall"
        );
        kill_attached(&mut ui);
    }

    /// Paste is input too. On the wall it must reach the focused tile, not the composer that
    /// is not even drawn — text landing there vanishes, then spawns as a task later.
    #[test]
    fn pasting_on_the_wall_reaches_the_focused_tile() {
        let mut session = sess("pasted-into", "/tmp/agentviewer-wall", 100);
        session.status = Status::Working;
        let target = (session.backend, session.id.clone());
        let mut ui = test_ui_with(vec![session]);
        ui.attached.insert(target.clone(), echoing_wall_tile_pty());
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);

        handle_paste("pasted", &mut ui);

        wait_for_pty_screen(&ui, &target, "pasted");
        assert_eq!(
            ui.composer.text(),
            "",
            "the paste must not have gone into the hidden composer"
        );
        kill_attached(&mut ui);
    }

    /// Zooming in with Ctrl+O reuses the wall's PTY, so the Left-gate tracker has to already
    /// know the line is mid-edit. Otherwise the first Left backs out instead of moving the
    /// cursor, right after you typed something.
    #[test]
    fn typing_on_the_wall_arms_the_left_gate_for_the_zoomed_view() {
        let mut ui = wall_ui_with_one_live_tile();
        let key = (BackendKind::Claude, "live-tile".to_string());
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);

        press_normal_key(&mut ui, &[], 'x', KeyModifiers::NONE);

        assert!(
            ui.detach_trackers
                .get(&key)
                .is_some_and(|tracker| !tracker.detach_on_left()),
            "a half-typed wall line must not read as an empty input line"
        );
        kill_attached(&mut ui);
    }

    /// The wheel scrolls the tile under the pointer back over its retained output, and does
    /// not steal the focus while doing it — reading a tile is not selecting it.
    #[test]
    fn the_wheel_scrolls_the_tile_under_the_pointer_without_focusing_it() {
        let mut first = sess("tile-a", "/tmp/agentviewer-wall", 100);
        first.status = Status::Working;
        let mut second = sess("tile-b", "/tmp/agentviewer-wall", 200);
        second.status = Status::Working;
        let second_key = (second.backend, second.id.clone());
        let mut ui = test_ui_with(vec![first, second]);
        // A child that prints far more than the tile is tall, so there is history to scroll.
        for key in [
            (BackendKind::Claude, "tile-a".to_string()),
            second_key.clone(),
        ] {
            ui.attached.insert(
                key,
                PtySession::spawn(PtySpec {
                    program: "sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        "i=0; while [ $i -lt 60 ]; do echo line-$i; i=$((i+1)); done; sleep 30"
                            .to_string(),
                    ],
                    cwd: None,
                    envs: Vec::new(),
                    rows: 6,
                    cols: 40,
                    palette: None,
                    scrollback_rows: agent_viewer_core::pty::VIEWPORT_SCROLLBACK_ROWS,
                })
                .expect("scrollable tile child"),
            );
        }
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        wait_for_pty_screen(&ui, &second_key, "line-59");
        *ui.wall_rects.borrow_mut() = vec![
            ratatui::layout::Rect::new(0, 0, 40, 10),
            ratatui::layout::Rect::new(40, 0, 40, 10),
        ];

        // Wheel up over the SECOND tile while the FIRST has the focus. This child does not
        // track the mouse, so the fallback path moves the viewer's own viewport.
        assert_eq!(wall_focus(&ui), 0);
        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 55,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            &mut ui,
        );

        assert_eq!(
            wall_focus(&ui),
            0,
            "the wheel must not move the keyboard focus"
        );
        let scrolled = ui.attached[&second_key].with_screen(|screen| screen.contents());
        assert!(
            !scrolled.contains("line-59"),
            "the tile did not scroll back off the live tail: {scrolled:?}"
        );

        // Wheel back down returns it to the live tail.
        for _ in 0..3 {
            handle_mouse(
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: 55,
                    row: 4,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ui,
            );
        }
        let live = ui.attached[&second_key].with_screen(|screen| screen.contents());
        assert!(live.contains("line-59"), "the tile did not return to live");
        kill_attached(&mut ui);
    }

    /// The case that matters in practice: `claude attach` runs in the alternate screen and
    /// tracks the mouse, so it owns its scrollback and only a forwarded wheel report scrolls
    /// it. The report must arrive in the CHILD's coordinates, not the wall's — a tile is
    /// offset into the grid, and the child thinks it is alone on a terminal its own size.
    #[test]
    fn a_mouse_tracking_tile_gets_a_wheel_report_in_its_own_coordinates() {
        let mut session = sess("tracks-mouse", "/tmp/agentviewer-wall", 100);
        session.status = Status::Working;
        let target = (session.backend, session.id.clone());
        let mut ui = test_ui_with(vec![session]);
        // Turn on SGR mouse tracking, then echo whatever reports arrive back onto the screen.
        ui.attached.insert(target.clone(), mouse_echoing_pty());
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        let rect = ratatui::layout::Rect::new(40, 6, 40, 12);
        *ui.wall_rects.borrow_mut() = vec![rect];
        let content = agent_viewer_tui::ui::wall::tile_content(rect);
        assert_eq!((content.x, content.y), (41, 8));
        // The fixture only records once it has enabled tracking and started reading.
        wait_for_pty_screen(&ui, &target, "READY");

        // Wheel up three cells right and two cells down inside the child's own area.
        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: content.x + 3,
                row: content.y + 2,
                modifiers: KeyModifiers::NONE,
            },
            &mut ui,
        );

        // The fixture echoes the raw bytes as hex. SGR wheel-up is button 64 and the child's
        // own 1-based cell is (4, 3), so it must receive ESC [ < 6 4 ; 4 ; 3 — proof the
        // report was both forwarded and translated out of the wall's coordinates.
        // `cat -v` shows ESC as `^[`. SGR wheel-up is button 64 and the child's own 1-based
        // cell is (4, 3) — proof the report was forwarded AND translated out of the wall's
        // coordinates into the child's.
        wait_for_pty_screen(&ui, &target, "^[[<64;4;3M");
        kill_attached(&mut ui);
        kill_attached(&mut ui);
    }

    #[test]
    fn ctrl_w_toggles_the_video_wall() {
        let mut ui = wall_ui_with_one_live_tile();
        assert!(!ui.wall.on);

        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        assert!(ui.wall.on, "Ctrl+W did not turn the wall on");
        assert!(ui.notice.text().contains("video wall"));

        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        assert!(!ui.wall.on, "Ctrl+W did not turn the wall off");
        kill_attached(&mut ui);
    }

    /// Esc belongs to the child: it is how you interrupt Claude, and a wall you cannot
    /// interrupt from is worse than no wall. Ctrl+W and Ctrl+] are the exits.
    #[test]
    fn esc_reaches_the_tile_and_does_not_leave_the_wall() {
        let mut ui = wall_ui_with_one_live_tile();
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);

        press_normal_code(&mut ui, &[], KeyCode::Esc, KeyModifiers::NONE);
        assert!(ui.wall.on, "Esc must go to the tile, not close the wall");

        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        assert!(!ui.wall.on, "Ctrl+W is the exit");
        kill_attached(&mut ui);
    }

    /// The wall reserves what it needs and forwards the rest. A viewer chord that still acted
    /// here would be a chord the session could never receive — Ctrl+A is "start of line" while
    /// you are typing a reply, not "show all". Ctrl+X and Ctrl+K are the deliberate exceptions
    /// and have their own tests; everything below must still reach the child.
    #[test]
    fn viewer_chords_go_to_the_tile_instead_of_acting() {
        let mut ui = wall_ui_with_one_live_tile();
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        assert!(ui.wall.on);

        let group_mode = ui.app.group_mode();
        let show_all = ui.app.show_all();
        let sprite = ui.sprite;
        for chord in ['s', 'a', 'f', 'g', 'r', 'e'] {
            press_normal_key(&mut ui, &[], chord, KeyModifiers::CONTROL);
        }

        assert_eq!(ui.app.group_mode(), group_mode, "Ctrl+S must not regroup");
        assert_eq!(ui.app.show_all(), show_all, "Ctrl+A must not show all");
        assert_eq!(ui.sprite, sprite, "Ctrl+G must not cycle the sprite");
        assert!(
            matches!(ui.mode, Mode::Normal),
            "no chord may open a modal over the wall"
        );

        // Typing goes to the tile, not the composer — the composer is not even on screen.
        press_normal_key(&mut ui, &[], 'z', KeyModifiers::NONE);
        assert_eq!(ui.composer.text(), "");

        assert!(ui.wall.on, "none of those chords should have left the wall");
        kill_attached(&mut ui);
    }

    /// Drain one finished mutation, exactly as the run loop's per-tick poll does. The removal
    /// queued behind a stop is a dependent job: it does not start until the stop's success has
    /// been drained, so a test that skips this never sees the second stage run.
    fn drain_one_mutation(ui: &mut Ui) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while ui.mutations.poll().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the background mutation never finished"
            );
            std::thread::yield_now();
        }
    }

    /// Two live tiles with the focus moved onto the second one. The pair the wall's own
    /// session-scoped chords have to aim at.
    fn wall_ui_focused_on_the_second_tile() -> Ui {
        let mut first = sess("wall-keep", "/tmp/agentviewer-wall", 100);
        first.status = Status::Working;
        let mut second = sess("wall-retire", "/tmp/agentviewer-wall", 200);
        second.status = Status::Working;
        second.short_id = Some("wall-retire".to_string());
        let keys = [
            (first.backend, first.id.clone()),
            (second.backend, second.id.clone()),
        ];
        let mut ui = test_ui_with(vec![first, second]);
        for key in &keys {
            ui.attached.insert(key.clone(), wall_tile_pty());
        }
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        assert!(ui.wall.on);
        press_normal_code(&mut ui, &[], KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(
            wall_focus(&ui),
            1,
            "Shift+Right did not move the wall focus"
        );
        ui
    }

    /// The point of the chord: retire a panel you are finished with without walking back to
    /// the list. It has to hit the tile the grid is showing as focused — `kill_stage` reads
    /// `app.selected()`, which the wall tracks separately, so a missing selection pin would
    /// stop or remove a session the user is not even looking at.
    #[test]
    fn ctrl_x_on_the_wall_stops_then_removes_the_focused_tile() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        ui.mutation_executor = std::sync::Arc::new(move |mutation| {
            let seen = match &mutation {
                crate::ops::Mutation::Stop(request) => format!("stop:{}", request.id()),
                crate::ops::Mutation::Remove { request, .. } => format!("remove:{}", request.id()),
                other => panic!("Ctrl+X submitted {:?}", std::mem::discriminant(other)),
            };
            seen_tx.send(seen).expect("report the mutation");
            Ok(MutationOutcome {
                notice: String::new(),
                spawned: None,
            })
        });

        press_normal_key(&mut ui, &[], 'x', KeyModifiers::CONTROL);
        assert_eq!(
            seen_rx.recv_timeout(std::time::Duration::from_secs(1)),
            Ok("stop:wall-retire".to_string()),
            "the first Ctrl+X must stop the FOCUSED tile, not the list's parked row"
        );
        assert!(ui.wall.on, "Ctrl+X must not leave the wall");
        assert!(
            ui.app.is_armed(agent_viewer_core::spawn::now_ms()),
            "the first press must arm removal so the footer can show the countdown"
        );
        drain_one_mutation(&mut ui);

        press_normal_key(&mut ui, &[], 'x', KeyModifiers::CONTROL);
        assert_eq!(
            seen_rx.recv_timeout(std::time::Duration::from_secs(1)),
            Ok("remove:wall-retire".to_string()),
            "the second Ctrl+X must remove the same tile"
        );
        assert!(ui.wall.on, "removing a tile must not close the wall");
        kill_attached(&mut ui);
    }

    /// Ctrl+K is the wall's menu for everything the grid has no chord for. It must open over
    /// the wall (not close it) with the focused tile as the target its action items act on.
    #[test]
    fn ctrl_k_on_the_wall_opens_the_palette_for_the_focused_tile() {
        let mut ui = wall_ui_focused_on_the_second_tile();

        press_normal_key(&mut ui, &[], 'k', KeyModifiers::CONTROL);

        assert!(
            matches!(ui.mode, Mode::Palette(_)),
            "Ctrl+K must open the palette on the wall"
        );
        assert!(
            ui.wall.on,
            "the palette floats over the wall; it must not close it"
        );
        assert_eq!(
            ui.app.selected().map(|session| session.id.clone()),
            Some("wall-retire".to_string()),
            "the palette must be aimed at the focused tile"
        );
        kill_attached(&mut ui);
    }

    #[test]
    fn wall_quickswitcher_uses_the_ctrl_x_action_for_claude() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(agent_viewer_core::claude::ClaudeBackend::new())];

        press_normal_key(&mut ui, &backends, 'k', KeyModifiers::CONTROL);

        let Mode::Palette(palette) = &ui.mode else {
            panic!("Ctrl+K opens the wall quickswitcher");
        };
        assert!(!palette.results().any(|item| item.name == "Archive session"));
        assert!(palette.results().any(|item| {
            item.name == "Stop or remove session"
                && matches!(
                    item.target,
                    PaletteTarget::Action(PaletteAction::StopOrRemove)
                )
                && item.enabled
        }));

        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        ui.mutation_executor = std::sync::Arc::new(move |mutation| {
            let seen = match mutation {
                crate::ops::Mutation::Stop(request) => format!("stop:{}", request.id()),
                crate::ops::Mutation::Remove { request, .. } => {
                    format!("remove:{}", request.id())
                }
                _ => panic!("unexpected quickswitcher mutation"),
            };
            seen_tx.send(seen).expect("report quickswitcher mutation");
            Ok(MutationOutcome {
                notice: String::new(),
                spawned: None,
            })
        });

        highlight_palette_target(&mut ui, &PaletteTarget::Action(PaletteAction::StopOrRemove));
        press_palette_code(&mut ui, &backends, KeyCode::Enter);
        assert_eq!(
            seen_rx.recv_timeout(std::time::Duration::from_secs(1)),
            Ok("stop:wall-retire".to_string())
        );
        drain_one_mutation(&mut ui);

        press_normal_key(&mut ui, &backends, 'k', KeyModifiers::CONTROL);
        highlight_palette_target(&mut ui, &PaletteTarget::Action(PaletteAction::StopOrRemove));
        press_palette_code(&mut ui, &backends, KeyCode::Enter);
        assert_eq!(
            seen_rx.recv_timeout(std::time::Duration::from_secs(1)),
            Ok("remove:wall-retire".to_string())
        );
        drain_one_mutation(&mut ui);
        kill_attached(&mut ui);
    }

    /// A Claude backend whose only variable is whether it advertises spawn: capable puts Enter
    /// on the same spawn path the list's composer takes, refusing puts it on the refusal path.
    /// Everything else panics — nothing but spawn belongs on either.
    pub(crate) struct SpawnBackend {
        pub(crate) spawn: bool,
    }

    impl agent_viewer_core::Backend for SpawnBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }
        fn capabilities(&self) -> agent_viewer_core::Capabilities {
            agent_viewer_core::Capabilities {
                spawn: self.spawn,
                ..agent_viewer_core::Capabilities::none()
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
        ) -> std::result::Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attach is not exercised by composer submission")
        }
    }

    /// One live tile with an echoing child, the wall up, and the compose overlay open over it.
    /// The shape every "the box has the keyboard, the tile must not see it" test starts from.
    fn composing_over_an_echoing_tile(id: &str) -> (Ui, (BackendKind, String)) {
        let mut session = sess(id, "/tmp/agentviewer-wall", 100);
        session.status = Status::Working;
        let target = (session.backend, session.id.clone());
        let mut ui = test_ui_with(vec![session]);
        ui.attached.insert(target.clone(), echoing_wall_tile_pty());
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        pick_palette_target(
            &mut ui,
            "New session",
            &PaletteTarget::Action(PaletteAction::Spawn),
        );
        (ui, target)
    }

    /// Leave compose, then type a sentinel straight into the tile and wait for it to land.
    ///
    /// A pty is ordered, so once the sentinel is on the child's screen anything forwarded
    /// earlier would already be there too — which turns the absence of `needle` from a race
    /// into a fact.
    fn assert_tile_never_saw(ui: &mut Ui, target: &(BackendKind, String), needle: &str) {
        press_normal_code(ui, &[], KeyCode::Esc, KeyModifiers::NONE);
        for character in "SENTINEL".chars() {
            press_normal_key(ui, &[], character, KeyModifiers::NONE);
        }
        wait_for_pty_screen(ui, target, "SENTINEL");
        assert!(
            ui.attached[target].with_screen(|screen| !screen.contents().contains(needle)),
            "{needle:?} was typed into the live session"
        );
    }

    /// Starting a new task was the one thing the wall could not do: it hides the composer, so
    /// every route to a spawn ran through leaving the grid and coming back. The palette is
    /// where the wall keeps what it has no chord for, so that is where the entry point belongs.
    #[test]
    fn the_wall_palette_offers_a_new_session_that_opens_the_composer() {
        let mut ui = wall_ui_focused_on_the_second_tile();

        assert!(
            palette_items(&[], &ui).iter().any(|item| {
                item.name == "New session"
                    && matches!(&item.target, PaletteTarget::Action(PaletteAction::Spawn))
                    && item.enabled
            }),
            "the wall palette must offer a New session entry"
        );

        pick_palette_target(
            &mut ui,
            "New session",
            &PaletteTarget::Action(PaletteAction::Spawn),
        );

        assert!(
            matches!(ui.mode, Mode::Compose),
            "picking New session must hand the keyboard to the composer"
        );
        assert!(
            ui.wall.on,
            "the composer floats over the wall; it must not close it"
        );
        kill_attached(&mut ui);
    }

    /// Off the wall the composer is already on screen and already owns the keyboard, so an
    /// entry whose whole job is "give the composer the keyboard" would be a row that does
    /// nothing — noise in the one list the user scans when they do not know the chord.
    #[test]
    fn the_new_session_entry_is_absent_from_the_list_palette() {
        let ui = test_ui_with(vec![sess("only", "/tmp/agentviewer-compose", 100)]);
        assert!(!ui.wall.on);

        assert!(
            !palette_items(&[], &ui)
                .iter()
                .any(|item| matches!(&item.target, PaletteTarget::Action(PaletteAction::Spawn))),
            "the list palette must not offer the wall's compose entry"
        );
    }

    /// The overlay is only worth drawing if it actually catches the keys. The focused tile owns
    /// the keyboard the rest of the time, so the bytes have to stop reaching the child the
    /// moment compose opens — otherwise the draft is typed into a live agent mid-turn.
    #[test]
    fn typing_while_composing_fills_the_composer_and_never_reaches_the_tile() {
        let (mut ui, target) = composing_over_an_echoing_tile("compose-tile");

        for character in "draft".chars() {
            press_normal_key(&mut ui, &[], character, KeyModifiers::NONE);
        }
        assert_eq!(ui.composer.text(), "draft");

        assert_tile_never_saw(&mut ui, &target, "draft");
        kill_attached(&mut ui);
    }

    /// The whole point of the overlay: a task typed over the wall has to actually start one,
    /// through the list's own spawn path, and hand the keyboard straight back to the grid.
    #[test]
    fn enter_while_composing_spawns_the_typed_task_and_returns_to_the_wall() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        ui.mutation_executor = std::sync::Arc::new(move |mutation| {
            let seen = match &mutation {
                crate::ops::Mutation::Spawn { task, .. } => task.clone(),
                other => panic!(
                    "composing on the wall submitted {:?}",
                    std::mem::discriminant(other)
                ),
            };
            seen_tx.send(seen).expect("report the mutation");
            Ok(MutationOutcome {
                notice: String::new(),
                spawned: None,
            })
        });
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(SpawnBackend { spawn: true })];

        pick_palette_target(
            &mut ui,
            "New session",
            &PaletteTarget::Action(PaletteAction::Spawn),
        );
        for character in "ship the wall composer".chars() {
            press_normal_key(&mut ui, &backends, character, KeyModifiers::NONE);
        }
        press_normal_code(&mut ui, &backends, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            seen_rx.recv_timeout(std::time::Duration::from_secs(1)),
            Ok("ship the wall composer".to_string()),
            "Enter must run the list's spawn path with the typed task"
        );
        assert!(
            matches!(ui.mode, Mode::Normal),
            "the keyboard must go back to the grid once the task is away"
        );
        assert!(ui.wall.on, "spawning must not close the wall");
        kill_attached(&mut ui);
    }

    /// Esc here means "never mind, back to the grid", not "throw the draft away". Losing a
    /// half-typed task because you glanced at a tile is the failure this prevents.
    #[test]
    fn esc_while_composing_returns_to_the_wall_with_the_draft_intact() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        pick_palette_target(
            &mut ui,
            "New session",
            &PaletteTarget::Action(PaletteAction::Spawn),
        );
        for character in "half a thought".chars() {
            press_normal_key(&mut ui, &[], character, KeyModifiers::NONE);
        }

        press_normal_code(&mut ui, &[], KeyCode::Esc, KeyModifiers::NONE);

        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.wall.on, "Esc must return to the wall, not close it");
        assert_eq!(
            ui.composer.text(),
            "half a thought",
            "Esc must not discard the draft"
        );
        assert_eq!(
            wall_focus(&ui),
            1,
            "backing out of compose must not move the grid's focus"
        );
        kill_attached(&mut ui);
    }

    /// A pasted task description is how a long prompt actually gets into that box, and the
    /// overlay owns the keyboard while it is up. Dropping the paste is the same "the text
    /// vanished" symptom the wall's own paste guard exists to prevent, one mode over.
    #[test]
    fn pasting_while_composing_fills_the_composer_and_never_reaches_the_tile() {
        let (mut ui, target) = composing_over_an_echoing_tile("paste-tile");

        handle_paste("pasted-draft", &mut ui);
        assert_eq!(ui.composer.text(), "pasted-draft");

        assert_tile_never_saw(&mut ui, &target, "pasted-draft");
        kill_attached(&mut ui);
    }

    /// Ctrl+W is documented as an unconditional way back to the list, so an overlay that
    /// swallowed it would turn the grid into the trap that guarantee exists to prevent.
    #[test]
    fn ctrl_w_while_composing_returns_to_the_list_with_the_draft_intact() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        pick_palette_target(
            &mut ui,
            "New session",
            &PaletteTarget::Action(PaletteAction::Spawn),
        );
        for character in "half a thought".chars() {
            press_normal_key(&mut ui, &[], character, KeyModifiers::NONE);
        }

        let quit = press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);

        assert!(
            !quit,
            "Ctrl+W leaves the wall, it does not leave the viewer"
        );
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(
            !ui.wall.on,
            "Ctrl+W has to reach the list, not stop at the overlay"
        );
        assert_eq!(
            ui.composer.text(),
            "half a thought",
            "every way out of compose keeps the draft"
        );
        kill_attached(&mut ui);
    }

    /// On the wall Ctrl+C belongs to the focused tile, never to the viewer. Opening this box
    /// must not re-arm it as a teardown: quitting from here would kill every tile's child, and
    /// the draft with them.
    #[test]
    fn ctrl_c_while_composing_returns_to_the_wall_instead_of_quitting() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        pick_palette_target(
            &mut ui,
            "New session",
            &PaletteTarget::Action(PaletteAction::Spawn),
        );
        for character in "half a thought".chars() {
            press_normal_key(&mut ui, &[], character, KeyModifiers::NONE);
        }

        let quit = press_normal_key(&mut ui, &[], 'c', KeyModifiers::CONTROL);

        assert!(!quit, "Ctrl+C in compose must not tear the viewer down");
        assert!(matches!(ui.mode, Mode::Normal));
        assert!(ui.wall.on, "Ctrl+C backs out to the grid, like Esc");
        assert_eq!(
            ui.composer.text(),
            "half a thought",
            "every way out of compose keeps the draft"
        );
        assert_eq!(
            ui.attached.len(),
            2,
            "the tiles' children must survive backing out of compose"
        );
        kill_attached(&mut ui);
    }

    /// The silent one: `spawn_target()` reads the list selection, which the wall pins to the
    /// focused tile, so an arrow that moved it here would start the new session in a different
    /// directory than the one on screen — with nothing on screen to say so.
    #[test]
    fn arrows_while_composing_leave_the_selection_and_the_wall_focus_alone() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        pick_palette_target(
            &mut ui,
            "New session",
            &PaletteTarget::Action(PaletteAction::Spawn),
        );
        let selected = ui.app.selected().map(|session| session.id.clone());
        assert_eq!(selected, Some("wall-retire".to_string()));

        press_normal_code(&mut ui, &[], KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            ui.app.selected().map(|session| session.id.clone()),
            selected,
            "Down repointed the directory the new session would land in"
        );
        press_normal_code(&mut ui, &[], KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            ui.app.selected().map(|session| session.id.clone()),
            selected,
            "Up repointed the directory the new session would land in"
        );
        assert_eq!(
            wall_focus(&ui),
            1,
            "the arrows must not move the grid's focus either"
        );
        kill_attached(&mut ui);
    }

    /// The editing chords have to belong to the box, not to the child underneath it: a Tab
    /// forwarded to a live agent mid-turn is an answer the user never typed.
    #[test]
    fn tab_and_backspace_while_composing_edit_the_composer_not_the_tile() {
        let (mut ui, target) = composing_over_an_echoing_tile("edit-tile");
        for character in "abc".chars() {
            press_normal_key(&mut ui, &[], character, KeyModifiers::NONE);
        }
        let backend = ui.composer.backend();

        press_normal_code(&mut ui, &[], KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(ui.composer.text(), "ab", "Backspace must edit the draft");
        press_normal_code(&mut ui, &[], KeyCode::Tab, KeyModifiers::NONE);
        assert_ne!(
            ui.composer.backend(),
            backend,
            "Tab must cycle the composer's agent"
        );
        press_normal_code(&mut ui, &[], KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(
            ui.composer.text(),
            "ab",
            "Shift+Tab is the model cycle, not text"
        );

        // Nothing typed OR edited in the box may have reached the child, Tab included.
        assert_tile_never_saw(&mut ui, &target, "ab");
        kill_attached(&mut ui);
    }

    /// An empty composer has nothing to send and no list row underneath to activate, so Enter
    /// has to stay put rather than start a session with no task in it.
    #[test]
    fn enter_on_an_empty_composer_submits_nothing_and_stays_in_compose() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        ui.mutation_executor = std::sync::Arc::new(move |mutation| {
            seen_tx
                .send(format!("{:?}", std::mem::discriminant(&mutation)))
                .expect("report the mutation");
            Ok(MutationOutcome {
                notice: String::new(),
                spawned: None,
            })
        });
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(SpawnBackend { spawn: true })];

        pick_palette_target(
            &mut ui,
            "New session",
            &PaletteTarget::Action(PaletteAction::Spawn),
        );
        press_normal_code(&mut ui, &backends, KeyCode::Enter, KeyModifiers::NONE);

        assert!(
            seen_rx
                .recv_timeout(std::time::Duration::from_millis(250))
                .is_err(),
            "Enter on an empty composer submitted a mutation"
        );
        assert!(
            matches!(ui.mode, Mode::Compose),
            "Enter with nothing to send must leave the box open"
        );
        assert!(ui.wall.on);
        kill_attached(&mut ui);
    }

    /// The draft is only drawn while composing, so a refusal that closed the box would strand
    /// text the user can neither see nor recover — with the notice explaining it flashing on a
    /// grid they were just sent back to.
    #[test]
    fn a_refused_spawn_keeps_the_composer_open_with_the_draft() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(SpawnBackend { spawn: false })];

        pick_palette_target(
            &mut ui,
            "New session",
            &PaletteTarget::Action(PaletteAction::Spawn),
        );
        for character in "a task that goes nowhere".chars() {
            press_normal_key(&mut ui, &backends, character, KeyModifiers::NONE);
        }
        press_normal_code(&mut ui, &backends, KeyCode::Enter, KeyModifiers::NONE);

        assert!(
            matches!(ui.mode, Mode::Compose),
            "a refused spawn must not drop the user back onto the grid"
        );
        assert_eq!(
            ui.composer.text(),
            "a task that goes nowhere",
            "a refused spawn must keep the draft"
        );
        assert!(
            ui.notice.text().contains("does not support spawn"),
            "the refusal has to say why: {}",
            ui.notice.text()
        );
        kill_attached(&mut ui);
    }

    /// The trap this closes: the Commands group pushes `/<command> ` into a composer that is
    /// not on screen while the wall is on, so the palette used to stuff state the user could
    /// neither see nor finish typing.
    #[test]
    fn a_palette_command_on_the_wall_opens_the_composer_holding_it() {
        let mut ui = wall_ui_focused_on_the_second_tile();

        pick_palette_target(
            &mut ui,
            "/theme",
            &PaletteTarget::Command("theme".to_string()),
        );

        assert!(
            matches!(ui.mode, Mode::Compose),
            "the pushed command has to end up somewhere the user can see it"
        );
        assert!(ui.wall.on);
        assert_eq!(ui.composer.text(), "/theme ");
        kill_attached(&mut ui);
    }

    /// Same trap as the Commands group: picking a model only sets composer state, and on the
    /// wall that composer is not on screen, so the pick landed somewhere invisible.
    #[test]
    fn a_palette_model_on_the_wall_opens_the_composer_holding_it() {
        let mut ui = wall_ui_focused_on_the_second_tile();
        let model = ui.composer.model().to_string();

        pick_palette_target(
            &mut ui,
            &model,
            &PaletteTarget::Model {
                backend: BackendKind::Claude,
                name: model.clone(),
            },
        );

        assert!(
            matches!(ui.mode, Mode::Compose),
            "the picked model has to end up somewhere the user can see it"
        );
        assert!(ui.wall.on);
        assert_eq!(ui.composer.model(), model);
        kill_attached(&mut ui);
    }

    #[test]
    fn shift_arrows_walk_the_grid_and_pin_the_list_selection() {
        let mut first = sess("tile-a", "/tmp/agentviewer-wall", 100);
        first.status = Status::Working;
        let mut second = sess("tile-b", "/tmp/agentviewer-wall", 200);
        second.status = Status::Working;
        let keys = [
            (first.backend, first.id.clone()),
            (second.backend, second.id.clone()),
        ];
        let mut ui = test_ui_with(vec![first, second]);
        for key in &keys {
            ui.attached.insert(key.clone(), wall_tile_pty());
        }
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        assert_eq!(wall_focus(&ui), 0);
        let ordered =
            agent_viewer_tui::ui::wall::tile_keys(&ui.app, agent_viewer_core::spawn::now_ms());
        assert_eq!(ordered.len(), 2);

        // A plain arrow is the child's, not the wall's — that is what makes the tile a real
        // input surface.
        press_normal_code(&mut ui, &[], KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            wall_focus(&ui),
            0,
            "a bare arrow must go to the tile, not move the focus"
        );

        // Nor is Ctrl+arrow the wall's: the host terminal keeps it and it never reaches the
        // viewer, so binding it would be a key that silently does nothing.
        press_normal_code(&mut ui, &[], KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(wall_focus(&ui), 0);

        // Two tiles are a 2x1 row, so Shift+Right moves and Shift+Down does not.
        press_normal_code(&mut ui, &[], KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(wall_focus(&ui), 1);
        assert_eq!(
            ui.app.selected().map(|s| s.id.clone()),
            Some(ordered[1].1.clone()),
            "the tile selection must pin the list selection"
        );

        press_normal_code(&mut ui, &[], KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(wall_focus(&ui), 1, "a 2x1 grid has no second row");

        press_normal_code(&mut ui, &[], KeyCode::Left, KeyModifiers::SHIFT);
        assert_eq!(wall_focus(&ui), 0);
        assert_eq!(
            ui.app.selected().map(|s| s.id.clone()),
            Some(ordered[0].1.clone())
        );
        kill_attached(&mut ui);
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
            _effort: Option<&str>,
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
            _effort: Option<&str>,
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
            BackendKind::Codex
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
            _effort: Option<&str>,
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
    fn ctrl_b_toggles_the_tail_pane_and_disturbs_nothing_else() {
        let mut ui = test_ui_with(vec![
            sess("first", "/tmp/agentviewer-tail-first", 100),
            sess("second", "/tmp/agentviewer-tail-second", 200),
        ]);
        select_session_row(&mut ui, "first");
        ui.composer.push_str("draft stays");
        let selected = ui.app.selected_index();

        assert!(!ui.tail_open);
        assert!(!press_normal_key(&mut ui, &[], 'b', KeyModifiers::CONTROL));
        assert!(ui.tail_open, "ctrl+b opens the pane");
        assert!(!press_normal_key(&mut ui, &[], 'b', KeyModifiers::CONTROL));
        assert!(!ui.tail_open, "ctrl+b closes it again");

        // The chord belongs to the pane alone: it never reaches the composer (where every
        // bare letter starts a task), never changes mode, and never moves the selection.
        assert_eq!(ui.composer.text(), "draft stays");
        assert!(matches!(ui.mode, Mode::Normal));
        assert_eq!(ui.app.selected_index(), selected);
    }

    /// Draw one list frame at `width`, which is what populates `list_hit` with the real
    /// measured list geometry the tail pane's width gate reads.
    fn render_list_frame(ui: &Ui, width: u16) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
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
                        attach: None,
                        pr_status: &ui.pr_status,
                        logos: None,
                        list_hit: &ui.list_hit,
                        themes: &ui.themes,
                        sprite: ui.sprite,
                        age_ramp: ui.age_ramp,
                        tail: None,
                        wall: None,
                        wall_rects: &ui.wall_rects,
                    },
                );
            })
            .expect("draw list frame");
    }

    /// The tail-pane entry in an open palette, as the user would find it.
    fn tail_palette_item(ui: &Ui) -> agent_viewer_tui::ui::PaletteItem {
        let Mode::Palette(palette) = &ui.mode else {
            panic!("expected the palette to be open");
        };
        palette
            .results()
            .find(|item| matches!(&item.target, PaletteTarget::Action(PaletteAction::TailPane)))
            .cloned()
            .expect("the palette offers the tail pane")
    }

    /// Open the palette and type `query`, leaving it open on the ranked results. An empty
    /// query lists only enabled actions, so a disabled one is found by typing for it, which
    /// is how the user would find it too.
    fn open_palette_with_query(ui: &mut Ui, query: &str) {
        let mut terminal = test_terminal();
        assert!(!press_normal_key(ui, &[], 'k', KeyModifiers::CONTROL));
        for character in query.chars() {
            handle_palette_key(
                key(KeyCode::Char(character), KeyModifiers::NONE),
                &[],
                ui,
                &mut terminal,
            )
            .expect("type palette query");
        }
    }

    /// Open the palette, type `query`, and press Enter on the top hit.
    fn pick_from_palette(ui: &mut Ui, query: &str) {
        pick_palette(ui, query, None);
    }

    /// Open the palette, type `query`, walk to the first result whose target is `wanted`, and
    /// press Enter on it. Walking keeps a test about what an entry DOES independent of how the
    /// ranking happens to order that query's other hits.
    fn pick_palette_target(ui: &mut Ui, query: &str, wanted: &PaletteTarget) {
        pick_palette(ui, query, Some(wanted));
    }

    /// Open the palette on `query` and Enter on `wanted`, or on the top hit when it is `None`.
    fn pick_palette(ui: &mut Ui, query: &str, wanted: Option<&PaletteTarget>) {
        fn highlighted_is(ui: &Ui, wanted: &PaletteTarget) -> bool {
            matches!(&ui.mode, Mode::Palette(palette)
                if palette.highlighted().map(|item| &item.target) == Some(wanted))
        }

        open_palette_with_query(ui, query);
        let mut terminal = test_terminal();
        if let Some(wanted) = wanted {
            let count = match &ui.mode {
                Mode::Palette(palette) => palette.result_count(),
                _ => panic!("the palette did not open"),
            };
            for _ in 0..count {
                if highlighted_is(ui, wanted) {
                    break;
                }
                handle_palette_key(
                    key(KeyCode::Down, KeyModifiers::NONE),
                    &[],
                    ui,
                    &mut terminal,
                )
                .expect("walk the palette results");
            }
            assert!(
                highlighted_is(ui, wanted),
                "the palette had no {wanted:?} result for {query:?}"
            );
        }
        handle_palette_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &[],
            ui,
            &mut terminal,
        )
        .expect("accept palette selection");
    }

    #[test]
    fn the_quick_switcher_toggles_the_tail_pane_both_ways() {
        let mut ui = test_ui_with(vec![sess("only", "/tmp/agentviewer-tail-palette", 100)]);
        render_list_frame(&ui, 170);

        // Closed: typing "tail pane" finds an entry offering to show it.
        open_palette_with_query(&mut ui, "tail pane");
        let item = tail_palette_item(&ui);
        assert_eq!(item.name, "Show tail pane");
        assert!(item.enabled);
        ui.mode = Mode::Normal;

        pick_from_palette(&mut ui, "tail pane");
        assert!(ui.tail_open, "the palette entry opened the pane");
        assert!(matches!(ui.mode, Mode::Normal));

        // Open: the same entry now reads as hide, and picking it closes the pane again.
        render_list_frame(&ui, 170);
        open_palette_with_query(&mut ui, "tail pane");
        let item = tail_palette_item(&ui);
        assert_eq!(item.name, "Hide tail pane");
        assert!(item.detail.contains("showing now"));
        ui.mode = Mode::Normal;

        pick_from_palette(&mut ui, "tail pane");
        assert!(!ui.tail_open, "the palette entry closed the pane");
    }

    #[test]
    fn the_quick_switcher_disables_the_tail_pane_on_a_narrow_terminal() {
        let mut ui = test_ui_with(vec![sess("only", "/tmp/agentviewer-tail-narrow-p", 100)]);
        render_list_frame(&ui, 80);

        open_palette_with_query(&mut ui, "tail pane");
        let item = tail_palette_item(&ui);
        assert!(!item.enabled, "80 columns cannot show the pane");
        assert!(
            item.disabled_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("80")),
            "the reason names the measured width: {:?}",
            item.disabled_reason
        );
        ui.mode = Mode::Normal;

        // Picking it anyway leaves the pane closed and says why.
        pick_from_palette(&mut ui, "tail pane");
        assert!(!ui.tail_open);
        assert!(ui.notice.text().contains("80"), "{:?}", ui.notice.text());
    }

    #[test]
    fn ctrl_b_refuses_to_open_a_pane_a_narrow_terminal_cannot_render() {
        let mut ui = test_ui_with(vec![sess("only", "/tmp/agentviewer-tail-narrow", 100)]);

        // At 80 columns the pane's 46 would leave the list unreadable, so the chord refuses
        // and says why instead of turning a flag on that renders nothing.
        render_list_frame(&ui, 80);
        assert!(!press_normal_key(&mut ui, &[], 'b', KeyModifiers::CONTROL));
        assert!(!ui.tail_open, "the pane must not open at 80 columns");
        assert!(
            ui.notice.text().contains("80"),
            "the refusal names the measured width: {:?}",
            ui.notice.text()
        );

        // Wide enough, and the same chord opens it.
        render_list_frame(&ui, agent_viewer_tui::ui::TAIL_MIN_TOTAL_WIDTH + 20);
        assert!(!press_normal_key(&mut ui, &[], 'b', KeyModifiers::CONTROL));
        assert!(ui.tail_open);
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
    fn quickswitcher_omits_hold_session_and_keeps_visible_session_and_action() {
        let mut hold = sess("hold-id", "/tmp/agentviewer-palette-hold", 200);
        hold.title = "Hold".to_string();
        let mut ordinary = sess("ordinary-id", "/tmp/agentviewer-palette-ordinary", 100);
        ordinary.title = "Ordinary session".to_string();
        let mut ui = test_ui_with(vec![hold, ordinary]);

        assert!(!press_normal_key(&mut ui, &[], 'k', KeyModifiers::CONTROL));
        let Mode::Palette(palette) = &ui.mode else {
            panic!("expected quickswitcher palette");
        };

        assert!(!palette.results().any(|item| {
            matches!(
                &item.target,
                PaletteTarget::Session { id, .. } if id == "hold-id"
            )
        }));
        assert!(palette.results().any(|item| {
            item.name == "Ordinary session"
                && matches!(
                    &item.target,
                    PaletteTarget::Session { id, .. } if id == "ordinary-id"
                )
        }));
        assert!(palette.results().any(|item| {
            item.name == "Show all sessions"
                && matches!(&item.target, PaletteTarget::Action(PaletteAction::ShowAll))
        }));
    }

    #[test]
    fn age_ramp_is_off_by_default_and_toggles_from_the_palette() {
        let mut ui = test_ui_with(Vec::new());
        assert!(!ui.age_ramp, "the age ramp must start off");

        assert!(
            palette_items(&[], &ui).iter().any(|item| {
                item.name == "Age ramp"
                    && matches!(&item.target, PaletteTarget::Action(PaletteAction::AgeRamp))
                    && item.enabled
            }),
            "the palette is the only entry point, so the item has to be there and enabled"
        );

        super::toggle_age_ramp(&mut ui);
        assert!(ui.age_ramp);
        assert_eq!(ui.notice.text(), "age ramp: on");

        super::toggle_age_ramp(&mut ui);
        assert!(!ui.age_ramp);
        assert_eq!(ui.notice.text(), "age ramp: off");
    }

    #[test]
    fn switching_the_age_ramp_on_under_a_non_truecolor_theme_says_it_will_do_nothing() {
        let mut ui = test_ui_with(Vec::new());
        // Walk the picker to `terminal`, the builtin with no truecolor endpoint to fade toward.
        while ui.themes.active().id != "terminal" {
            ui.themes.move_preview(1);
        }

        super::toggle_age_ramp(&mut ui);
        assert!(
            ui.age_ramp,
            "the flag still flips, so a theme change picks it up"
        );
        assert_eq!(
            ui.notice.text(),
            "age ramp: on · no effect under the terminal match theme"
        );
    }

    #[test]
    fn palette_models_only_include_available_backends() {
        let mut ui = test_ui_with(Vec::new());
        ui.composer.set_available_backends(vec![BackendKind::Codex]);

        let items = palette_items(&[], &ui);
        let model_backends = items
            .iter()
            .filter_map(|item| match item.target {
                PaletteTarget::Model { backend, .. } => Some(backend),
                _ => None,
            })
            .collect::<HashSet<_>>();

        assert_eq!(model_backends, HashSet::from([BackendKind::Codex]));
    }

    #[test]
    fn palette_auto_models_are_gated_on_router_availability() {
        let mut ui = test_ui_with(Vec::new());
        ui.models
            .seed(BackendKind::Claude, vec!["opus[1m]".to_string()], true);
        ui.models
            .seed(BackendKind::Codex, vec!["default".to_string()], true);

        let automatic_targets = palette_items(&[], &ui)
            .into_iter()
            .filter_map(|item| match item.target {
                PaletteTarget::Model { backend, name } if name == AUTO_MODEL => Some(backend),
                _ => None,
            })
            .collect::<HashSet<_>>();

        assert!(automatic_targets.is_empty());
    }

    #[test]
    fn routed_codex_palette_omits_redundant_default() {
        let mut ui = test_ui_with(Vec::new());
        ui.composer.set_available_backends(vec![BackendKind::Codex]);
        ui.composer.set_auto_available(true);
        ui.models.seed(
            BackendKind::Codex,
            vec!["default".to_string(), "gpt".to_string()],
            true,
        );

        let model_names = palette_items(&[], &ui)
            .into_iter()
            .filter_map(|item| match item.target {
                PaletteTarget::Model {
                    backend: BackendKind::Codex,
                    name,
                } => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(model_names, vec![AUTO_MODEL.to_string(), "gpt".to_string()]);
    }

    #[test]
    fn palette_offers_and_accepts_auto_for_each_concrete_provider() {
        let mut catalog_ui = test_ui_with(Vec::new());
        catalog_ui.composer.set_auto_available(true);
        catalog_ui
            .models
            .seed(BackendKind::Claude, vec!["opus[1m]".to_string()], true);
        catalog_ui
            .models
            .seed(BackendKind::Codex, vec!["default".to_string()], true);

        let automatic_rows = palette_items(&[], &catalog_ui)
            .into_iter()
            .filter(|item| {
                matches!(
                    &item.target,
                    PaletteTarget::Model { name, .. } if name == AUTO_MODEL
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(automatic_rows.len(), 2);
        for backend in [BackendKind::Claude, BackendKind::Codex] {
            let row = automatic_rows
                .iter()
                .find(|item| {
                    item.target
                        == (PaletteTarget::Model {
                            backend,
                            name: AUTO_MODEL.to_string(),
                        })
                })
                .expect("each concrete provider has an automatic model row");
            assert!(row.detail.contains(backend.name()));
            assert!(row.detail.contains("keep model and effort automatic"));
            assert!(!row.detail.contains("router choose"));
        }

        for backend in [BackendKind::Claude, BackendKind::Codex] {
            let mut ui = test_ui_with(Vec::new());
            ui.composer.set_auto_available(true);
            ui.composer.default_to_auto();
            ui.models
                .seed(BackendKind::Claude, vec!["opus[1m]".to_string()], true);
            ui.models
                .seed(BackendKind::Codex, vec!["default".to_string()], true);
            ui.composer.push_str("keep this draft");
            open_palette(&[], &mut ui);
            let mut terminal = test_terminal();
            for character in AUTO_MODEL.chars() {
                handle_palette_key(
                    key(KeyCode::Char(character), KeyModifiers::NONE),
                    &[],
                    &mut ui,
                    &mut terminal,
                )
                .expect("type automatic model query");
            }
            let target = PaletteTarget::Model {
                backend,
                name: AUTO_MODEL.to_string(),
            };
            highlight_palette_target(&mut ui, &target);
            handle_palette_key(
                key(KeyCode::Enter, KeyModifiers::NONE),
                &[],
                &mut ui,
                &mut terminal,
            )
            .expect("accept automatic model");

            assert!(!ui.composer.is_auto());
            assert_eq!(ui.composer.backend(), backend);
            assert_eq!(ui.composer.model(), AUTO_MODEL);
            assert_eq!(ui.composer.text(), "keep this draft");
        }
    }

    #[test]
    fn a_palette_model_target_survives_an_async_catalog_refresh() {
        use crate::actions::install_models;
        use std::time::{Duration, Instant};

        let mut ui = test_ui_with(Vec::new());
        ui.models.seed(
            BackendKind::Claude,
            vec!["opus[1m]".to_string(), "retired".to_string()],
            false,
        );
        open_palette(&[], &mut ui);
        let target = PaletteTarget::Model {
            backend: BackendKind::Claude,
            name: "retired".to_string(),
        };
        let Mode::Palette(palette) = &mut ui.mode else {
            panic!("expected the palette to be open");
        };
        palette.push_str("retired");
        highlight_palette_target(&mut ui, &target);

        ui.models.request_with(BackendKind::Claude, || {
            vec!["opus[1m]".to_string(), "replacement".to_string()]
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            install_models(&mut ui);
            if ui
                .models
                .models(BackendKind::Claude)
                .is_some_and(|models| models.iter().any(|model| model == "replacement"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "replacement catalog did not arrive"
            );
            std::thread::yield_now();
        }
        assert!(
            !ui.models
                .models(BackendKind::Claude)
                .expect("refreshed catalog")
                .iter()
                .any(|model| model == "retired")
        );

        let mut terminal = test_terminal();
        handle_palette_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &[],
            &mut ui,
            &mut terminal,
        )
        .expect("accept stale palette model target");

        assert_eq!(ui.composer.model(), "retired");
    }

    #[test]
    fn cached_palette_action_dispatches_before_fresh_authorization() {
        let mut external = sess("external", "/tmp/agentviewer-palette-disabled", 100);
        external.backend = BackendKind::Codex;
        let mut ui = test_ui_with(vec![external]);
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
        assert!(ui.mutations.in_flight("codex:external:hide"));
    }

    /// Picking a model explicitly is a decision to use that provider, so it must leave Auto even
    /// when the model belongs to the backend already sitting underneath Auto. Otherwise
    /// `ensure_models` restores the single "auto" entry and Enter routes instead of spawning the
    /// model the user just chose.
    #[test]
    fn palette_model_pick_on_the_backend_under_auto_leaves_auto() {
        use super::{ensure_models, select_palette_model};
        let mut ui = test_ui_with(Vec::new());
        ui.models
            .seed(BackendKind::Codex, vec!["grok-4".to_string()], true);
        ui.composer.set_auto_available(true);
        while !ui.composer.is_auto() {
            ui.composer.cycle_backend();
        }
        ensure_models(&mut ui);
        assert_eq!(ui.composer.backend(), BackendKind::Codex);
        assert_eq!(ui.composer.model(), "auto");

        select_palette_model(&mut ui, BackendKind::Codex, "grok-4".to_string());
        ensure_models(&mut ui);

        assert!(
            !ui.composer.is_auto(),
            "an explicit model pick must leave Auto"
        );
        assert_eq!(ui.composer.model(), "grok-4");
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
    fn attach_capture_defaults_enable_capture_for_every_backend() {
        let mut capture_states = Vec::new();
        for backend in [BackendKind::Codex, BackendKind::Claude] {
            let mut session = sess("shared-attach", "/tmp/agentviewer-shared-attach", 100);
            session.backend = backend;
            // Avoid Claude's real trust preflight; this fake backend only exercises the shared
            // attach success transition after its command is accepted.
            session.short_id = Some("short".to_string());
            let mut ui = test_ui_with(vec![session]);
            ui.mouse_capture = false;
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
                crate::actions::install_attach_plan(&mut ui, &mut terminal, focus_plan(plan))
                    .expect("install selected session")
            );

            assert!(matches!(ui.mode, Mode::Attached), "{backend:?} must attach");
            capture_states.push((backend, ui.mouse_capture));
        }

        assert_eq!(
            capture_states,
            vec![(BackendKind::Codex, true), (BackendKind::Claude, true)],
            "attach capture defaults must match each backend"
        );
    }

    /// True until the pid is gone from the process table, polled briefly because the reader
    /// thread teardown is not instantaneous.
    fn pid_alive(pid: u32) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            if std::time::Instant::now() > deadline {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    /// The core of the no-persistence rule: a session is connected exactly while it is on
    /// screen. Leaving it must kill the child, not park it for a later re-attach.
    #[test]
    fn leaving_an_attached_session_closes_its_connection() {
        let mut ui = test_ui_with(Vec::new());
        let target = (BackendKind::Codex, "closes-on-exit".to_string());
        let pty = wall_tile_pty();
        let pid = pty.pid().expect("child pid");
        ui.attached.insert(target.clone(), pty);
        ui.detach_trackers
            .insert(target.clone(), DetachTracker::new());
        ui.mode = Mode::Attached;
        ui.focused = Some(target.clone());

        handle_attached_key(key(KeyCode::Char(']'), KeyModifiers::CONTROL), &mut ui);

        assert!(matches!(ui.mode, Mode::Normal));
        assert!(
            !ui.attached.contains_key(&target),
            "the connection must not survive leaving the session"
        );
        assert!(
            !ui.detach_trackers.contains_key(&target),
            "the per-PTY input gate must die with its PTY"
        );
        assert!(!pid_alive(pid), "child {pid} outlived the session view");
    }

    /// The one exception: the wall owns its tiles, and zooming into one then backing out
    /// returns you to the wall, which is about to draw that same connection again.
    #[test]
    fn zooming_out_of_a_wall_tile_keeps_the_wall_connection() {
        let mut ui = test_ui_with(Vec::new());
        let target = (BackendKind::Codex, "wall-tile".to_string());
        let pty = wall_tile_pty();
        let pid = pty.pid().expect("child pid");
        ui.attached.insert(target.clone(), pty);
        ui.wall.on = true;
        ui.wall.requested.insert(target.clone());
        ui.wall.sized.insert(target.clone(), (10, 40));
        ui.mode = Mode::Attached;
        ui.focused = Some(target.clone());

        handle_attached_key(key(KeyCode::Char(']'), KeyModifiers::CONTROL), &mut ui);

        assert!(matches!(ui.mode, Mode::Normal));
        assert!(
            ui.attached.contains_key(&target),
            "the wall still owns this connection"
        );
        assert!(
            !ui.wall.sized.contains_key(&target),
            "the recorded tile size must be dropped so the wall resizes the zoomed child back down"
        );
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
        kill_attached(&mut ui);
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
            session.backend = BackendKind::Codex;
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
            session.backend = BackendKind::Codex;
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
            crate::actions::install_attach_plan(&mut ui, &mut terminal, focus_plan(plan))
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
                .contains_key(&(BackendKind::Codex, "b".to_string())),
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
            session.backend = BackendKind::Codex;
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
            Some(&MouseTarget::Session(BackendKind::Codex, "b".to_string()))
        );

        // A refresh lands a newer session between "a" and "b", pushing "b" one row down.
        let mut refreshed = vec![
            sess("a", "/tmp/agentviewer-mouse-reflow", 100),
            sess("c", "/tmp/agentviewer-mouse-reflow", 200),
            sess("b", "/tmp/agentviewer-mouse-reflow", 300),
        ];
        for session in &mut refreshed {
            session.backend = BackendKind::Codex;
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
            crate::actions::install_attach_plan(&mut ui, &mut terminal, focus_plan(plan))
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
                .contains_key(&(BackendKind::Codex, "b".to_string()))
        );
        assert!(
            !ui.attached
                .contains_key(&(BackendKind::Codex, "c".to_string())),
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
            session.backend = BackendKind::Codex;
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
            session.backend = BackendKind::Codex;
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
            session.backend = BackendKind::Codex;
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
            session.backend = BackendKind::Codex;
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
            session.backend = BackendKind::Codex;
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
            session.backend = BackendKind::Codex;
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

        let key = (BackendKind::Codex, "b".to_string());
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
            crate::actions::install_attach_plan(&mut ui, &mut terminal, focus_plan(plan))
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
            session.backend = BackendKind::Codex;
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
                backend: BackendKind::Codex,
                id: "b".to_string(),
                buffer: String::new(),
            }),
            Mode::Reply(ReplyModal {
                backend: BackendKind::Codex,
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
                session.backend = BackendKind::Codex;
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
                crate::actions::install_attach_plan(&mut ui, &mut terminal, focus_plan(plan))
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
                crate::actions::install_attach_plan(&mut ui, &mut terminal, focus_plan(plan))
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
            }
        }
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
    fn ctrl_c_quits_when_no_child_is_taking_our_keys() {
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_quit_chord(ctrl_c, true, false));
    }

    #[test]
    fn ctrl_c_does_not_quit_while_a_child_is_taking_our_keys() {
        // Attached, or focused on a live wall tile, Ctrl+C must reach the child as an
        // interrupt rather than tearing down the viewer.
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!is_quit_chord(ctrl_c, true, true));
    }

    /// The wall must not swallow the only quit chord. With the wall open but no live child
    /// focused (nothing running, or a tile still connecting) Ctrl+C still tears the viewer
    /// down, because there is nothing there to send an interrupt to.
    #[test]
    fn ctrl_c_still_quits_on_a_wall_with_no_live_tile() {
        let mut ui = test_ui_with(vec![sess("idle-only", "/tmp/agentviewer-wall", 100)]);
        press_normal_key(&mut ui, &[], 'w', KeyModifiers::CONTROL);
        assert!(ui.wall.on);

        assert!(
            wall_input_target(&mut ui).is_none(),
            "no tile has a live child, so nothing should be taking keys"
        );
        assert!(press_normal_key(&mut ui, &[], 'c', KeyModifiers::CONTROL));
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
        external.backend = BackendKind::Codex;
        let mut managed = sess("managed", "/tmp/managed", 100);
        managed.backend = BackendKind::Codex;
        managed.daemon_hosted = true;
        let mut ui = test_ui_with(vec![external, managed]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> =
            vec![Box::new(RowScopedArchivingBackend)];

        select_session_row(&mut ui, "external");
        crate::actions::hide_selected(&backends, &mut ui, true);
        assert!(ui.mutations.in_flight("codex:external:hide"));

        select_session_row(&mut ui, "managed");
        crate::actions::hide_selected(&backends, &mut ui, true);
        assert!(ui.mutations.in_flight("codex:managed:hide"));
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

    // --- palette actions target the row the palette was opened on --------------------

    /// Move the palette highlight onto a specific row, so the assertion is about what Enter
    /// does to that row rather than about how the fuzzy ranker orders a query.
    fn highlight_palette_target(ui: &mut Ui, wanted: &PaletteTarget) {
        let Mode::Palette(palette) = &mut ui.mode else {
            panic!("expected the palette to be open");
        };
        for _ in 0..palette.result_count() {
            if palette.highlighted().map(|item| &item.target) == Some(wanted) {
                return;
            }
            palette.move_highlight(1);
        }
        panic!("the palette does not offer {wanted:?}");
    }

    /// Report which session each archive mutation actually ran against.
    fn recording_archive_executor(ui: &mut Ui) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        ui.mutation_executor = std::sync::Arc::new(move |mutation| {
            let crate::ops::Mutation::Hide(request) = mutation else {
                panic!("the palette archive row must only ever hide");
            };
            tx.lock()
                .expect("mutation recorder")
                .send(request.id().to_string())
                .expect("record the archived session");
            Ok(MutationOutcome {
                notice: String::new(),
                spawned: None,
            })
        });
        rx
    }

    /// The palette's ACTION rows read "the selected session", and the 1s refresh keeps running
    /// while the palette is up. Enter must archive the row the palette was opened on, not
    /// whichever row the selection was clamped onto in the meantime.
    #[test]
    fn a_palette_action_archives_the_row_it_was_opened_on_not_a_moved_selection() {
        let alpha = sess("alpha", "/tmp/agentviewer-palette-alpha", 200);
        let bravo = sess("bravo", "/tmp/agentviewer-palette-bravo", 100);
        let mut ui = test_ui_with(vec![alpha.clone(), bravo.clone()]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(ArchivingBackend)];
        let archived = recording_archive_executor(&mut ui);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Claude, alpha.id.clone()))
        );

        open_palette(&backends, &mut ui);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Claude, bravo.id.clone()))
        );
        highlight_palette_target(&mut ui, &PaletteTarget::Action(PaletteAction::Archive));
        press_palette_code(&mut ui, &backends, KeyCode::Enter);

        assert_eq!(
            archived
                .recv_timeout(std::time::Duration::from_secs(1))
                .as_deref(),
            Ok("alpha"),
            "the palette must archive the row it was opened on"
        );
    }

    /// And when that row has left the listing entirely, the action is refused rather than
    /// following the selection onto a session the user never chose.
    #[test]
    fn a_palette_action_whose_row_left_the_listing_notices_instead_of_acting() {
        let alpha = sess("alpha", "/tmp/agentviewer-palette-alpha", 200);
        let bravo = sess("bravo", "/tmp/agentviewer-palette-bravo", 100);
        let mut ui = test_ui_with(vec![alpha.clone(), bravo.clone()]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(ArchivingBackend)];
        let archived = recording_archive_executor(&mut ui);
        assert!(
            ui.app
                .select_by_key(&(BackendKind::Claude, alpha.id.clone()))
        );

        open_palette(&backends, &mut ui);
        // The refresh drops the row the palette was opened on; the selection clamps onto the
        // only row left.
        ui.app.set_sessions(vec![bravo.clone()]);
        highlight_palette_target(&mut ui, &PaletteTarget::Action(PaletteAction::Archive));
        press_palette_code(&mut ui, &backends, KeyCode::Enter);

        assert!(
            archived
                .recv_timeout(std::time::Duration::from_millis(250))
                .is_err(),
            "no session may be archived once the palette's row is gone"
        );
        assert_eq!(ui.notice.text(), "alpha is no longer listed");
        assert!(matches!(ui.mode, Mode::Normal));
    }

    // --- Ctrl+N triage inbox --------------------------------------------------------

    fn blocked_session(id: &str, updated_at_ms: i64) -> Session {
        let mut session = sess(id, "/home/me/git/acme/widget", updated_at_ms);
        session.status = agent_viewer_core::Status::NeedsInput {
            reason: Some("Pick a direction.".to_string()),
        };
        session
    }

    fn press_triage(ui: &mut Ui, code: KeyCode, modifiers: KeyModifiers) {
        handle_triage_key(key(code, modifiers), ui).expect("triage key routing");
    }

    fn press_palette_code(
        ui: &mut Ui,
        backends: &[Box<dyn agent_viewer_core::Backend>],
        code: KeyCode,
    ) {
        let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("fixed terminal");
        handle_palette_key(key(code, KeyModifiers::NONE), backends, ui, &mut terminal)
            .expect("palette key routing");
    }

    fn triage_state(ui: &Ui) -> &TriageState {
        match &ui.mode {
            Mode::Triage(state) => state,
            _ => panic!("expected the triage modal to be open"),
        }
    }

    /// Put a real child in the panel, standing in for the attached session. `cat` echoes what
    /// it is sent, so the pty screen is direct evidence of what actually reached the session.
    fn attach_a_child(ui: &mut Ui, key: crate::Key) {
        let pty = agent_viewer_core::pty::PtySession::spawn(agent_viewer_core::pty::PtySpec {
            program: "cat".to_string(),
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            rows: 10,
            cols: 40,
            palette: None,
            scrollback_rows: 0,
        })
        .expect("panel child");
        ui.focused = Some(key.clone());
        ui.attached.insert(key, pty);
    }

    fn screen_contains(ui: &Ui, needle: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let key = ui.focused.clone().expect("a focused child");
        while std::time::Instant::now() < deadline {
            let seen = ui.attached[&key].with_screen(|screen| screen.contents());
            if seen.contains(needle) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn ctrl_n_opens_triage_on_the_needs_input_queue_oldest_first() {
        let mut ui = test_ui_with(vec![
            blocked_session("newer", 3_000),
            sess("busy", "/tmp", 2_000),
            blocked_session("older", 1_000),
        ]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();

        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);

        assert!(matches!(ui.mode, Mode::Triage(_)), "Ctrl+N opens triage");
        let state = triage_state(&ui);
        assert_eq!(state.len(), 2, "only the blocked sessions are queued");
        assert_eq!(
            state.current().map(|item| item.id.as_str()),
            Some("older"),
            "the longest wait is first"
        );
    }

    #[test]
    fn ctrl_n_on_an_empty_queue_notices_instead_of_opening_a_modal() {
        let mut ui = test_ui_with(vec![sess("busy", "/tmp", 1_000)]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();

        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);

        assert!(
            matches!(ui.mode, Mode::Normal),
            "no modal over an empty queue"
        );
        assert_eq!(ui.notice.text(), "nothing waiting for input");
    }

    #[test]
    fn ctrl_n_does_not_collide_with_an_already_claimed_chord() {
        let claimed = [
            KeyCode::Char('a'),
            KeyCode::Char('d'),
            KeyCode::Char('f'),
            KeyCode::Char('k'),
            KeyCode::Char('r'),
            KeyCode::Char('u'),
        ];
        assert!(
            !claimed.contains(&KeyCode::Char('n')),
            "Ctrl+N must not be an already-claimed chord"
        );
    }

    #[test]
    fn the_command_palette_offers_triage_and_opens_it() {
        let mut ui = test_ui_with(vec![blocked_session("blocked", 100)]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();

        press_normal_key(&mut ui, &backends, 'k', KeyModifiers::CONTROL);
        let listed = match &ui.mode {
            Mode::Palette(state) => state
                .results()
                .find(|item| item.target == PaletteTarget::Action(PaletteAction::Triage))
                .cloned(),
            _ => panic!("Ctrl+K opens the palette"),
        }
        .expect("triage is offered in the palette");
        assert!(
            listed.enabled,
            "triage is a queue over every session, so no selected row can disable it"
        );

        for character in "triage".chars() {
            press_palette_code(&mut ui, &backends, KeyCode::Char(character));
        }
        press_palette_code(&mut ui, &backends, KeyCode::Enter);

        assert!(
            matches!(ui.mode, Mode::Triage(_)),
            "running the palette entry opens the same modal Ctrl+N does"
        );
    }

    /// The whole point of the panel: what you type reaches the agent, not a buffer of ours.
    #[test]
    fn typing_in_triage_reaches_the_session_rather_than_a_local_buffer() {
        let mut ui = test_ui_with(vec![blocked_session("blocked", 100)]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
        let item = triage_state(&ui).current().expect("an item").key();
        attach_a_child(&mut ui, item);

        for character in "artifact-first".chars() {
            press_triage(&mut ui, KeyCode::Char(character), KeyModifiers::NONE);
        }
        press_triage(&mut ui, KeyCode::Enter, KeyModifiers::NONE);

        assert!(
            screen_contains(&ui, "artifact-first"),
            "the answer must arrive at the session itself"
        );
        assert!(
            matches!(ui.mode, Mode::Triage(_)),
            "answering keeps you in the queue"
        );
    }

    /// Esc, Enter, arrows and digits are how you answer an agent's prompt. Reserving any of
    /// them for the queue would break the panel for the sessions it exists to serve.
    #[test]
    fn escape_and_the_arrows_belong_to_the_session_not_the_queue() {
        let mut ui = test_ui_with(vec![blocked_session("blocked", 100)]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
        let item = triage_state(&ui).current().expect("an item").key();
        attach_a_child(&mut ui, item);

        for code in [KeyCode::Esc, KeyCode::Down, KeyCode::Char('2')] {
            press_triage(&mut ui, code, KeyModifiers::NONE);
            assert!(
                matches!(ui.mode, Mode::Triage(_)),
                "{code:?} must not close or redirect the modal"
            );
        }
        assert!(
            screen_contains(&ui, "2"),
            "a digit types into the session like any other key"
        );
    }

    #[test]
    fn ctrl_n_walks_the_queue_and_ctrl_p_walks_back() {
        let mut ui = test_ui_with(vec![
            blocked_session("older", 100),
            blocked_session("newer", 200),
        ]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
        assert_eq!(triage_state(&ui).progress(), (1, 2));

        press_triage(&mut ui, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(triage_state(&ui).progress(), (2, 2));
        assert_eq!(
            triage_state(&ui).current().map(|item| item.id.as_str()),
            Some("newer")
        );

        press_triage(&mut ui, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(triage_state(&ui).progress(), (1, 2));
    }

    #[test]
    fn walking_past_the_last_item_closes_the_modal_without_wrapping() {
        let mut ui = test_ui_with(vec![blocked_session("only", 100)]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);

        press_triage(&mut ui, KeyCode::Char('n'), KeyModifiers::CONTROL);

        assert!(
            matches!(ui.mode, Mode::Normal),
            "running off the end lands on the list rather than wrapping to the top"
        );
    }

    /// Both encodings of Ctrl+] must leave. A live run found the literal-only match left the
    /// chord dead in the terminal, because crossterm's legacy parser folds 0x1D onto Ctrl+5.
    #[test]
    fn both_encodings_of_ctrl_bracket_leave_the_queue() {
        for code in [KeyCode::Char(']'), KeyCode::Char('5')] {
            let mut ui = test_ui_with(vec![blocked_session("blocked", 100)]);
            let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
            press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
            assert!(matches!(ui.mode, Mode::Triage(_)));

            press_triage(&mut ui, code, KeyModifiers::CONTROL);

            assert!(
                matches!(ui.mode, Mode::Normal),
                "{code:?} with CTRL must leave the queue"
            );
        }
    }

    #[test]
    fn ctrl_bracket_leaves_the_queue_and_leaves_the_composer_and_selection_alone() {
        let mut ui = test_ui_with(vec![
            blocked_session("blocked", 100),
            sess("other", "/tmp", 200),
            sess("third", "/tmp", 300),
        ]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        for character in "half a task".chars() {
            press_normal_key(&mut ui, &backends, character, KeyModifiers::NONE);
        }
        press_normal_code(&mut ui, &backends, KeyCode::Down, KeyModifiers::NONE);
        let composer_before = ui.composer.text().to_string();
        let selection_before = ui.app.selected_index();

        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
        let item = triage_state(&ui).current().expect("an item").key();
        attach_a_child(&mut ui, item);
        for character in "an answer".chars() {
            press_triage(&mut ui, KeyCode::Char(character), KeyModifiers::NONE);
        }
        press_triage(&mut ui, KeyCode::Char(']'), KeyModifiers::CONTROL);

        assert!(matches!(ui.mode, Mode::Normal), "Ctrl+] lands on the list");
        assert_eq!(
            ui.composer.text(),
            composer_before,
            "typing into the session must never reach the composer"
        );
        assert_eq!(ui.app.selected_index(), selection_before);
    }

    /// Ctrl+C in the triage panel is an interrupt for the session being answered. Quitting
    /// there tears down every PTY the viewer owns while a child is live, which is the opposite
    /// of what a user stopping a runaway answer is asking for.
    #[test]
    fn ctrl_c_in_triage_interrupts_the_session_rather_than_quitting_the_viewer() {
        let mut ui = test_ui_with(vec![blocked_session("blocked", 100)]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
        let item = triage_state(&ui).current().expect("an item").key();
        attach_a_child(&mut ui, item.clone());

        let quit = press_normal_key(&mut ui, &backends, 'c', KeyModifiers::CONTROL);

        assert!(!quit, "Ctrl+C must not tear the viewer down from triage");
        assert!(matches!(ui.mode, Mode::Triage(_)));
        // `cat` runs with ISIG on, so 0x03 reaching it is observable as the child dying.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !ui.attached.get_mut(&item).expect("panel child").is_exited() {
            assert!(
                std::time::Instant::now() < deadline,
                "Ctrl+C must reach the session in the panel as an interrupt"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// With nothing live in the panel there is no child to interrupt, so the chord keeps its
    /// global meaning rather than becoming a dead key.
    #[test]
    fn ctrl_c_in_triage_without_a_live_child_still_quits() {
        let mut ui = test_ui_with(vec![blocked_session("blocked", 100)]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
        assert!(ui.attached.is_empty());

        assert!(press_normal_key(
            &mut ui,
            &backends,
            'c',
            KeyModifiers::CONTROL
        ));
    }

    /// A triage visit lasts exactly as long as the item is in the panel: walking on closes the
    /// child it left, so a long queue does not accumulate invisible processes.
    #[test]
    fn walking_to_the_next_item_closes_the_child_it_left() {
        let mut ui = test_ui_with(vec![
            blocked_session("older", 100),
            blocked_session("newer", 200),
        ]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
        let first = triage_state(&ui).current().expect("an item").key();
        attach_a_child(&mut ui, first.clone());

        press_triage(&mut ui, KeyCode::Char('n'), KeyModifiers::CONTROL);

        assert!(
            !ui.attached.contains_key(&first),
            "the item the queue walked off must not stay connected off screen"
        );
        assert!(
            !ui.detach_trackers.contains_key(&first),
            "its per-PTY state goes with it"
        );
    }

    /// Same on the way back: Ctrl+P is a move like any other.
    #[test]
    fn walking_back_closes_the_child_it_left() {
        let mut ui = test_ui_with(vec![
            blocked_session("older", 100),
            blocked_session("newer", 200),
        ]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
        press_triage(&mut ui, KeyCode::Char('n'), KeyModifiers::CONTROL);
        let second = triage_state(&ui).current().expect("an item").key();
        attach_a_child(&mut ui, second.clone());

        press_triage(&mut ui, KeyCode::Char('p'), KeyModifiers::CONTROL);

        assert!(!ui.attached.contains_key(&second));
    }

    #[test]
    fn leaving_the_queue_closes_the_child_it_was_showing() {
        let mut ui = test_ui_with(vec![blocked_session("blocked", 100)]);
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = Vec::new();
        press_normal_key(&mut ui, &backends, 'n', KeyModifiers::CONTROL);
        let item = triage_state(&ui).current().expect("an item").key();
        attach_a_child(&mut ui, item.clone());

        press_triage(&mut ui, KeyCode::Char(']'), KeyModifiers::CONTROL);

        assert!(matches!(ui.mode, Mode::Normal));
        assert!(
            !ui.attached.contains_key(&item),
            "nothing stays connected once the queue is closed"
        );
    }

    #[test]
    fn plain_c_and_other_ctrl_chords_are_not_quit() {
        // A bare 'c' types into the composer; other Ctrl-chords keep their own actions.
        let plain_c = key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!is_quit_chord(plain_c, false, false));
        let ctrl_x = key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(!is_quit_chord(ctrl_x, true, false));
    }
}
