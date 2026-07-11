use agent_viewer_core::{BackendKind, Session, Status};
use agent_viewer_tui::app::{App, GroupMode, KillStage, Row, Section, format_elapsed};
use std::path::PathBuf;

/// Synthetic session with a nonexistent cwd so project_root falls back to cwd
/// (no filesystem, no subprocess).
fn sess(backend: BackendKind, id: &str, cwd: &str, updated_at_ms: i64, status: Status) -> Session {
    Session {
        backend,
        id: id.to_string(),
        title: id.to_string(),
        cwd: PathBuf::from(cwd),
        created_at_ms: updated_at_ms,
        updated_at_ms,
        status,
        hidden: false,
        source_label: "test".to_string(),
        summary: String::new(),
        companion: false,
        pid: None,
        rollout_path: None,
    }
}

fn session_rows(rows: &[Row]) -> Vec<&Row> {
    rows.iter()
        .filter(|r| matches!(r, Row::Session { .. }))
        .collect()
}

fn section_headers(rows: &[Row]) -> Vec<Section> {
    rows.iter()
        .filter_map(|r| match r {
            Row::SectionHeader { section, .. } => Some(*section),
            _ => None,
        })
        .collect()
}

fn project_headers(rows: &[Row]) -> Vec<&Row> {
    rows.iter()
        .filter(|r| matches!(r, Row::ProjectHeader { .. }))
        .collect()
}

/// Deterministically move the cursor onto the session row with `id`.
fn select(app: &mut App, id: &str) {
    app.move_selection(-100_000);
    for _ in 0..10_000 {
        if app.selected().map(|s| s.id.as_str()) == Some(id) {
            return;
        }
        app.move_selection(1);
    }
    panic!("session {id} not selectable");
}

// --- Preserved v1 behavior (filter/anchor) ---

#[test]
fn filter_matches_title_and_cwd_case_insensitive() {
    let sessions = vec![
        sess(BackendKind::Codex, "alpha", "/synthetic/apples", 300, Status::Done),
        sess(BackendKind::Codex, "beta", "/synthetic/bananas", 200, Status::Done),
    ];
    let mut app = App::new(sessions);
    assert_eq!(session_rows(&app.visible()).len(), 2);

    // Case-insensitive title match ("alpha" == title of the first session).
    app.set_filter("ALPHA".to_string());
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

// --- v2 list model (tests 31-37) ---

#[test]
fn state_sections_order_and_fold() {
    let sessions = vec![
        sess(BackendKind::Codex, "needs", "/p", 600, Status::NeedsInput),
        sess(BackendKind::Codex, "work", "/p", 500, Status::Working),
        sess(BackendKind::Codex, "idle", "/p", 400, Status::Idle),
        sess(BackendKind::Codex, "done", "/p", 300, Status::Done),
        sess(BackendKind::Codex, "failed", "/p", 200, Status::Failed),
        sess(BackendKind::Codex, "stopped", "/p", 100, Status::Stopped),
    ];
    let app = App::new(sessions);
    let rows = app.visible();

    // Section headers appear in the fixed order; Failed/Stopped have no headers.
    assert_eq!(
        section_headers(&rows),
        vec![Section::NeedsInput, Section::Working, Section::Idle, Section::Done]
    );
    // Failed and Stopped rows keep their own status and sit in the Done section.
    assert!(rows.iter().any(
        |r| matches!(r, Row::Session { id, status: Status::Failed, .. } if id == "failed")
    ));
    assert!(rows.iter().any(
        |r| matches!(r, Row::Session { id, status: Status::Stopped, .. } if id == "stopped")
    ));

    // Empty sections are omitted: a Working-only app has only the Working header.
    let only_working = App::new(vec![sess(
        BackendKind::Codex,
        "w",
        "/p",
        1,
        Status::Working,
    )]);
    assert_eq!(section_headers(&only_working.visible()), vec![Section::Working]);
}

#[test]
fn done_overflow_more_marker() {
    let sessions: Vec<Session> = (0..20)
        .map(|i| {
            sess(
                BackendKind::Codex,
                &format!("d{i}"),
                "/p",
                1000 - i as i64,
                Status::Done,
            )
        })
        .collect();
    let app = App::new(sessions);
    let rows = app.visible();

    // 15 Done session rows + a "… 5 more" marker, no 16th session row.
    assert_eq!(session_rows(&rows).len(), 15);
    assert!(
        rows.iter()
            .any(|r| matches!(r, Row::MoreMarker { hidden: 5 }))
    );
}

#[test]
fn toggle_group_mode_project_rows() {
    // A codex and an opencode session sharing one cwd must merge into one project.
    let sessions = vec![
        sess(BackendKind::Codex, "cx", "/synthetic/shared", 300, Status::Working),
        sess(BackendKind::Opencode, "oc", "/synthetic/shared", 200, Status::Done),
    ];
    let mut app = App::new(sessions);
    assert_eq!(app.group_mode(), GroupMode::ByState);

    // Ctrl+S -> ByProject: one ProjectHeader (cross-backend merge), no SectionHeaders.
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByProject);
    let rows = app.visible();
    assert!(section_headers(&rows).is_empty());
    assert_eq!(project_headers(&rows).len(), 1);
    assert!(rows.iter().any(|r| matches!(
        r,
        Row::ProjectHeader { root, count: 2 } if root == &PathBuf::from("/synthetic/shared")
    )));
    assert_eq!(session_rows(&rows).len(), 2);

    // Toggle back -> sections again.
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);
    assert!(project_headers(&app.visible()).is_empty());
    assert!(!section_headers(&app.visible()).is_empty());
}

