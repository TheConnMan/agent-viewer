//! Mouse-event-to-bytes encoding for the embedded-attach view. Sibling of `attach.rs`
//! (the key-to-bytes module): a pure function turning a crossterm `MouseEvent` into the
//! xterm mouse report the child PTY expects, driven by the child's own advertised mouse
//! mode/encoding (read from its vt100 screen). No terminal I/O, no `Ui`.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tui_term::vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// Which class of report the event is, for mode gating. The child only wants some classes
/// depending on its active mouse mode (press-only, press+release, motion, any-motion).
enum Class {
    /// A press or a scroll wheel tick. Reported in every non-None mode.
    PressScroll,
    /// A button release. Reported only in PressRelease/ButtonMotion/AnyMotion.
    Release,
    /// A drag (motion with a button held). Reported only in ButtonMotion/AnyMotion.
    Drag,
    /// Bare pointer motion (no button). Reported only in AnyMotion.
    Moved,
}

/// xterm button numbers for the three buttons (Left=0, Middle=1, Right=2).
fn btn_num(btn: MouseButton) -> u16 {
    match btn {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Encode a crossterm `MouseEvent` into the child's expected mouse report bytes, or None
/// when the child does not want this event (mode None, event in the header rows above the
/// child screen, a class the active mode does not report, or an unencodable coordinate).
///
/// `row_offset` is how many rows the attach view draws above the child's top-left cell
/// (the attach view has a one-row header, so the caller passes 1).
pub fn encode_mouse_report(
    ev: MouseEvent,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
    row_offset: u16,
) -> Option<Vec<u8>> {
    // The child has mouse tracking off: nothing to send.
    if mode == MouseProtocolMode::None {
        return None;
    }
    // Events over the header belong to the viewer, not the child screen.
    if ev.row < row_offset {
        return None;
    }
    let child_row = ev.row - row_offset;
    let child_col = ev.column;

    let (base, release, class) = match ev.kind {
        MouseEventKind::ScrollUp => (64u16, false, Class::PressScroll),
        MouseEventKind::ScrollDown => (65, false, Class::PressScroll),
        MouseEventKind::ScrollLeft => (66, false, Class::PressScroll),
        MouseEventKind::ScrollRight => (67, false, Class::PressScroll),
        MouseEventKind::Down(btn) => (btn_num(btn), false, Class::PressScroll),
        MouseEventKind::Up(btn) => (btn_num(btn), true, Class::Release),
        // Drag/Moved carry the motion bit (+32) in the button code.
        MouseEventKind::Drag(btn) => (btn_num(btn) + 32, false, Class::Drag),
        MouseEventKind::Moved => (3 + 32, false, Class::Moved),
    };

    // Mode gating: drop event classes the child's active mode does not report.
    let reported = match class {
        Class::PressScroll => true,
        Class::Release => matches!(
            mode,
            MouseProtocolMode::PressRelease
                | MouseProtocolMode::ButtonMotion
                | MouseProtocolMode::AnyMotion
        ),
        Class::Drag => matches!(
            mode,
            MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
        ),
        Class::Moved => matches!(mode, MouseProtocolMode::AnyMotion),
    };
    if !reported {
        return None;
    }

    // Modifier bits fold into the button code: Shift +4, Alt +8, Ctrl +16.
    let mut modbits = 0u16;
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        modbits += 4;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        modbits += 8;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        modbits += 16;
    }
    let cb = base + modbits;

    // 1-based coordinates.
    let cx = child_col + 1;
    let cy = child_row + 1;

    match encoding {
        MouseProtocolEncoding::Sgr => {
            // SGR keeps the button number even on release; the final char signals it
            // instead ('m' for release, 'M' for press/scroll/motion).
            let final_char = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{cb};{cx};{cy}{final_char}").into_bytes())
        }
        MouseProtocolEncoding::Default => {
            // X10 cannot express which button was released, so a release reports the
            // no-button code (3) plus the modifier bits.
            let cb_x10 = if release { 3 + modbits } else { cb };
            let b_cb = 32u16 + cb_x10;
            let b_cx = 32u16 + cx;
            let b_cy = 32u16 + cy;
            if b_cb > 255 || b_cx > 255 || b_cy > 255 {
                return None;
            }
            Some(vec![0x1b, b'[', b'M', b_cb as u8, b_cx as u8, b_cy as u8])
        }
        MouseProtocolEncoding::Utf8 => {
            // Like Default, but the coordinates are UTF-8 chars of codepoint 32+value, so
            // values past 127 become multi-byte. The button byte stays a single byte.
            let cb_x10 = if release { 3 + modbits } else { cb };
            let b_cb = 32u16 + cb_x10;
            if b_cb > 255 {
                return None;
            }
            let cx_char = char::from_u32(32u32 + cx as u32)?;
            let cy_char = char::from_u32(32u32 + cy as u32)?;
            let mut out = vec![0x1b, b'[', b'M', b_cb as u8];
            let mut buf = [0u8; 4];
            out.extend_from_slice(cx_char.encode_utf8(&mut buf).as_bytes());
            out.extend_from_slice(cy_char.encode_utf8(&mut buf).as_bytes());
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers,
        }
    }

    #[test]
    fn mode_none_returns_none() {
        let e = ev(MouseEventKind::ScrollUp, 5, 3, KeyModifiers::NONE);
        assert_eq!(
            encode_mouse_report(e, MouseProtocolMode::None, MouseProtocolEncoding::Sgr, 1),
            None
        );
    }

    #[test]
    fn header_row_event_returns_none() {
        // row 0 is above the 1-row header offset, so it is not on the child screen.
        let e = ev(MouseEventKind::ScrollUp, 5, 0, KeyModifiers::NONE);
        assert_eq!(
            encode_mouse_report(e, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr, 1),
            None
        );
    }

    #[test]
    fn scroll_up_sgr_at_offset_one() {
        // column 5 -> cx 6; row 3 with offset 1 -> child_row 2 -> cy 3.
        let e = ev(MouseEventKind::ScrollUp, 5, 3, KeyModifiers::NONE);
        let out = encode_mouse_report(e, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr, 1)
            .unwrap();
        assert_eq!(out, b"\x1b[<64;6;3M".to_vec());
    }

    #[test]
    fn scroll_down_sgr_is_sixty_five() {
        let e = ev(MouseEventKind::ScrollDown, 5, 3, KeyModifiers::NONE);
        let out = encode_mouse_report(e, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr, 1)
            .unwrap();
        assert_eq!(out, b"\x1b[<65;6;3M".to_vec());
    }

    #[test]
    fn ctrl_scroll_up_sgr_adds_sixteen() {
        let e = ev(MouseEventKind::ScrollUp, 5, 3, KeyModifiers::CONTROL);
        let out = encode_mouse_report(e, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr, 1)
            .unwrap();
        // 64 + 16 = 80.
        assert_eq!(out, b"\x1b[<80;6;3M".to_vec());
    }

    #[test]
    fn release_in_press_mode_is_dropped() {
        let e = ev(
            MouseEventKind::Up(MouseButton::Left),
            5,
            3,
            KeyModifiers::NONE,
        );
        assert_eq!(
            encode_mouse_report(e, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr, 1),
            None
        );
    }

    #[test]
    fn release_in_press_release_mode_ends_in_lowercase_m() {
        let e = ev(
            MouseEventKind::Up(MouseButton::Left),
            5,
            3,
            KeyModifiers::NONE,
        );
        let out = encode_mouse_report(
            e,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
            1,
        )
        .unwrap();
        assert_eq!(out, b"\x1b[<0;6;3m".to_vec());
        assert_eq!(*out.last().unwrap(), b'm');
    }

    #[test]
    fn left_down_sgr_button_zero() {
        let e = ev(
            MouseEventKind::Down(MouseButton::Left),
            5,
            3,
            KeyModifiers::NONE,
        );
        let out = encode_mouse_report(e, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr, 1)
            .unwrap();
        assert_eq!(out, b"\x1b[<0;6;3M".to_vec());
    }

    #[test]
    fn scroll_up_default_encoding_bytes() {
        let e = ev(MouseEventKind::ScrollUp, 5, 3, KeyModifiers::NONE);
        let out = encode_mouse_report(
            e,
            MouseProtocolMode::Press,
            MouseProtocolEncoding::Default,
            1,
        )
        .unwrap();
        // cb 64, cx 6, cy 3 -> bytes [ESC, '[', 'M', 32+64, 32+6, 32+3].
        assert_eq!(out, vec![0x1b, b'[', b'M', 32 + 64, 32 + 6, 32 + 3]);
    }
}
