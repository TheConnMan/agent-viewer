#![allow(dead_code)]
//! Shared test helpers: fixture location + real temp SQLite construction.
//! No mocking — temp DBs are built with plain rusqlite (writing a throwaway fixture DB
//! is allowed; only the tools' real DBs are read-only).

use std::path::{Path, PathBuf};
use tempfile::TempDir;

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
