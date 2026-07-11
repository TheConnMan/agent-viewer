mod common;

use agent_viewer_core::codex::registry::{Registry, find_state_db};
use agent_viewer_core::codex::source::Source;
use agent_viewer_core::error::Error;
use std::path::PathBuf;

const INSERT_COLS: &str = "INSERT INTO threads \
    (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, \
     sandbox_policy, approval_mode, archived, model, git_branch, first_user_message, \
     preview, created_at_ms, updated_at_ms) VALUES ";

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
fn threads_maps_rows_and_orders_by_recency() {
    let schema = common::read_fixture("threads_schema.sql");
    // active exec, recency 3000
    let s_exec = format!(
        "{INSERT_COLS}\
         ('t_exec','/home/user/proj/sessions/r1.jsonl',1,3,'exec','openai',\
          '/home/user/proj','Exec Title','workspace-write','on-request',0,\
          'gpt-5','main','first exec msg','preview exec',1000,3000)"
    );
    // archived cli, recency 2000
    let s_cli = format!(
        "{INSERT_COLS}\
         ('t_cli','/home/user/proj/archived_sessions/r2.jsonl',1,2,'cli','openai',\
          '/home/user/proj','Cli Title','workspace-write','on-request',1,\
          NULL,NULL,'first cli msg','preview cli',1000,2000)"
    );
    // vscode row with NULL updated_at_ms -> COALESCE(updated_at*1000)=1000
    let s_vscode = format!(
        "{INSERT_COLS}\
         ('t_vscode','/home/user/proj/sessions/r3.jsonl',1,1,'vscode','openai',\
          '/home/user/proj','Vscode Title','workspace-write','on-request',0,\
          NULL,NULL,'first vscode msg','preview vscode',NULL,NULL)"
    );
    let inserts = [s_exec.as_str(), s_cli.as_str(), s_vscode.as_str()];
    let (_dir, path) = common::temp_db(&schema, &inserts);

    let reg = Registry::open(&path).expect("open read-only");
    let threads = reg.threads().expect("query threads");
    assert_eq!(threads.len(), 3);

    // recency DESC: t_exec (3000), t_cli (2000), t_vscode (1000 via COALESCE)
    assert_eq!(threads[0].id, "t_exec");
    assert_eq!(threads[1].id, "t_cli");
    assert_eq!(threads[2].id, "t_vscode");

    let exec = &threads[0];
    assert_eq!(
        exec.rollout_path,
        PathBuf::from("/home/user/proj/sessions/r1.jsonl")
    );
    assert_eq!(exec.source, Source::Exec);
    assert_eq!(exec.cwd, PathBuf::from("/home/user/proj"));
    assert_eq!(exec.title, "Exec Title");
    assert!(!exec.archived);
    assert_eq!(exec.model.as_deref(), Some("gpt-5"));
    assert_eq!(exec.git_branch.as_deref(), Some("main"));
    assert_eq!(exec.first_user_message, "first exec msg");
    assert_eq!(exec.preview, "preview exec");
    assert_eq!(exec.created_at_ms, 1000);
    assert_eq!(exec.updated_at_ms, 3000);

    let cli = &threads[1];
    assert_eq!(cli.source, Source::Cli);
    assert!(cli.archived);
    assert_eq!(cli.model, None);
    assert_eq!(cli.git_branch, None);

    let vscode = &threads[2];
    assert_eq!(vscode.source, Source::VsCode);
    assert!(!vscode.archived);
    // NULL *_ms columns fall back through COALESCE to *_at * 1000.
    assert_eq!(vscode.updated_at_ms, 1000);
    assert_eq!(vscode.created_at_ms, 1000);
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
