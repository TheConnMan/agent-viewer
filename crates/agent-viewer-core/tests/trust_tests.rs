//! claude::ensure_trusted — pre-accepts the trust dialog for a cwd in the claude config
//! JSON so an attach into a fresh project does not stall on the trust prompt. Tempdir
//! fixtures only; the real ~/.claude.json is never touched.

use agent_viewer_core::claude::ensure_trusted;
use serde_json::{Value, json};
use std::path::Path;
use tempfile::TempDir;

fn write_config(dir: &Path, value: &Value) -> std::path::PathBuf {
    let path = dir.join("claude.json");
    std::fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    path
}

#[test]
fn ensure_trusted_noop_when_cwd_already_trusted() {
    let dir = TempDir::new().unwrap();
    let config = json!({
        "projects": { "/work/proj": { "hasTrustDialogAccepted": true, "history": [] } },
        "numStartups": 5
    });
    let path = write_config(dir.path(), &config);
    let before_bytes = std::fs::read(&path).unwrap();
    let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

    let changed = ensure_trusted(&path, Path::new("/work/proj")).expect("ensure_trusted");
    assert!(!changed); // already trusted -> Ok(false)

    // File is not rewritten: bytes and mtime are unchanged.
    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before_mtime);
}

#[test]
fn ensure_trusted_noop_when_ancestor_trusted() {
    let dir = TempDir::new().unwrap();
    let config = json!({
        "projects": { "/work": { "hasTrustDialogAccepted": true } }
    });
    let path = write_config(dir.path(), &config);
    let before_bytes = std::fs::read(&path).unwrap();

    // A trusted ancestor directory covers a nested cwd.
    let changed = ensure_trusted(&path, Path::new("/work/proj/sub")).expect("ensure_trusted");
    assert!(!changed);
    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
}

#[test]
fn ensure_trusted_merges_and_preserves_all_keys() {
    let dir = TempDir::new().unwrap();
    let config = json!({
        "projects": {
            "/other": { "hasTrustDialogAccepted": true },
            "/work/proj": { "someKey": "keep", "hasTrustDialogAccepted": false }
        },
        "topLevel": "keepme",
        "numStartups": 7
    });
    let path = write_config(dir.path(), &config);

    // "/other" is trusted but is NOT an ancestor of the cwd, and "/work/proj" itself is
    // present but not accepted -> the flag is set (Ok(true)).
    let changed = ensure_trusted(&path, Path::new("/work/proj")).expect("ensure_trusted");
    assert!(changed);

    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    // cwd's project object gains the trust flag while keeping its other keys.
    assert_eq!(after["projects"]["/work/proj"]["hasTrustDialogAccepted"], json!(true));
    assert_eq!(after["projects"]["/work/proj"]["someKey"], json!("keep"));
    // Unrelated project object and every top-level key are preserved verbatim.
    assert_eq!(after["projects"]["/other"]["hasTrustDialogAccepted"], json!(true));
    assert_eq!(after["topLevel"], json!("keepme"));
    assert_eq!(after["numStartups"], json!(7));
}

#[test]
fn ensure_trusted_errs_and_preserves_a_malformed_config() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("claude.json");
    // A real ~/.claude.json holds the account, projects, MCP config; if it is unparseable we
    // must NEVER overwrite it with a fresh minimal object.
    std::fs::write(&path, "{ this is not valid json ").unwrap();
    let before = std::fs::read(&path).unwrap();

    let result = ensure_trusted(&path, Path::new("/work/proj"));
    assert!(result.is_err()); // parse failure -> Err, never a silent wipe
    // The user's (unparseable) config is left byte-for-byte intact.
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn ensure_trusted_creates_missing_config() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("does-not-exist.json");
    assert!(!path.exists());

    let changed = ensure_trusted(&path, Path::new("/work/proj")).expect("ensure_trusted");
    assert!(changed); // created -> Ok(true)
    assert!(path.exists());

    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after["projects"]["/work/proj"]["hasTrustDialogAccepted"], json!(true));
}
