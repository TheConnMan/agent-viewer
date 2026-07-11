use agent_viewer_core::{BackendKind, Session, Status};
use agent_viewer_tui::app::{
    App, Composer, DetachTracker, GroupMode, KillStage, Row, Section, format_elapsed, row_layout,
};
use std::path::PathBuf;

/// Synthetic session with a nonexistent cwd so project_root falls back to cwd
/// (no filesystem, no subprocess).
fn sess(backend: BackendKind, id: &str, cwd: &str, updated_at_ms: i64, status: Status) -> Session {
    Session {
        backend,
        id: id.to_string(),
        short_id: None,
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
    // v2.1 default is ByProject; toggle into ByState to inspect the state sections.
    let mut app = App::new(sessions);
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);
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
    let mut only_working = App::new(vec![sess(
        BackendKind::Codex,
        "w",
        "/p",
        1,
        Status::Working,
    )]);
    only_working.toggle_group_mode();
    assert_eq!(section_headers(&only_working.visible()), vec![Section::Working]);
}

#[test]
fn done_section_is_uncapped() {
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
    // ByState view: the Done section now lists every session (the 15-cap + MoreMarker
    // are gone in v2.1).
    let mut app = App::new(sessions);
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);
    let rows = app.visible();

    assert_eq!(section_headers(&rows), vec![Section::Done]);
    assert_eq!(session_rows(&rows).len(), 20);
}

#[test]
fn toggle_group_mode_project_rows() {
    // A codex and an opencode session sharing one cwd must merge into one project.
    let sessions = vec![
        sess(BackendKind::Codex, "cx", "/synthetic/shared", 300, Status::Working),
        sess(BackendKind::Opencode, "oc", "/synthetic/shared", 200, Status::Done),
    ];
    let mut app = App::new(sessions);
    // v2.1 startup default is ByProject: one ProjectHeader (cross-backend merge), no sections.
    assert_eq!(app.group_mode(), GroupMode::ByProject);
    let rows = app.visible();
    assert!(section_headers(&rows).is_empty());
    assert_eq!(project_headers(&rows).len(), 1);
    assert!(rows.iter().any(|r| matches!(
        r,
        Row::ProjectHeader { root, count: 2 } if root == &PathBuf::from("/synthetic/shared")
    )));
    assert_eq!(session_rows(&rows).len(), 2);

    // Ctrl+S -> ByState: section headers, no project headers.
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);
    assert!(project_headers(&app.visible()).is_empty());
    assert!(!section_headers(&app.visible()).is_empty());

    // Toggle back -> ByProject again.
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByProject);
    assert!(!project_headers(&app.visible()).is_empty());
    assert!(section_headers(&app.visible()).is_empty());
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

// --- Sticky selection across per-tick reordering (finding 4) ---

#[test]
fn selection_and_arm_stick_across_reorder() {
    let sessions = vec![
        sess(BackendKind::Codex, "a", "/p", 300, Status::Working),
        sess(BackendKind::Codex, "b", "/p", 200, Status::Working),
    ];
    let mut app = App::new(sessions);
    select(&mut app, "b");
    // Arm b for removal, then confirm the hint is on b.
    assert_eq!(app.kill_stage(1_000), KillStage::Stop);
    assert!(app.is_armed(1_500));

    // A refresh reorders: b becomes the most-recent session and moves to a new row.
    app.set_sessions(vec![
        sess(BackendKind::Codex, "a", "/p", 300, Status::Working),
        sess(BackendKind::Codex, "b", "/p", 400, Status::Working),
    ]);
    // Selection follows session b to its new row (not the stale row index, which now
    // holds a different session).
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("b"));
    // The armed Ctrl+X hint is still on b (not desynced onto a), and the second press
    // removes b — proving the armed identity survived the reorder.
    assert!(app.is_armed(1_600));
    assert_eq!(app.kill_stage(2_000), KillStage::Remove);
}

#[test]
fn selection_clamps_when_selected_session_vanishes() {
    let sessions = vec![
        sess(BackendKind::Codex, "a", "/p", 300, Status::Working),
        sess(BackendKind::Codex, "b", "/p", 200, Status::Working),
    ];
    let mut app = App::new(sessions);
    select(&mut app, "b");
    // b disappears from the refresh; selection falls back to a surviving session row.
    app.set_sessions(vec![sess(BackendKind::Codex, "a", "/p", 300, Status::Working)]);
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("a"));
}

// --- Row layout reserves the elapsed slot (finding 3) ---

