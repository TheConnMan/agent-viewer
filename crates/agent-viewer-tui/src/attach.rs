//! Key-to-bytes encoding + attached-mode glue. Stream C owns this module.

/// crossterm KeyEvent -> raw bytes for the pty. Pinned table (section 5.11): chars as
/// UTF-8 (CTRL+c -> 0x01..0x1A by letter, ALT -> ESC prefix), Enter "\r", Tab "\t",
/// Backspace 0x7f, Esc 0x1b, arrows/Home/End/PgUp/PgDn/Delete/Insert/F1-F12 per the
/// table. Unmapped keys -> None. Ctrl+q NEVER reaches this (detach upstream).
pub fn key_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    let _ = key;
    todo!("Stream C: crossterm KeyEvent -> pty byte encoding per the pinned table")
}
