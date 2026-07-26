//! Key routing for every input mode, plus the actions those keys trigger (attach, spawn,
//! rename, stop/remove, hide). Everything here mutates the shared `Ui` state owned by the
//! run loop in `main.rs`.

use std::io;

use agent_viewer_core::backend::Backend;
use agent_viewer_tui::attach::key_to_bytes;
use agent_viewer_tui::ui::Mode;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::actions::{
    apply_rename, attach_selected, ensure_completions, ensure_models, hide_selected, kill_selected,
    open_filter, open_rename, open_reply, send_reply, spawn_from_composer, toggle_group_if_header,
};
use crate::{Refresher, Ui};

/// Returns `true` when the app should quit.
pub(crate) fn handle_key(
    key: KeyEvent,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<bool> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl+C kills the whole viewer (like `claude agents`) from every mode except an active
    // attach — there Ctrl+C must reach the child as an interrupt (0x03) so a runaway agent can
    // be stopped without tearing down the viewer. (macOS Cmd+C is swallowed by the terminal as
    // copy and never reaches us; Ctrl+C is the portable interrupt on macOS and Windows alike.)
    if is_quit_chord(key, ctrl, &ui.mode) {
        ui.attached.clear(); // drop = kill owned children, same teardown as `q`
        return Ok(true);
    }
    // Ctrl+T flips mouse capture, which is the only way to get the terminal's own text
    // selection back (with capture on, drag-select is swallowed as mouse reports, and the
    // Shift override is not universal across terminals). Claimed in EVERY mode, attach
    // included: the attached transcript is the surface users most want to copy out of, so
    // the child does not get this chord.
    if is_mouse_toggle_chord(key, ctrl) {
        set_mouse_capture(ui, !ui.mouse_capture);
        return Ok(false);
    }
    match &mut ui.mode {
        Mode::Attached => handle_attached_key(key, ui),
        Mode::Normal => return handle_normal_key(key, ctrl, backends, refresher, ui, terminal),
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

/// Route a mouse event. While attached, forward it to the focused child PTY as a native
/// mouse report (the child, e.g. codex, has its own mouse tracking on and scrolls itself),
/// so the wheel scrolls the transcript instead of the terminal's alternate-scroll turning it
/// into arrow keys codex reads as prompt-history navigation. In the list, click or hover
/// selects the row under the cursor and the wheel walks the selection (hit-testing reads the
/// geometry `draw` recorded on the last frame). Modals own their surface, so mouse is a no-op
/// there (the terminal's own text selection still works with Shift held).
pub(crate) fn handle_mouse(me: MouseEvent, ui: &mut Ui) {
    // Text-select mode: the terminal owns the mouse, so any report still in flight (or sent
    // by a terminal that ignored the disable sequence) must not steer the selection.
    if !ui.mouse_capture {
        return;
    }
    match &ui.mode {
        Mode::Attached => {
            let Some(fkey) = ui.focused.clone() else {
                return;
            };
            let Some(pty) = ui.attached.get_mut(&fkey) else {
                return;
            };
            let (mode, encoding) =
                pty.with_screen(|s| (s.mouse_protocol_mode(), s.mouse_protocol_encoding()));
            // draw_attach draws a one-row header above the child screen, so offset by 1.
            if let Some(bytes) = agent_viewer_tui::mouse::encode_mouse_report(me, mode, encoding, 1)
            {
                let _ = pty.write_input(&bytes);
            }
        }
        Mode::Normal => match me.kind {
            // Left click and bare hover both land the selection on the row under the cursor.
            MouseEventKind::Moved | MouseEventKind::Down(MouseButton::Left) => {
                if let Some(idx) = ui.list_hit.borrow().row_at(me.column, me.row) {
                    ui.app.select_visible_index(idx);
                }
            }
            // The wheel nudges the selection one selectable row at a time (same as arrows).
            MouseEventKind::ScrollDown => ui.app.move_selection(1),
            MouseEventKind::ScrollUp => ui.app.move_selection(-1),
            _ => {}
        },
        _ => {}
    }
}

/// Ctrl+T is the app-wide mouse-capture toggle. Pure predicate so the chord is unit-testable
/// without a live terminal; a bare `t` must stay composer text.
fn is_mouse_toggle_chord(key: KeyEvent, ctrl: bool) -> bool {
    ctrl && matches!(key.code, KeyCode::Char('t'))
}

/// Turn mouse reporting on or off, pushing the matching terminal mode change and telling the
/// user which mode they are in. Off hands the mouse back to the terminal so drag-select and
/// copy work natively; on restores click/hover row selection and wheel forwarding to an
/// attached child. Best-effort: a terminal that rejects the sequence still gets the flag flip,
/// which is what gates `handle_mouse`.
pub(crate) fn set_mouse_capture(ui: &mut Ui, on: bool) {
    use crossterm::execute;
    apply_mouse_capture_state(ui, on);
    let _ = if on {
        execute!(io::stdout(), crossterm::event::EnableMouseCapture)
    } else {
        execute!(io::stdout(), crossterm::event::DisableMouseCapture)
    };
}

/// The state half of `set_mouse_capture`: flip the flag and set the footer notice, with no
/// terminal I/O. Split out so tests can exercise it without writing real mode sequences to
/// the developer's stdout — a test that enabled capture and exited would leave the invoking
/// shell unable to drag-select, which is the very bug this toggle exists to fix.
fn apply_mouse_capture_state(ui: &mut Ui, on: bool) {
    ui.mouse_capture = on;
    ui.set_notice(
        if on {
            "mouse on - click/hover selects, wheel scrolls (ctrl+t to select text)"
        } else {
            "mouse off - drag to select and copy (ctrl+t to restore mouse)"
        }
        .to_string(),
    );
}

/// Ctrl+C is the app-wide "kill the viewer" chord, except while attached — there it is
/// forwarded to the child as a raw interrupt instead. Kept as a pure predicate so the quit
/// decision is unit-testable without a live terminal.
fn is_quit_chord(key: KeyEvent, ctrl: bool, mode: &Mode) -> bool {
    ctrl && matches!(key.code, KeyCode::Char('c')) && !matches!(mode, Mode::Attached)
}

fn handle_normal_key(
    key: KeyEvent,
    ctrl: bool,
    backends: &[Box<dyn Backend>],
    refresher: &Refresher,
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
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
            KeyCode::Char('s') => ui.app.toggle_group_mode(),
            KeyCode::Char('r') => open_rename(backends, ui),
            KeyCode::Char('e') => open_reply(backends, ui),
            KeyCode::Char('x') => kill_selected(backends, ui),
            KeyCode::Char('f') => open_filter(ui),
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
    match key.code {
        KeyCode::Down if suggesting || model_cmd => ui.composer.move_suggestion(1),
        KeyCode::Up if suggesting || model_cmd => ui.composer.move_suggestion(-1),
        // Arrows navigate/act at all times (App collapses any inline peek on the move).
        KeyCode::Down => ui.app.move_selection(1),
        KeyCode::Up => ui.app.move_selection(-1),
        KeyCode::Right => attach_selected(backends, ui, terminal)?,
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
        KeyCode::Tab => ui.composer.cycle_backend(),
        KeyCode::BackTab => ui.composer.cycle_model(),
        KeyCode::Backspace => ui.composer.backspace(),
        // Esc dismisses an open popup first; a second Esc clears the composer as before.
        KeyCode::Esc if suggesting => ui.composer.dismiss_suggestions(),
        KeyCode::Esc => ui.composer.clear(),
        KeyCode::Enter => {
            if ui.composer.model_picking() {
                // A /model picker is up: Enter picks the highlighted model.
                ui.composer.accept_model();
            } else if ui.composer.is_model_command() {
                // A /model command with no matches: a meta-command, nothing to spawn.
            } else if ui.composer.is_empty() {
                // On a group header, Enter collapses/expands the group (and persists) instead
                // of attaching; on a session it attaches as before.
                if !toggle_group_if_header(ui) {
                    attach_selected(backends, ui, terminal)?;
                }
            } else {
                spawn_from_composer(backends, refresher, ui);
            }
        }
        KeyCode::Char(c) => {
            if ui.composer.is_empty() {
                // Empty composer: the command hotkeys still fire; any other printable
                // starts a task (n included — it just types).
                match c {
                    'q' => {
                        ui.attached.clear(); // drop = kill owned children
                        return Ok(true);
                    }
                    'a' => ui.app.toggle_show_all(),
                    'h' => hide_selected(backends, ui, true),
                    'u' => hide_selected(backends, ui, false),
                    '?' => ui.mode = Mode::Help,
                    // On a header, Space collapses/expands the group (and persists), never
                    // typing a space; on a session it toggles the inline peek; otherwise it
                    // is composer text.
                    ' ' => {
                        if !toggle_group_if_header(ui) {
                            if ui.app.selected().is_some() {
                                ui.app.toggle_expanded();
                            } else {
                                ui.composer.push_char(' ');
                            }
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
    // Refresh the slash-command list for the (possibly new) backend/target and text.
    ensure_completions(ui);
    ensure_models(ui);
    Ok(false)
}

/// While attached: Ctrl+] always detaches; Left detaches when the input line is empty
/// (else it is forwarded); a dead child detaches on any key; everything else is encoded
/// to bytes and written to the PTY.
fn handle_attached_key(key: KeyEvent, ui: &mut Ui) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(fkey) = ui.focused.clone() else {
        ui.mode = Mode::Normal;
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
fn handle_reply_key(
    code: KeyCode,
    backends: &[Box<dyn Backend>],
    ui: &mut Ui,
    terminal: &mut ratatui::DefaultTerminal,
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
mod tests {
    use super::{ensure_completions, handle_rename_key, is_quit_chord, open_filter};
    use crate::{NoticeState, Ui};
    use agent_viewer_core::{BackendKind, Session, Status};
    use agent_viewer_tui::app::{App, Composer};
    use agent_viewer_tui::mutations::MutationRunner;
    use agent_viewer_tui::ui::{Mode, PeekCache, Pulses};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::HashMap;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn test_ui_with(sessions: Vec<Session>) -> Ui {
        Ui {
            app: App::new(sessions),
            mode: Mode::Normal,
            notice: NoticeState::default(),
            db: None,
            peek: PeekCache::new(),
            composer: Composer::new(),
            detach_trackers: HashMap::new(),
            last_backend_error: String::new(),
            mutations: MutationRunner::new(),
            models: agent_viewer_tui::model_cache::ModelCache::new(),
            pulses: Pulses::new(),
            pr_status: agent_viewer_tui::pr_cache::PrStatusCache::new(),
            pending_reply: None,
            attached: HashMap::new(),
            focused: None,
            focused_session: None,
            focused_exited: false,
            logos: None,
            list_hit: std::cell::RefCell::new(agent_viewer_tui::ui::ListHit::default()),
            mouse_capture: true,
        }
    }

    fn sess(id: &str, cwd: &str, updated_at_ms: i64) -> Session {
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

    /// A bg row: it carries the short id that names its job dir, so rename applies to it.
    fn bg_sess(id: &str, cwd: &str, updated_at_ms: i64) -> Session {
        Session {
            short_id: Some(id.to_string()),
            origin: agent_viewer_core::SessionOrigin::Background,
            ..sess(id, cwd, updated_at_ms)
        }
    }

    /// Move the selection onto the session row for `id` (row 0 is a section header).
    fn select_session_row(ui: &mut Ui, id: &str) {
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
        ) -> agent_viewer_core::Result<Option<u32>> {
            unreachable!("spawn is not exercised by the rename key tests")
        }
        fn attach_command(
            &self,
            _session: &Session,
        ) -> std::result::Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attach is not exercised by the rename key tests")
        }
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
    fn ctrl_r_is_gated_per_row_not_per_backend() {
        // The claude backend advertises rename but only bg rows have the job dir it writes,
        // so the gate must ask the ROW. A capability advertised and then failing at press
        // time is worse than one advertised unsupported up front.
        let mut ui = test_ui_with(vec![sess("s1", "/tmp/agentviewer-rename", 100)]);
        select_session_row(&mut ui, "s1"); // sess() builds rows with no short id
        let backends: Vec<Box<dyn agent_viewer_core::Backend>> = vec![Box::new(RenamingBackend)];

        crate::actions::open_rename(&backends, &mut ui);

        assert!(matches!(ui.mode, Mode::Normal), "must not open the editor");
        assert_eq!(ui.notice.text, "claude does not support rename");
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
    fn set_mouse_capture_flips_state_and_names_the_way_back() {
        use super::apply_mouse_capture_state as set_capture;
        let mut ui = test_ui_with(Vec::new());
        assert!(ui.mouse_capture, "capture starts on");

        // Off: the flag drops and the footer tells the user both what changed and how to undo
        // it, because the mode is otherwise invisible on screen.
        set_capture(&mut ui, false);
        assert!(!ui.mouse_capture);
        let off = ui.notice.text().to_string();
        assert!(off.contains("drag to select"), "notice was {off:?}");
        assert!(off.contains("ctrl+t"), "notice must name the way back: {off:?}");

        // Back on: flag restored, and the notice again names the escape hatch.
        set_capture(&mut ui, true);
        assert!(ui.mouse_capture);
        let on = ui.notice.text().to_string();
        assert!(on.contains("click/hover"), "notice was {on:?}");
        assert!(on.contains("ctrl+t"), "notice must name the way back: {on:?}");
    }

    #[test]
    fn mouse_events_are_ignored_while_capture_is_off() {
        use super::handle_mouse;
        use crossterm::event::{MouseEvent, MouseEventKind};

        let mut ui = test_ui_with(vec![
            sess("a", "/tmp/agentviewer-mouse-a", 200),
            sess("b", "/tmp/agentviewer-mouse-b", 100),
        ]);
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
        handle_mouse(wheel, &mut ui);
        let moved = ui.app.selected_index();
        assert_ne!(moved, start, "with capture on the wheel must move selection");

        // Capture off (text-select mode): the same wheel event changes nothing. While the
        // terminal owns the mouse, a stray report must not steer the selection.
        let before = ui.app.selected_index();
        ui.mouse_capture = false;
        handle_mouse(wheel, &mut ui);
        assert_eq!(
            ui.app.selected_index(),
            before,
            "the wheel must be inert while mouse capture is off"
        );
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
    fn plain_c_and_other_ctrl_chords_are_not_quit() {
        // A bare 'c' types into the composer; other Ctrl-chords keep their own actions.
        let plain_c = key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!is_quit_chord(plain_c, false, &Mode::Normal));
        let ctrl_x = key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(!is_quit_chord(ctrl_x, true, &Mode::Normal));
    }
}
