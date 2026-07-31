mod common;

use agent_viewer_core::Backend;
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::codex::registry::{Registry, find_state_db};
use agent_viewer_core::codex::source::Source;
use agent_viewer_core::error::Error;
use std::path::PathBuf;

const INSERT_COLS: &str = "INSERT INTO threads \
    (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, \
     sandbox_policy, approval_mode, archived, model, git_branch, first_user_message, \
     preview, created_at_ms, updated_at_ms) VALUES ";

fn thread_insert(id: &str, title: &str, updated_at_ms: i64) -> String {
    format!(
        "{INSERT_COLS}\
         ('{id}','/home/user/proj/sessions/{id}.jsonl',1,1,'cli','openai',\
          '/home/user/proj','{title}','workspace-write','on-request',0,\
          'gpt-5',NULL,'first message','preview',1000,{updated_at_ms})"
    )
}

fn copy_session_index(dir: &tempfile::TempDir) {
    std::fs::copy(
        common::fixture_path("codex_session_index_names.jsonl"),
        dir.path().join("session_index.jsonl"),
    )
    .expect("copy session index fixture");
}

#[test]
fn find_state_db_picks_highest_numeric() {
    let dir = tempfile::TempDir::new().unwrap();
    for n in ["state_3.sqlite", "state_5.sqlite", "state_10.sqlite"] {
        std::fs::write(dir.path().join(n), b"").unwrap();
    }
    // Numeric compare must beat lexicographic ("state_5" > "state_10" lexically).
    let got = find_state_db(dir.path()).expect("find highest");
    assert_eq!(got, dir.path().join("state_10.sqlite"));
}

#[test]
fn find_state_db_errors_when_none() {
    let dir = tempfile::TempDir::new().unwrap();
    match find_state_db(dir.path()) {
        Err(Error::NoStateDb(p)) => assert_eq!(p, dir.path().to_path_buf()),
        other => panic!("expected NoStateDb, got {other:?}"),
    }
}

