#![allow(dead_code)]
//! Shared test helpers: fixture location + real temp SQLite construction.
//! No mocking — temp DBs are built with plain rusqlite (writing a throwaway fixture DB
//! is allowed; only the tools' real DBs are read-only).

use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub fn rfc3339_at(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let seconds = unix_seconds.rem_euclid(86_400);
    let shifted_days = days + 719_468;
    let era = shifted_days / 146_097;
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = month_position + if month_position < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = seconds / 3_600;
    let minute = seconds % 3_600 / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.000Z")
}

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

pub fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

/// Create a temp SQLite DB, run the schema DDL, then execute each insert statement.
/// The writing connection is closed before returning so a read-only opener sees a
/// stable file. The returned TempDir must be kept alive by the caller.
pub fn temp_db(schema_sql: &str, inserts: &[&str]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create tempdir");
    let path = dir.path().join("test.sqlite");
    let conn = rusqlite::Connection::open(&path).expect("open temp db");
    conn.execute_batch(schema_sql).expect("run schema DDL");
    for stmt in inserts {
        conn.execute(stmt, []).expect("run insert");
    }
    conn.close().expect("close writer connection");
    (dir, path)
}

/// Copy a fixture file into a fresh temp dir and return the writable copy's path.
/// Used by tail tests that append lines to change (mtime, len).
pub fn copy_fixture_to_temp(name: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create tempdir");
    let dest = dir.path().join(name);
    std::fs::copy(fixture_path(name), &dest).expect("copy fixture");
    (dir, dest)
}

/// Display text carrying ESC, CSI leftovers, an RLO bidi override, and ZWSP.
/// A sanitizer that is the union of the TUI title set and Grok's C0/C1+bidi set
/// must drop the controls and keep the printable remainder.
pub fn hostile_display_text() -> &'static str {
    "safe\u{1b}[31m \u{202e}bidi\u{200b}zwsp"
}

pub fn sanitized_display_text() -> &'static str {
    "safe[31m bidizwsp"
}

/// Read and parse a JSON fixture relative to the fixtures directory.
pub fn fixture_json(rel: &str) -> serde_json::Value {
    let path = fixture_path(rel);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    serde_json::from_str(&contents)
        .unwrap_or_else(|e| panic!("parse fixture {} as JSON: {e}", path.display()))
}