#[test]
fn row_layout_reserves_elapsed_for_long_titles() {
    let width = 40;
    let tag_len = 2;
    let elapsed_len = 2; // e.g. "3h"
    let long = "this-is-an-extremely-long-session-title-that-overflows-the-row";
    let (name, summary, pad) = row_layout(width, tag_len, long, "", elapsed_len);
    // The title is truncated rather than clipping the right-aligned elapsed.
    assert!(name.chars().count() < long.chars().count());
    assert!(summary.is_empty());
    // The whole row (fixed left + name + pad + elapsed) fits exactly in width, so the
    // elapsed field is never pushed off the line.
    let left_fixed = 4 + tag_len;
    assert_eq!(left_fixed + name.chars().count() + pad + elapsed_len, width);
}

#[test]
fn row_layout_fits_name_and_summary_with_flush_elapsed() {
    let width = 60;
    let tag_len = 2;
    let elapsed_len = 3; // "10s"
    let (name, summary, pad) = row_layout(width, tag_len, "sess", "a short summary", elapsed_len);
    assert_eq!(name, "sess");
    assert_eq!(summary, "a short summary");
    let used = (4 + tag_len) + name.chars().count() + 2 + summary.chars().count();
    assert_eq!(used + pad + elapsed_len, width);
}

#[test]
fn row_layout_drops_summary_when_no_room() {
    // Width fits the name and the reserved elapsed but leaves nothing for a summary.
    let tag_len = 2;
    let elapsed_len = 3;
    let width = (4 + tag_len) + 4 + 1 + elapsed_len; // left_fixed + name(4) + gap + elapsed
    let (name, summary, pad) = row_layout(width, tag_len, "sess", "wont fit", elapsed_len);
    assert_eq!(name, "sess");
    assert!(summary.is_empty());
    assert!(pad >= 1);
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

// --- v2.1 inline spawn composer (item 8) ---

#[test]
fn composer_edit_and_backend_cycle() {
    let mut c = Composer::new();
    // Fresh composer: empty text, defaults to the Claude backend.
    assert!(c.is_empty());
    assert_eq!(c.text(), "");
    assert_eq!(c.backend(), BackendKind::Claude);

    c.push_char('h');
    c.push_char('i');
    assert_eq!(c.text(), "hi");
    assert!(!c.is_empty());

    c.backspace();
    assert_eq!(c.text(), "h");
    c.backspace();
    c.backspace(); // backspace on empty is a no-op, not a panic.
    assert!(c.is_empty());

    // Tab cycles Claude -> Codex -> Opencode -> Claude.
    c.cycle_backend();
    assert_eq!(c.backend(), BackendKind::Codex);
    c.cycle_backend();
    assert_eq!(c.backend(), BackendKind::Opencode);
    c.cycle_backend();
    assert_eq!(c.backend(), BackendKind::Claude);

    c.push_char('x');
    c.clear();
    assert!(c.is_empty());
    assert_eq!(c.text(), "");
}

#[test]
fn spawn_target_by_project_is_group_root_by_state_is_cwd() {
    // A real git-marked project so project_root(sub) folds up to the repo root, letting the
    // two grouping modes yield different spawn targets.
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let cwd = repo.join("sub");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut app = App::new(vec![sess(
        BackendKind::Codex,
        "s",
        cwd.to_str().unwrap(),
        100,
        Status::Working,
    )]);

    // Default ByProject: a session row's spawn target is its project group root (the repo).
    assert_eq!(app.group_mode(), GroupMode::ByProject);
    select(&mut app, "s");
    assert_eq!(app.spawn_target(), Some(repo.clone()));

    // ByState: the spawn target is the selected session's own cwd (the subdir).
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);
    select(&mut app, "s");
    assert_eq!(app.spawn_target(), Some(cwd.clone()));
}

#[test]
fn spawn_target_none_when_list_empty() {
    let app = App::new(Vec::new());
    assert_eq!(app.spawn_target(), None);
}

// --- v2.1 detach tracker (item 9) ---

#[test]
fn detach_tracker_left_gates_on_empty_input() {
    let mut t = DetachTracker::new();
    // Fresh: nothing pending -> left detaches.
    assert!(t.detach_on_left());

    // Type two chars -> left is forwarded to the pty, not a detach.
    t.on_char();
    t.on_char();
    assert!(!t.detach_on_left());

    // Backspace twice clears the buffer -> left detaches again.
    t.on_backspace();
    t.on_backspace();
    assert!(t.detach_on_left());

    // Backspace saturates at zero (no underflow) -> still detaches.
    t.on_backspace();
    assert!(t.detach_on_left());

    // Type then Enter (submit) resets pending to zero -> left detaches.
    t.on_char();
    assert!(!t.detach_on_left());
    t.on_enter();
    assert!(t.detach_on_left());
}