#[test]
fn threads_maps_rows_and_orders_by_recency_ascending() {
    let schema = common::read_fixture("threads_schema.sql");
    // Creation order is the reverse of update order, so this proves update time is the key.
    let s_exec = format!(
        "{INSERT_COLS}\
         ('t_exec','/home/user/proj/sessions/r1.jsonl',1,3,'exec','openai',\
          '/home/user/proj','Exec Title','workspace-write','on-request',0,\
          'gpt-5','main','first exec msg','preview exec',1000,3000)"
    );
    let s_cli = format!(
        "{INSERT_COLS}\
         ('t_cli','/home/user/proj/archived_sessions/r2.jsonl',1,2,'cli','openai',\
          '/home/user/proj','Cli Title','workspace-write','on-request',1,\
          NULL,NULL,'first cli msg','preview cli',4000,2000)"
    );
    let s_vscode = format!(
        "{INSERT_COLS}\
         ('t_vscode','/home/user/proj/sessions/r3.jsonl',5,1,'vscode','openai',\
          '/home/user/proj','Vscode Title','workspace-write','on-request',0,\
          NULL,NULL,'first vscode msg','preview vscode',NULL,NULL)"
    );
    let s_tie_z = format!(
        "{INSERT_COLS}\
         ('t_tie_z','/home/user/proj/sessions/r4.jsonl',1,2,'cli','openai',\
          '/home/user/proj','Tie Z','workspace-write','on-request',0,\
          NULL,NULL,'first tie z msg','preview tie z',2000,2500)"
    );
    let s_tie_a = format!(
        "{INSERT_COLS}\
         ('t_tie_a','/home/user/proj/sessions/r5.jsonl',1,2,'cli','openai',\
          '/home/user/proj','Tie A','workspace-write','on-request',0,\
          NULL,NULL,'first tie a msg','preview tie a',3000,2500)"
    );
    let inserts = [
        s_exec.as_str(),
        s_cli.as_str(),
        s_vscode.as_str(),
        s_tie_z.as_str(),
        s_tie_a.as_str(),
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let reg = Registry::open(&path).expect("open read-only");
    let threads = reg.threads().expect("query threads");
    assert_eq!(threads.len(), 5);

    assert_eq!(
        threads
            .iter()
            .map(|thread| thread.id.as_str())
            .collect::<Vec<_>>(),
        vec!["t_vscode", "t_cli", "t_tie_z", "t_tie_a", "t_exec"]
    );

    let exec = threads.iter().find(|thread| thread.id == "t_exec").unwrap();
    assert_eq!(
        exec.rollout_path,
        PathBuf::from("/home/user/proj/sessions/r1.jsonl")
    );
    assert_eq!(exec.source, Source::Exec);
    assert_eq!(exec.cwd, PathBuf::from("/home/user/proj"));
    assert_eq!(exec.git_branch.as_deref(), Some("main"));
    assert_eq!(exec.title, "Exec Title");
    assert!(!exec.archived);
    assert_eq!(exec.preview, "preview exec");
    assert_eq!(exec.created_at_ms, 1000);
    assert_eq!(exec.updated_at_ms, 3000);

    let cli = threads.iter().find(|thread| thread.id == "t_cli").unwrap();
    assert_eq!(cli.source, Source::Cli);
    assert!(cli.archived);

    let vscode = threads
        .iter()
        .find(|thread| thread.id == "t_vscode")
        .unwrap();
    assert_eq!(vscode.source, Source::VsCode);
    assert!(!vscode.archived);
    // NULL *_ms columns fall back through COALESCE to *_at * 1000.
    assert_eq!(vscode.updated_at_ms, 1000);
    assert_eq!(vscode.created_at_ms, 5000);
}

#[test]
fn threads_extract_only_valid_spawn_parent_ids() {
    let schema = common::read_fixture("threads_schema.sql");
    let nested = format!(
        "{INSERT_COLS}\
         ('nested','/r/nested.jsonl',1,1,\
          '{{\"subagent\":{{\"thread_spawn\":{{\"parent_thread_id\":\"root\",\
          \"agent_nickname\":\"Worker\"}}}}}}','openai','/p','Nested',\
          'workspace-write','on-request',0,NULL,NULL,'msg','preview',1000,4000)"
    );
    let plain = format!(
        "{INSERT_COLS}\
         ('plain','/r/plain.jsonl',1,1,'cli','openai','/p','Plain',\
          'workspace-write','on-request',0,NULL,NULL,'msg','preview',1000,3000)"
    );
    let wrong_type = format!(
        "{INSERT_COLS}\
         ('wrong_type','/r/wrong_type.jsonl',1,1,\
          '{{\"subagent\":{{\"thread_spawn\":{{\"parent_thread_id\":7}}}}}}',\
          'openai','/p','Wrong Type','workspace-write','on-request',0,NULL,NULL,\
          'msg','preview',1000,2000)"
    );
    let malformed = format!(
        "{INSERT_COLS}\
         ('malformed','/r/malformed.jsonl',1,1,\
          '{{\"subagent\":{{\"thread_spawn\":{{\"parent_thread_id\":\"root\"',\
          'openai','/p','Malformed','workspace-write','on-request',0,NULL,NULL,\
          'msg','preview',1000,1000)"
    );
    let inserts = [
        nested.as_str(),
        plain.as_str(),
        wrong_type.as_str(),
        malformed.as_str(),
    ];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let threads = Registry::open(&path)
        .expect("open read only")
        .threads()
        .expect("query threads");

    let parent_for = |id: &str| {
        threads
            .iter()
            .find(|thread| thread.id == id)
            .expect("fixture thread")
            .parent_thread_id
            .as_deref()
    };
    assert_eq!(parent_for("nested"), Some("root"));
    assert_eq!(parent_for("plain"), None);
    assert_eq!(parent_for("wrong_type"), None);
    assert_eq!(parent_for("malformed"), None);
}

#[test]
fn threads_overlay_uses_the_latest_valid_duplicate_name() {
    let schema = common::read_fixture("threads_schema.sql");
    let insert = thread_insert("fixture_thread_duplicate", "SQLite Original Name", 3000);
    let (dir, path) = common::temp_db(&schema, &[insert.as_str()]);
    copy_session_index(&dir);

    let threads = Registry::open(&path)
        .expect("open read only")
        .threads()
        .expect("query threads");

    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, "fixture_thread_duplicate");
    assert_eq!(
        threads[0].title, "Latest Index Name",
        "later empty and nonstring duplicate names must not erase the last valid index name"
    );
}

