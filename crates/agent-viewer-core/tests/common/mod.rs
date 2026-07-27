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

/// Read and parse a JSON fixture relative to the fixtures directory.
pub fn fixture_json(rel: &str) -> serde_json::Value {
    let path = fixture_path(rel);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    serde_json::from_str(&contents)
        .unwrap_or_else(|e| panic!("parse fixture {} as JSON: {e}", path.display()))
}

/// Replay an ordered queue of captured JSON frames in tests.
pub struct FrameReplay {
    frames: std::collections::VecDeque<serde_json::Value>,
}

impl FrameReplay {
    /// Build a replay queue from fixture paths in wire order.
    pub fn from_fixtures(paths: &[&str]) -> Self {
        Self {
            frames: paths.iter().map(|path| fixture_json(path)).collect(),
        }
    }

    /// Pop the next captured frame without correlating it to a request.
    pub fn next_frame(&mut self) -> Option<serde_json::Value> {
        self.frames.pop_front()
    }

    /// Pop a frame and correlate a response ID to the supplied request ID.
    pub fn respond_to(&mut self, request: &serde_json::Value) -> Option<serde_json::Value> {
        let mut frame = self.next_frame()?;
        let request_id = request.get("id").cloned();

        if let (Some(id), Some(object)) = (request_id, frame.as_object_mut())
            && (object.contains_key("result") || object.contains_key("error"))
        {
            object.insert("id".to_string(), id);
        }

        Some(frame)
    }

    /// Report how many captured frames remain in the replay queue.
    pub fn remaining(&self) -> usize {
        self.frames.len()
    }
}

/// Concatenate result data rows from captured pagination responses.
pub fn replay_pages(paths: &[&str]) -> Vec<serde_json::Value> {
    let mut rows = Vec::new();

    for path in paths {
        let fixture = fixture_json(path);
        let page = fixture
            .pointer("/result/data")
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| panic!("fixture {path} has no result.data array"));
        rows.extend(page.iter().cloned());
    }

    rows
}
