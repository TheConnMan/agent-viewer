use codex_viewer_core::{BackendKind, Session, Status};
use codex_viewer_tui::app::{App, Row};
use std::path::PathBuf;

/// Synthetic session with a nonexistent cwd so project_root falls back to cwd
/// (no filesystem, no subprocess).
fn sess(backend: BackendKind, id: &str, title: &str, cwd: &str, updated_at_ms: i64, hidden: bool) -> Session {
    Session {
        backend,
        id: id.to_string(),
        title: title.to_string(),
        cwd: PathBuf::from(cwd),
        created_at_ms: updated_at_ms,
        updated_at_ms,
        status: Status::Done,
        hidden,
        source_label: "test".to_string(),
        rollout_path: None,
    }
}

fn session_rows(rows: &[Row]) -> Vec<&Row> {
    rows.iter().filter(|r| matches!(r, Row::Session { .. })).collect()
}

fn headers(rows: &[Row]) -> Vec<&Row> {
    rows.iter().filter(|r| matches!(r, Row::GroupHeader { .. })).collect()
}

#[test]
fn toggle_hidden_includes_hidden_rows() {
    let sessions = vec![
        sess(BackendKind::Codex, "vis", "Visible", "/synthetic/p", 200, false),
        sess(BackendKind::Codex, "hid", "Hidden", "/synthetic/p", 100, true),
    ];
    let mut app = App::new(sessions);

    // Default: hidden session excluded.
    let rows = app.visible();
    assert!(session_rows(&rows).iter().all(|r| !matches!(r, Row::Session { hidden: true, .. })));
    assert_eq!(session_rows(&rows).len(), 1);

    // After toggle: the hidden session is present with hidden == true.
    app.toggle_show_hidden();
    let rows = app.visible();
    assert!(rows.iter().any(|r| matches!(r, Row::Session { hidden: true, id, .. } if id == "hid")));
    assert_eq!(session_rows(&rows).len(), 2);
}

#[test]
fn filter_matches_title_and_cwd_case_insensitive() {
    let sessions = vec![
        sess(BackendKind::Codex, "alpha", "Alpha Task", "/synthetic/apples", 300, false),
        sess(BackendKind::Codex, "beta", "Beta Task", "/synthetic/bananas", 200, false),
    ];
    let mut app = App::new(sessions);
    assert_eq!(session_rows(&app.visible()).len(), 2);

    // Case-insensitive title match.
    app.set_filter("alpha".to_string());
    let rows = app.visible();
    let sr = session_rows(&rows);
    assert_eq!(sr.len(), 1);
    assert!(matches!(sr[0], Row::Session { id, .. } if id == "alpha"));

    // Case-insensitive cwd match.
    app.set_filter("BANANAS".to_string());
    let rows = app.visible();
    let sr = session_rows(&rows);
    assert_eq!(sr.len(), 1);
    assert!(matches!(sr[0], Row::Session { id, .. } if id == "beta"));

    // Clearing restores.
    app.set_filter(String::new());
    assert_eq!(session_rows(&app.visible()).len(), 2);
}

#[test]
fn collapse_emits_header_only_with_count() {
    // Group A (newest, 2 sessions) selected by default; group B (1 session) unaffected.
    let sessions = vec![
        sess(BackendKind::Codex, "a1", "A One", "/synthetic/a", 400, false),
        sess(BackendKind::Codex, "a2", "A Two", "/synthetic/a", 300, false),
        sess(BackendKind::Codex, "b1", "B One", "/synthetic/b", 200, false),
    ];
    let mut app = App::new(sessions);
    app.toggle_collapse_selected();
    let rows = app.visible();

    // Group A collapsed: a header with collapsed=true, count=2, and no A session rows.
    assert!(rows.iter().any(|r| matches!(
        r,
        Row::GroupHeader { collapsed: true, count: 2, root } if root == &PathBuf::from("/synthetic/a")
    )));
    assert!(!rows.iter().any(|r| matches!(r, Row::Session { id, .. } if id == "a1" || id == "a2")));

    // Group B still shows its session.
    assert!(rows.iter().any(|r| matches!(r, Row::Session { id, .. } if id == "b1")));
}

#[test]
fn rows_carry_backend_kinds() {
    let sessions = vec![
        sess(BackendKind::Codex, "cx", "Codex Sess", "/synthetic/cx", 300, false),
        sess(BackendKind::Claude, "cl", "Claude Sess", "/synthetic/cl", 200, false),
        sess(BackendKind::Opencode, "oc", "Opencode Sess", "/synthetic/oc", 100, false),
    ];
    let app = App::new(sessions);
    let rows = app.visible();
    let backend_of = |id: &str| {
        rows.iter().find_map(|r| match r {
            Row::Session { id: rid, backend, .. } if rid == id => Some(*backend),
            _ => None,
        })
    };
    assert_eq!(backend_of("cx"), Some(BackendKind::Codex));
    assert_eq!(backend_of("cl"), Some(BackendKind::Claude));
    assert_eq!(backend_of("oc"), Some(BackendKind::Opencode));
}

#[test]
fn groups_merge_across_backends() {
    // A codex and an opencode session sharing the same cwd land in ONE group.
    let sessions = vec![
        sess(BackendKind::Codex, "cx", "Codex Sess", "/synthetic/shared", 300, false),
        sess(BackendKind::Opencode, "oc", "Opencode Sess", "/synthetic/shared", 200, false),
    ];
    let app = App::new(sessions);
    let rows = app.visible();
    assert_eq!(headers(&rows).len(), 1, "shared cwd must be one group");
    assert_eq!(session_rows(&rows).len(), 2);
}