#[test]
fn threads_overlay_isolates_a_malformed_line_from_valid_neighbors() {
    let schema = common::read_fixture("threads_schema.sql");
    let ordinary = thread_insert("fixture_thread_ordinary", "Ordinary SQLite Name", 2000);
    let duplicate = thread_insert("fixture_thread_duplicate", "Duplicate SQLite Name", 3000);
    let (dir, path) = common::temp_db(&schema, &[ordinary.as_str(), duplicate.as_str()]);
    copy_session_index(&dir);

    let threads = Registry::open(&path)
        .expect("open read only")
        .threads()
        .expect("query threads");
    let ordinary = threads
        .iter()
        .find(|thread| thread.id == "fixture_thread_ordinary")
        .expect("ordinary fixture thread");
    let duplicate = threads
        .iter()
        .find(|thread| thread.id == "fixture_thread_duplicate")
        .expect("duplicate fixture thread");

    assert_eq!(
        ordinary.title, "Ordinary Index Name",
        "a valid entry before the malformed line must survive"
    );
    assert_eq!(
        duplicate.title, "Latest Index Name",
        "a valid entry after the malformed line must still be read"
    );
}

#[test]
fn threads_overlay_ignores_names_for_unknown_ids() {
    let schema = common::read_fixture("threads_schema.sql");
    let insert = thread_insert("fixture_thread_sqlite_only", "SQLite Only Name", 3000);
    let (dir, path) = common::temp_db(&schema, &[insert.as_str()]);
    copy_session_index(&dir);

    let threads = Registry::open(&path)
        .expect("open read only")
        .threads()
        .expect("query threads");

    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, "fixture_thread_sqlite_only");
    assert_eq!(
        threads[0].title, "SQLite Only Name",
        "an index name for another id must not affect this row"
    );
}

#[test]
fn threads_without_a_session_index_keep_the_sqlite_title() {
    let schema = common::read_fixture("threads_schema.sql");
    let insert = thread_insert("fixture_thread_duplicate", "SQLite Fallback Name", 3000);
    let (_dir, path) = common::temp_db(&schema, &[insert.as_str()]);

    let threads = Registry::open(&path)
        .expect("open read only")
        .threads()
        .expect("query threads without an index");

    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, "fixture_thread_duplicate");
    assert_eq!(threads[0].title, "SQLite Fallback Name");
}

/// The overlay is parsed once and held under the index file's (mtime, len), so the whole file
/// is not re-read on every listing tick. A rename must still land on the very next listing.
/// Every step here changes the file's LENGTH as well as its mtime, so the test cannot pass by
/// accident on filesystem timestamp granularity.
#[test]
fn threads_overlay_follows_a_rewritten_session_index() {
    let schema = common::read_fixture("threads_schema.sql");
    let insert = thread_insert("fixture_thread_duplicate", "SQLite Fallback Name", 3000);
    let (dir, path) = common::temp_db(&schema, &[insert.as_str()]);
    let reg = Registry::open(&path).expect("open read only");
    let title = |threads: Vec<agent_viewer_core::codex::registry::Thread>| threads[0].title.clone();

    assert_eq!(
        title(reg.threads().expect("query without an index")),
        "SQLite Fallback Name"
    );

    copy_session_index(&dir);
    assert_eq!(
        title(reg.threads().expect("query after the index appeared")),
        "Latest Index Name",
        "an index file that appears after the first listing must still be read"
    );

    std::fs::write(
        dir.path().join("session_index.jsonl"),
        b"{\"id\":\"fixture_thread_duplicate\",\"thread_name\":\"Renamed\"}\n",
    )
    .expect("rewrite session index");
    assert_eq!(
        title(reg.threads().expect("query after the rename")),
        "Renamed",
        "a rename rewrites session_index.jsonl, and the cached overlay must not survive it"
    );
}

