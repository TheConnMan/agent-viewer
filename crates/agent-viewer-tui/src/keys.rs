//! Key routing for every input mode, plus the actions those keys trigger (attach, spawn,
//! rename, stop/remove, hide). Everything here mutates the shared `Ui` state owned by the
//! run loop in `main.rs`.

use std::io;
use std::time::Instant;

use agent_viewer_core::backend::{Backend, BackendKind, Capabilities};
use agent_viewer_core::claude::ensure_trusted;
use agent_viewer_core::pty::{PtySession, spec_from_command};
use agent_viewer_core::spawn::now_ms;
use agent_viewer_core::{Session, Status};
use agent_viewer_tui::app::{
    CodexReply, DetachTracker, KillStage, codex_reply_keystroke, file_stems, reply_allowed,
    subdir_names,
};
use agent_viewer_tui::attach::key_to_bytes;
use agent_viewer_tui::ui::{Mode, RenameModal, ReplyModal};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::auto_enter::{AutoEnter, AutoEnterStage};
use crate::ops::{Mutation, run_mutation};
use crate::pending_reply::PendingReply;
use crate::{Key, Refresher, Ui};

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

    // Ctrl-chords always act, regardless of composer state.
    if ctrl {
        match key.code {
            KeyCode::Char('s') => ui.app.toggle_group_mode(),
            KeyCode::Char('r') => open_rename(ui),
            KeyCode::Char('x') => kill_selected(backends, ui),
            KeyCode::Char('f') => open_filter(ui),
            _ => {}
        }
        return Ok(false);
    }

    // While the slash-command popup is open, Up/Down/Tab/Esc drive it instead of the list.
    let suggesting = ui.composer.suggestions_active();
    match key.code {
        KeyCode::Down if suggesting => ui.composer.move_suggestion(1),
        KeyCode::Up if suggesting => ui.composer.move_suggestion(-1),
        // Arrows navigate/act at all times (App collapses any inline peek on the move).
        KeyCode::Down => ui.app.move_selection(1),
        KeyCode::Up => ui.app.move_selection(-1),
        KeyCode::Right => attach_selected(backends, ui, terminal)?,
        // Tab accepts the highlighted suggestion while the popup is open, else cycles the
        // target backend; Shift+Tab cycles that backend's model.
        KeyCode::Tab if suggesting => {
            ui.composer.accept_suggestion();
        }
        KeyCode::Tab => ui.composer.cycle_backend(),
        KeyCode::BackTab => ui.composer.cycle_model(),
        KeyCode::Backspace => ui.composer.backspace(),
        // Esc dismisses an open popup first; a second Esc clears the composer as before.
        KeyCode::Esc if suggesting => ui.composer.dismiss_suggestions(),
        KeyCode::Esc => ui.composer.clear(),
        KeyCode::Enter => {
            if ui.composer.is_empty() {
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
                    'r' => open_reply(backends, ui),
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

    // Any key here is the user taking over, so cancel a pending auto-Enter or reply injection
    // on this attach (do not type our queued reply in behind the user's own input).
    ui.auto_enter = None;
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

// --- Actions --------------------------------------------------------------------

/// Enter/Space on a header toggles + persists the collapse. Returns true when a header was
/// handled (so the caller skips attach/peek).
fn toggle_group_if_header(ui: &mut Ui) -> bool {
    let Some((key, collapsed)) = ui.app.toggle_selected_group() else {
        return false;
    };
    if let Some(db) = &ui.db {
        let _ = db.set_group_collapsed(&key.to_storage(), collapsed);
    }
    true
}

/// Ctrl+F — enter filter mode with a fresh, empty query.
fn open_filter(ui: &mut Ui) {
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
fn ensure_completions(ui: &mut Ui) {
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

/// Open the rename modal for the selected session (claude falls back to the local
/// name override on apply, so it opens regardless of the backend's rename capability).
fn open_rename(ui: &mut Ui) {
    let Some(session) = ui.app.selected() else {
        return;
    };
    ui.mode = Mode::Rename(RenameModal {
        backend: session.backend,
        id: session.id.clone(),
        buffer: session.title.clone(),
    });
}

/// `r` on an empty composer — focus a reply input for the selected session, gated on the
/// backend supporting reply AND the session actually being blocked (the sole safety gate).
/// Force the peek open so the pending ask stays visible above the input.
fn open_reply(backends: &[Box<dyn Backend>], ui: &mut Ui) {
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
/// have changed while typing), attach (reusing the auto-Enter landing path), then arm the
/// one-shot injector to write the payload once we are safely in the run. Claude sends the
/// text + Enter; codex maps y/n approvals to a single keystroke and otherwise attaches with
/// focus for the user to finish; opencode is gated out upstream.
fn send_reply(
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

    if !attach_session(backends, ui, terminal, &session)? {
        return Ok(());
    }
    if let Some(payload) = payload {
        let key: Key = (backend_kind, id);
        // require_run_view: only claude opens onto the agents list; codex resume lands in the
        // pending prompt. fresh_attach: attach_session arms auto_enter exactly for a fresh
        // live-claude list attach (so the injector waits for its explicit landing); a reused
        // PTY leaves auto_enter unarmed, so run-view is confirmed by the marker being absent.
        let require_run_view = matches!(backend_kind, BackendKind::Claude);
        let fresh_attach = ui.auto_enter.as_ref().map(|ae| &ae.key) == Some(&key);
        ui.pending_reply = Some(PendingReply {
            key,
            payload,
            require_run_view,
            fresh_attach,
            armed_at: Instant::now(),
            ready_since: None,
        });
    }
    Ok(())
}

/// Submit the rename to the background runner (the app-server/UDS rename can take 1-2s).
fn apply_rename(ui: &mut Ui) {
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

fn kill_selected(backends: &[Box<dyn Backend>], ui: &mut Ui) {
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

fn hide_selected(backends: &[Box<dyn Backend>], ui: &mut Ui, hide: bool) {
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
        .unwrap_or(Capabilities {
            spawn: false,
            hide: false,
            attach: false,
            stop: false,
            remove: false,
            rename: false,
            reply: false,
        })
}

fn attach_selected(
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
/// live PTY (resize) or spawn one, arm the live-claude auto-Enter, and focus it. Returns true
/// when it ended attached (Mode::Attached), false when it bailed with a notice.
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
        // Pre-accept the trust dialog before a claude RESUME attach into a fresh project
        // (best-effort; the live agents-view path and other backends never need it).
        let claude_live =
            session.pid.is_some() || matches!(session.status, Status::Working | Status::NeedsInput);
        if session.backend == BackendKind::Claude && !claude_live {
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
                // A live claude attach opens the agents view; arm the one-shot auto-Enter
                // to land in the preselected run (only on a fresh spawn, never a re-attach).
                if session.backend == BackendKind::Claude && claude_live {
                    ui.auto_enter = Some(AutoEnter {
                        key: key.clone(),
                        armed_at: Instant::now(),
                        stage: AutoEnterStage::AwaitingList,
                        marker_since: None,
                        expanded_absent_seen: false,
                    });
                    // A fresh attach has not landed yet: clear any stale landing signal so the
                    // reply injector waits for THIS attach's explicit success.
                    ui.auto_enter_landed = None;
                }
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
fn spawn_from_composer(backends: &[Box<dyn Backend>], refresher: &Refresher, ui: &mut Ui) {
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

#[cfg(test)]
mod tests {
    use super::{ensure_completions, is_quit_chord, open_filter};
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
            pulses: Pulses::new(),
            pr_status: agent_viewer_tui::pr_cache::PrStatusCache::new(),
            auto_enter: None,
            auto_enter_landed: None,
            pending_reply: None,
            attached: HashMap::new(),
            focused: None,
            focused_session: None,
            focused_exited: false,
            logos: None,
        }
    }

    fn sess(id: &str, cwd: &str, updated_at_ms: i64) -> Session {
        Session {
            backend: BackendKind::Claude,
            id: id.into(),
            short_id: None,
            title: id.into(),
            cwd: std::path::PathBuf::from(cwd),
            created_at_ms: updated_at_ms,
            updated_at_ms,
            status: Status::Done,
            hidden: false,
            source_label: "test".into(),
            summary: String::new(),
            companion: false,
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
        }
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
            Some((BackendKind::Claude, Some("/tmp/agentviewer-target-a".into())))
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
            Some(&(BackendKind::Claude, Some("/tmp/agentviewer-target-b".into())))
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
    fn plain_c_and_other_ctrl_chords_are_not_quit() {
        // A bare 'c' types into the composer; other Ctrl-chords keep their own actions.
        let plain_c = key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!is_quit_chord(plain_c, false, &Mode::Normal));
        let ctrl_x = key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(!is_quit_chord(ctrl_x, true, &Mode::Normal));
    }
}