#[test]
fn show_all_covers_companions_and_archived() {
    let mut companion = sess(BackendKind::Codex, "comp", "/p", 300, Status::Idle);
    companion.companion = true;
    let mut archived = sess(BackendKind::Codex, "arch", "/p", 200, Status::Done);
    archived.hidden = true;
    let visible = sess(BackendKind::Codex, "vis", "/p", 100, Status::Done);
    let mut app = App::new(vec![companion, archived, visible]);

    // Default view hides both the companion and the archived row.
    let ids: Vec<String> = session_rows(&app.visible())
        .iter()
        .filter_map(|r| match r {
            Row::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["vis".to_string()]);
    assert_eq!(app.hidden_count(), 2);

    // 'a' reveals both hidden classes.
    app.toggle_show_all();
    let shown: Vec<String> = session_rows(&app.visible())
        .iter()
        .filter_map(|r| match r {
            Row::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert!(shown.contains(&"comp".to_string()));
    assert!(shown.contains(&"arch".to_string()));
    assert!(shown.contains(&"vis".to_string()));
}

#[test]
fn kill_stage_two_stage() {
    let sessions = vec![
        sess(BackendKind::Codex, "work", "/p", 300, Status::Working),
        sess(BackendKind::Codex, "other", "/p", 200, Status::Working),
        sess(BackendKind::Codex, "done", "/p", 100, Status::Done),
    ];
    let mut app = App::new(sessions);

    // Working session: first press arms + Stop; second press within 2s -> Remove.
    select(&mut app, "work");
    assert_eq!(app.kill_stage(1_000), KillStage::Stop);
    assert_eq!(app.kill_stage(2_999), KillStage::Remove); // 1999ms later, in window
    // Third press later re-arms (window elapsed / cleared by the remove) -> Stop.
    assert_eq!(app.kill_stage(5_000), KillStage::Stop);

    // Done session: first press is a silent Noop (not stoppable), second -> Remove.
    select(&mut app, "done");
    assert_eq!(app.kill_stage(10_000), KillStage::Noop);
    assert_eq!(app.kill_stage(11_000), KillStage::Remove);

    // A second press on a DIFFERENT session re-arms it, never removes.
    select(&mut app, "work");
    assert_eq!(app.kill_stage(20_000), KillStage::Stop);
    select(&mut app, "other");
    assert_eq!(app.kill_stage(20_500), KillStage::Stop);
}

#[test]
fn format_elapsed_buckets() {
    assert_eq!(format_elapsed(45_000), "45s");
    assert_eq!(format_elapsed(180_000), "3m");
    assert_eq!(format_elapsed(7_200_000), "2h");
    assert_eq!(format_elapsed(432_000_000), "5d");
    assert_eq!(format_elapsed(-5), "0s");
}

#[test]
fn rows_carry_summary_and_updated() {
    let mut s = sess(BackendKind::Codex, "sx", "/p", 12_345, Status::Working);
    s.summary = "one-line preview".to_string();
    let app = App::new(vec![s]);
    let rows = app.visible();

    let row = rows
        .iter()
        .find(|r| matches!(r, Row::Session { id, .. } if id == "sx"))
        .expect("session row");
    match row {
        Row::Session {
            summary,
            updated_at_ms,
            ..
        } => {
            assert_eq!(summary, "one-line preview");
            assert_eq!(*updated_at_ms, 12_345);
        }
        _ => unreachable!(),
    }
}
