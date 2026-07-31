pub mod backend;
pub mod claude;
pub mod codex;
pub mod error;
pub mod group;
pub mod platform;
pub mod pr_status;
pub mod pty;
pub mod router;
pub mod spawn;
pub mod state;

pub use backend::{
    Backend, BackendKind, Capabilities, ListingCacheScope, PrRef, Session, SessionOrigin,
    SpawnResult, Status, TailEvent,
};
pub use error::{AttachRefusal, Error, Result};
pub use pr_status::PrBadgeColor;
pub use state::{
    ListingCacheClaim, ListingCacheLease, ListingCacheRead, ListingCacheSnapshot,
    ListingCacheWrite, ViewerDb,
};

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

/// Open a SQLite DB read-only with a 500ms busy timeout (Codex writes
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

/// Bytes a tail-pane read may touch at the end of a JSONL transcript.
///
/// Transcripts grow without bound while a session works (18.6 MB measured on this box), and the
/// tail pane refreshes on every 2s tick, so reading whole files re-parsed megabytes per tick to
/// display twelve events. This window is deliberately far wider than the 64 KiB the status tail
/// classifies over: a single transcript line can be an `apply_patch` call carrying a whole diff,
/// and a window that lands inside one such line would show an empty pane.
pub(crate) const TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;

/// Read at most the final `window` bytes of `path` as lossy UTF-8.
///
/// The leading line is usually a fragment; every caller feeds the result to `parse_json_line`,
/// which drops it silently along with any other malformed line.
///
/// The READER is bounded, not just the seek. `read_to_end` from the seek offset keeps reading
/// past the length that was sampled a moment earlier, so against a rollout a live session is
/// still appending to it returns the window PLUS everything written during the read - a
/// listing/tail worker that stays busy for as long as the writer keeps going, on a 2s tick.
/// `take` caps the read at the window no matter how fast the file grows.
pub(crate) fn read_tail_window(path: &std::path::Path, window: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(window)))?;
    let mut buf = Vec::with_capacity(usize::try_from(len.min(window)).unwrap_or(0));
    file.take(window).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
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

#[cfg(test)]
mod tail_window_tests {
    use super::read_tail_window;
    use std::io::Write;

    /// The flat post-condition: a file far larger than the window yields at most the window.
    #[test]
    fn read_tail_window_never_returns_more_than_the_window() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("static.log");
        std::fs::write(&path, "a".repeat(64 * 1024)).expect("write file");

        let text = read_tail_window(&path, 4_096).expect("read tail");

        assert_eq!(text.len(), 4_096, "the window is both the cap and the read");
    }

    /// A file SHORTER than the window still returns all of it (the cap must not become a floor).
    #[test]
    fn read_tail_window_returns_a_short_file_whole() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("short.log");
        std::fs::write(&path, "tiny").expect("write file");

        assert_eq!(read_tail_window(&path, 4_096).expect("read tail"), "tiny");
    }

    /// THE REGRESSION. The read used to be `read_to_end` from the seek offset, which follows
    /// bytes appended after the length was sampled: against a file a live session is still
    /// writing, one tail read returns the window plus everything the writer produced during
    /// it. The assertion is absolute (never more than the window), so it cannot fail on a
    /// bounded reader no matter how the threads interleave - only an unbounded one trips it.
    #[test]
    fn read_tail_window_stays_bounded_while_the_file_is_being_appended_to() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("growing.log");
        std::fs::write(&path, "s".repeat(128 * 1024)).expect("seed file");

        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .expect("open for append");
            let chunk = "w".repeat(64 * 1024);
            for _ in 0..512 {
                file.write_all(chunk.as_bytes()).expect("append chunk");
            }
        });

        let mut reads = 0_u32;
        while !writer.is_finished() {
            let text = read_tail_window(&path, 4_096).expect("read tail");
            assert!(
                text.len() <= 4_096,
                "tail read ran past its window: {} bytes",
                text.len()
            );
            reads += 1;
        }
        writer.join().expect("writer thread");

        assert!(reads > 0, "the reader never raced the writer");
    }
}
