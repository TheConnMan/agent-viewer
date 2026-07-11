use crate::error::Result;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: std::path::PathBuf,
    pub originator: String,
    pub cli_version: String,
}

/// Parse ONLY the first line (type "session_meta"; fields under .payload).
/// The first line is large (payload.base_instructions) — read_line, do not cap.
/// Empty file or non-session_meta first line -> Err.
pub fn read_session_meta(path: &std::path::Path) -> Result<SessionMeta> {
    let _ = path;
    todo!()
}

/// Terminal marker check. Read at most the final 64 KiB, split to lines, return true
/// iff any line is an event_msg whose payload.type == "task_complete".
pub fn has_task_complete_tail(path: &std::path::Path) -> Result<bool> {
    let _ = path;
    todo!()
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptItem {
    pub role: String,
    pub text: String,
}

/// Full lazy parse for the detail pane. Keep only response_item lines with
/// payload.role + payload.content[]; concatenate content[].text where
/// content[].type is "input_text" or "output_text". Skip malformed lines silently.
pub fn read_transcript(path: &std::path::Path) -> Result<Vec<TranscriptItem>> {
    let _ = path;
    todo!()
}
