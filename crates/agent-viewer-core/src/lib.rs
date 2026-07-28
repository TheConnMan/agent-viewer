pub mod backend;
pub mod claude;
pub mod codex;
pub mod error;
pub mod group;
#[cfg(target_os = "linux")]
#[path = "opencode.rs"]
mod opencode_impl;
#[cfg(not(target_os = "linux"))]
#[path = "opencode_portable.rs"]
mod opencode_impl;
pub mod opencode {
    pub use crate::backend::opencode_capabilities_for_platform as capabilities_for_platform;
    pub use crate::opencode_impl::*;
}
pub mod platform;
pub mod pr_status;
pub mod pty;
pub mod spawn;
pub mod state;

pub use backend::{
    Backend, BackendKind, Capabilities, PrRef, Session, SessionOrigin, SpawnResult, Status,
    StatusEvent, StatusSink, Subscription,
};
pub use error::{AttachRefusal, Error, Result};
pub use pr_status::PrBadgeColor;

/// Flag any session whose cwd is a non-empty path that no longer exists on disk as a
/// companion, so the default view hides deleted-dir noise (e.g. agentos /tmp sessions).
/// Only ever SETS companion — an already-flagged session and a session with a live or
/// empty cwd are left untouched.
pub fn mark_dead_dirs(sessions: &mut [Session]) {
    for session in sessions.iter_mut() {
        if session.companion {
            continue;
        }
        if session.cwd.as_os_str().is_empty() {
            continue;
        }
        if !session.cwd.exists() {
            session.companion = true;
        }
    }
}

/// Open a SQLite DB read-only with a 500ms busy timeout (Codex and opencode write
/// concurrently). Read-only flags mean the file is never created if missing.
pub fn open_readonly(path: &std::path::Path) -> Result<rusqlite::Connection> {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_millis(500))?;
    Ok(conn)
}

/// The current user's home directory, or an empty path when no supported variable is set.
pub fn home_dir() -> std::path::PathBuf {
    platform::home_from(
        platform::current_platform(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
        std::env::var_os("HOMEDRIVE").as_deref(),
        std::env::var_os("HOMEPATH").as_deref(),
    )
}

/// $CODEX_HOME if set, else $HOME/.codex.
pub fn default_codex_home() -> std::path::PathBuf {
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        return std::path::PathBuf::from(codex_home);
    }
    home_dir().join(".codex")
}

/// value[key] as &str — the ubiquitous JSON string-field accessor.
pub(crate) fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

/// One JSONL line -> Value: trim, then None for blank or malformed lines (all the
/// line-oriented parsers skip those silently).
pub(crate) fn parse_json_line(line: &str) -> Option<serde_json::Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// Parse the UTC RFC 3339 shape written by Codex and Claude into epoch milliseconds.
/// Fractional seconds are truncated to milliseconds. Other shapes are skipped.
pub(crate) fn rfc3339_millis(timestamp: &str) -> Option<i64> {
    let body = timestamp.strip_suffix('Z')?;
    if body.len() < 19
        || body.as_bytes().get(4) != Some(&b'-')
        || body.as_bytes().get(7) != Some(&b'-')
        || body.as_bytes().get(10) != Some(&b'T')
        || body.as_bytes().get(13) != Some(&b':')
        || body.as_bytes().get(16) != Some(&b':')
    {
        return None;
    }
    let year = body.get(0..4)?.parse::<i64>().ok()?;
    let month = body.get(5..7)?.parse::<i64>().ok()?;
    let day = body.get(8..10)?.parse::<i64>().ok()?;
    let hour = body.get(11..13)?.parse::<i64>().ok()?;
    let minute = body.get(14..16)?.parse::<i64>().ok()?;
    let second = body.get(17..19)?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day < 1 || day > month_days[(month - 1) as usize] {
        return None;
    }
    let fraction = body.get(19..)?;
    let millis = if fraction.is_empty() {
        0
    } else {
        let digits = fraction.strip_prefix('.')?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let mut value = 0_i64;
        for byte in digits.bytes().take(3) {
            value = value * 10 + i64::from(byte - b'0');
        }
        value * 10_i64.pow(3_u32.saturating_sub(digits.len().min(3) as u32))
    };

    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?;
    seconds.checked_mul(1_000)?.checked_add(millis)
}

pub(crate) fn activity_window(window: std::time::Duration) -> (i64, i64) {
    let now = crate::spawn::now_ms();
    let width = i64::try_from(window.as_millis()).unwrap_or(i64::MAX);
    (now.saturating_sub(width), now)
}
