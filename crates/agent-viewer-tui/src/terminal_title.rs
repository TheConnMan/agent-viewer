use std::io::Write;
use std::path::Path;

use crossterm::execute;
use crossterm::terminal::SetTitle;

/// Format the terminal tab title from a sanitized launch directory name.
pub fn format_terminal_title(workspace: &Path) -> String {
    let basename = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.chars()
                .filter(|character| !character.is_control())
                .collect::<String>()
        })
        .filter(|name| !name.is_empty());

    basename
        .map(|name| format!("Agent Viewer · {name}"))
        .unwrap_or_else(|| "Agent Viewer".to_string())
}

/// Emit the terminal tab title without surfacing unsupported writer errors.
pub fn set_terminal_title<W: Write>(writer: &mut W, workspace: &Path) {
    let _ = execute!(writer, SetTitle(format_terminal_title(workspace)));
}
