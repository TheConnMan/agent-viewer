use agent_viewer_core::GrokLifecycle;
use std::path::PathBuf;

fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME is required")).join(".grok")
        })
}

#[test]
#[ignore = "requires explicit authorization to delete one exact live Grok session"]
fn live_delete_exact_opt_in() {
    let session_id = std::env::var("GROK_LIVE_DELETE_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .expect("set GROK_LIVE_DELETE_SESSION_ID to the exact disposable session identity");
    let expected_cwd = std::env::var_os("GROK_LIVE_DELETE_EXPECTED_CWD")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .expect("set GROK_LIVE_DELETE_EXPECTED_CWD to the exact durable session cwd");
    let lifecycle = GrokLifecycle::new("grok", grok_home());

    let matching = lifecycle
        .list()
        .expect("list live Grok sessions before exact deletion")
        .into_iter()
        .filter(|session| session.id == session_id)
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "exactly one live Grok session must match the authorized identity"
    );
    assert_eq!(
        matching[0].cwd, expected_cwd,
        "the authorized identity must resolve to the exact expected cwd"
    );

    lifecycle
        .delete(&session_id)
        .expect("delete the exact authorized live Grok session");
    let remaining = lifecycle
        .list()
        .expect("list live Grok sessions after exact deletion");
    assert!(
        !remaining.iter().any(|session| session.id == session_id),
        "the exact authorized live Grok session remained after deletion"
    );
}
