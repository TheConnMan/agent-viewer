//! Key-to-bytes encoding for the embedded-attach view. Stream C owns this module.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const ESC: u8 = 0x1b;

/// crossterm KeyEvent -> raw bytes for the pty. Pinned table (section 5.11): chars as
/// UTF-8 (CTRL+letter -> 0x01..0x1A, ALT -> ESC prefix), Enter "\r", Tab "\t",
/// Backspace 0x7f, Esc 0x1b, arrows/Home/End/PgUp/PgDn/Delete/Insert/F1-F12 per the
/// table. Unmapped keys -> None. Ctrl+q NEVER reaches this (detach upstream).
pub fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let bytes: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl+letter -> control code 0x01..0x1A; other ctrl combos unmapped.
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    vec![(lower as u8) - b'a' + 1]
                } else {
                    return None;
                }
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![ESC, b'[', b'Z'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![ESC],
        KeyCode::Up => vec![ESC, b'[', b'A'],
        KeyCode::Down => vec![ESC, b'[', b'B'],
        KeyCode::Right => vec![ESC, b'[', b'C'],
        KeyCode::Left => vec![ESC, b'[', b'D'],
        KeyCode::Home => vec![ESC, b'[', b'H'],
        KeyCode::End => vec![ESC, b'[', b'F'],
        KeyCode::PageUp => vec![ESC, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![ESC, b'[', b'6', b'~'],
        KeyCode::Delete => vec![ESC, b'[', b'3', b'~'],
        KeyCode::Insert => vec![ESC, b'[', b'2', b'~'],
        KeyCode::F(n) => match n {
            1 => vec![ESC, b'O', b'P'],
            2 => vec![ESC, b'O', b'Q'],
            3 => vec![ESC, b'O', b'R'],
            4 => vec![ESC, b'O', b'S'],
            5 => vec![ESC, b'[', b'1', b'5', b'~'],
            6 => vec![ESC, b'[', b'1', b'7', b'~'],
            7 => vec![ESC, b'[', b'1', b'8', b'~'],
            8 => vec![ESC, b'[', b'1', b'9', b'~'],
            9 => vec![ESC, b'[', b'2', b'0', b'~'],
            10 => vec![ESC, b'[', b'2', b'1', b'~'],
            11 => vec![ESC, b'[', b'2', b'3', b'~'],
            12 => vec![ESC, b'[', b'2', b'4', b'~'],
            _ => return None,
        },
        _ => return None,
    };

    // ALT prefixes the sequence with ESC (meta). Ctrl+letter already collapsed to a
    // control byte; an Alt+Ctrl+letter still gets the ESC prefix, matching xterm.
    if alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(ESC);
        out.extend_from_slice(&bytes);
        Some(out)
    } else {
        Some(bytes)
    }
}