#[test]
fn distinct_models_orders_by_frequency_and_drops_empty_null() {
    let schema = common::read_fixture("threads_schema.sql");
    // gpt-5 x3, gpt-5-codex x2, o3 x1, plus one empty-string and one NULL model that
    // must be dropped entirely.
    let mut inserts = Vec::new();
    let rows = [
        ("m1", "gpt-5"),
        ("m2", "gpt-5"),
        ("m3", "gpt-5"),
        ("m4", "gpt-5-codex"),
        ("m5", "gpt-5-codex"),
        ("m6", "o3"),
        ("m7", "''"), // empty-string model literal
        ("m8", "NULL"),
    ];
    for (id, model) in rows {
        let model_literal = if model == "NULL" || model == "''" {
            model.to_string()
        } else {
            format!("'{model}'")
        };
        inserts.push(format!(
            "{INSERT_COLS}\
             ('{id}','/r/{id}.jsonl',1,1,'cli','openai','/p','T',\
              'workspace-write','on-request',0,{model_literal},NULL,'msg','preview',1000,1000)"
        ));
    }
    let insert_refs: Vec<&str> = inserts.iter().map(String::as_str).collect();
    let (_dir, path) = common::temp_db(&schema, &insert_refs);

    let reg = Registry::open(&path).expect("open read-only");
    let models = reg.distinct_models().expect("distinct models");
    // Most-used first; empty and NULL never appear.
    assert_eq!(models, vec!["gpt-5", "gpt-5-codex", "o3"]);
    assert!(!models.iter().any(|m| m.is_empty()));
}

#[test]
fn open_missing_db_errors_and_creates_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("nope.sqlite");
    assert!(Registry::open(&path).is_err());
    // Read-only open must NOT create the file.
    assert!(!path.exists());
}

/// Build one `state_N.sqlite` at an exact path (the shared helper picks its own filename, and
/// the rollover test below needs both versions side by side in one codex home).
fn write_state_db(path: &std::path::Path, schema: &str, inserts: &[String]) {
    let conn = rusqlite::Connection::open(path).expect("open state db");
    conn.execute_batch(schema).expect("run schema DDL");
    for stmt in inserts {
        conn.execute(stmt, []).expect("run insert");
    }
    conn.close().expect("close writer connection");
}

/// A Codex upgrade lays down a NEW `state_N+1.sqlite` and leaves the old one in place, readable
/// and frozen. The listing has to follow the rollover while running: the registry used to be
/// re-resolved only when a read ERRORED, and the stale file never errors, so every session
/// created after the upgrade stayed invisible until the viewer was restarted.
#[test]
fn list_follows_a_state_db_rollover_without_a_restart() {
    let schema = common::read_fixture("threads_schema.sql");
    let dir = tempfile::TempDir::new().unwrap();
    write_state_db(
        &dir.path().join("state_1.sqlite"),
        &schema,
        &[thread_insert("t_before", "Before", 1000)],
    );

    let mut backend = CodexBackend::new(dir.path().to_path_buf());
    let ids = |sessions: Vec<agent_viewer_core::Session>| {
        sessions
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(backend.list().expect("first listing")),
        vec!["t_before"]
    );

    write_state_db(
        &dir.path().join("state_2.sqlite"),
        &schema,
        &[
            thread_insert("t_before", "Before", 1000),
            thread_insert("t_after", "After", 2000),
        ],
    );
    let after = ids(backend.list().expect("listing after the rollover"));
    assert!(
        after.contains(&"t_after".to_string()),
        "a session created in state_2 must appear without a restart, got {after:?}"
    );
}
