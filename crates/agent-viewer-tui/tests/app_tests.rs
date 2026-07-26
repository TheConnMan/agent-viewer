use agent_viewer_core::{BackendKind, Session, SessionOrigin, Status};
use agent_viewer_tui::app::{
    App, Composer, DetachTracker, GroupKey, GroupMode, KillStage, Row, Section, format_elapsed,
    row_layout,
};
use std::collections::HashSet;
use std::path::PathBuf;

/// Synthetic session with a nonexistent cwd so project_root falls back to cwd
/// (no filesystem, no subprocess).
fn sess(backend: BackendKind, id: &str, cwd: &str, updated_at_ms: i64, status: Status) -> Session {
    Session {
        backend,
        id: id.to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: id.to_string(),
        cwd: PathBuf::from(cwd),
        git_branch: None,
        status,
        created_at_ms: updated_at_ms,
        updated_at_ms,
        hidden: false,
        companion: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
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

// --- Mouse hit-test selection ---

#[test]
fn select_visible_index_lands_on_selectable_rows_only() {
    // Two sessions in different project dirs so the row model carries headers (and, between
    // groups, a spacer) alongside the session rows — the mix a mouse click can land on.
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "alpha",
            "/synthetic/apples",
            300,
            Status::Idle,
        ),
        sess(
            BackendKind::Codex,
            "beta",
            "/synthetic/bananas",
            200,
            Status::Idle,
        ),
    ];
    let mut app = App::new(sessions);
    let rows: Vec<Row> = app.visible().to_vec();

    // A Session-row index lands the cursor on that session.
    let beta_idx = rows
        .iter()
        .position(|r| matches!(r, Row::Session { id, .. } if id == "beta"))
        .expect("beta session row");
    assert!(app.select_visible_index(beta_idx));
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("beta"));
    assert_eq!(app.selected_index(), beta_idx);

    // A header index is selectable too (headers accept Enter to collapse/expand).
    let header_idx = rows
        .iter()
        .position(|r| matches!(r, Row::ProjectHeader { .. }))
        .expect("a project header row");
    assert!(app.select_visible_index(header_idx));
    assert_eq!(app.selected_index(), header_idx);

    // A Spacer index (if the layout has one) is rejected and leaves the selection put.
    if let Some(spacer_idx) = rows.iter().position(|r| matches!(r, Row::Spacer)) {
        let before = app.selected_index();
        assert!(!app.select_visible_index(spacer_idx));
        assert_eq!(app.selected_index(), before);
    }

    // An out-of-range index is a no-op.
    let before = app.selected_index();
    assert!(!app.select_visible_index(rows.len() + 5));
    assert_eq!(app.selected_index(), before);
}

// --- Preserved v1 behavior (filter/anchor) ---

#[test]
fn filter_matches_title_and_cwd_case_insensitive() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "alpha",
            "/synthetic/apples",
            300,
            Status::Done,
        ),
        sess(
            BackendKind::Codex,
            "beta",
            "/synthetic/bananas",
            200,
            Status::Done,
        ),
    ];
    let mut app = App::new(sessions);
    assert_eq!(session_rows(app.visible()).len(), 2);

    // Case-insensitive title match ("alpha" == title of the first session).
    app.set_filter("ALPHA".to_string());
    let rows = app.visible();
    let sr = session_rows(rows);
    assert_eq!(sr.len(), 1);
    assert!(matches!(sr[0], Row::Session { id, .. } if id == "alpha"));

    // Case-insensitive cwd match.
    app.set_filter("BANANAS".to_string());
    let rows = app.visible();
    let sr = session_rows(rows);
    assert_eq!(sr.len(), 1);
    assert!(matches!(sr[0], Row::Session { id, .. } if id == "beta"));

    // Clearing restores.
    app.set_filter(String::new());
    assert_eq!(session_rows(app.visible()).len(), 2);
}

// --- v2 list model (tests 31-37) ---

#[test]
fn state_sections_order_and_fold() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "needs",
            "/p",
            600,
            Status::needs_input(),
        ),
        sess(BackendKind::Codex, "work", "/p", 500, Status::Working),
        sess(BackendKind::Codex, "idle", "/p", 400, Status::Idle),
        sess(BackendKind::Codex, "done", "/p", 300, Status::Done),
        sess(BackendKind::Codex, "error", "/p", 200, Status::Error),
        sess(BackendKind::Codex, "unknown", "/p", 100, Status::Unknown),
    ];
    // v2.1 default is ByProject; toggle into ByState to inspect the state sections.
    let mut app = App::new(sessions);
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);
    let rows = app.visible();

    // Section headers appear in the fixed order.
    assert_eq!(
        section_headers(rows),
        vec![
            Section::NeedsInput,
            Section::Working,
            Section::Idle,
            Section::Done
        ]
    );
    // Error stays terminal while Unknown remains a nonterminal status.
    assert!(
        rows.iter()
            .any(|r| matches!(r, Row::Session { id, status: Status::Error, .. } if id == "error"))
    );
    assert!(
        rows.iter().any(
            |r| matches!(r, Row::Session { id, status: Status::Unknown, .. } if id == "unknown")
        )
    );

    // Empty sections are omitted: a Working-only app has only the Working header.
    let mut only_working = App::new(vec![sess(
        BackendKind::Codex,
        "w",
        "/p",
        1,
        Status::Working,
    )]);
    only_working.toggle_group_mode();
    assert_eq!(
        section_headers(only_working.visible()),
        vec![Section::Working]
    );
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

    assert_eq!(section_headers(rows), vec![Section::Done]);
    assert_eq!(session_rows(rows).len(), 20);
}

#[test]
fn toggle_group_mode_project_rows() {
    // A codex and an opencode session sharing one cwd must merge into one project.
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "cx",
            "/synthetic/shared",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Opencode,
            "oc",
            "/synthetic/shared",
            200,
            Status::Done,
        ),
    ];
    let mut app = App::new(sessions);
    // v2.1 startup default is ByProject: one ProjectHeader (cross-backend merge), no sections.
    assert_eq!(app.group_mode(), GroupMode::ByProject);
    let rows = app.visible();
    assert!(section_headers(rows).is_empty());
    assert_eq!(project_headers(rows).len(), 1);
    assert!(rows.iter().any(|r| matches!(
        r,
        Row::ProjectHeader { root, count: 2, .. } if root == &PathBuf::from("/synthetic/shared")
    )));
    assert_eq!(session_rows(rows).len(), 2);

    // Ctrl+S -> ByState: section headers, no project headers.
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);
    assert!(project_headers(app.visible()).is_empty());
    assert!(!section_headers(app.visible()).is_empty());

    // Toggle back -> ByProject again.
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByProject);
    assert!(!project_headers(app.visible()).is_empty());
    assert!(section_headers(app.visible()).is_empty());
}

#[test]
fn spacer_rows_separate_groups_but_never_bookend() {
    // Two distinct project groups -> exactly one spacer, sitting BETWEEN their headers.
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a",
            "/synthetic/one",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b",
            "/synthetic/two",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    assert_eq!(app.group_mode(), GroupMode::ByProject);
    let rows = app.visible();

    let spacers = rows.iter().filter(|r| matches!(r, Row::Spacer)).count();
    assert_eq!(spacers, 1); // one gap for two groups
    assert!(!matches!(rows.first(), Some(Row::Spacer))); // never leads
    assert!(!matches!(rows.last(), Some(Row::Spacer))); // never trails

    let headers: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| matches!(r, Row::ProjectHeader { .. }).then_some(i))
        .collect();
    let spacer_at = rows.iter().position(|r| matches!(r, Row::Spacer)).unwrap();
    assert!(headers[0] < spacer_at && spacer_at < headers[1]);

    // A single group has no spacer at all.
    let solo = App::new(vec![sess(
        BackendKind::Codex,
        "s",
        "/synthetic/one",
        1,
        Status::Working,
    )]);
    assert!(!solo.visible().iter().any(|r| matches!(r, Row::Spacer)));

    // Selection still lands only on session rows (spacers are skipped like headers).
    select(&mut app, "b");
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("b"));
}

#[test]
fn arrow_navigation_stops_on_headers_and_wraps_at_ends() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "needs",
            "/p",
            600,
            Status::needs_input(),
        ),
        sess(BackendKind::Codex, "work", "/p", 500, Status::Working),
        sess(BackendKind::Codex, "idle", "/p", 400, Status::Idle),
        sess(BackendKind::Codex, "done", "/p", 300, Status::Done),
    ];
    let mut app = App::new(sessions);
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);
    // Startup still lands on the first SESSION, not the first header.
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("needs"));

    // Up from the first session now lands ON the NEEDS INPUT header (headers are selectable).
    app.move_selection(-1);
    assert!(matches!(
        app.visible()[app.selected_index()],
        Row::SectionHeader {
            section: Section::NeedsInput,
            ..
        }
    ));
    assert!(app.selected().is_none()); // a header is not a session

    // Up again from the very first row wraps to the last row ("done").
    app.move_selection(-1);
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("done"));

    // Down from the last row wraps back to the very first row (the NEEDS INPUT header).
    app.move_selection(1);
    assert!(matches!(
        app.visible()[app.selected_index()],
        Row::SectionHeader {
            section: Section::NeedsInput,
            ..
        }
    ));

    // A mid-list Down stops on the next section header before its session.
    select(&mut app, "work");
    app.move_selection(1);
    assert!(matches!(
        app.visible()[app.selected_index()],
        Row::SectionHeader {
            section: Section::Idle,
            ..
        }
    ));
    app.move_selection(1);
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("idle"));
}

#[test]
fn arrow_wrap_is_scoped_to_the_filtered_rows() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "alpha",
            "/synthetic/aaa",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "beta",
            "/synthetic/bbb",
            200,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "gamma",
            "/synthetic/aaa-more",
            100,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    app.set_filter("aaa".to_string()); // only alpha + gamma remain visible (two groups)
    assert_eq!(session_rows(app.visible()).len(), 2);
    // beta is filtered out entirely.
    assert!(
        !app.visible()
            .iter()
            .any(|r| matches!(r, Row::Session { id, .. } if id == "beta"))
    );

    select(&mut app, "gamma"); // last visible session row
    app.move_selection(1); // Down at the bottom wraps to the FIRST visible row (aaa header)
    assert_eq!(app.selected_index(), 0);
    assert!(matches!(app.visible()[0], Row::ProjectHeader { .. }));

    app.move_selection(-1); // Up from the first row wraps back to the last (gamma)
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("gamma"));
}

#[test]
fn rename_resolves_target_by_key_after_reorder() {
    // Regression: rename must resolve its target by (backend, id), never by selection — the
    // background refresh reorders rows while the user types, drifting selection off the row.
    let a = sess(BackendKind::Codex, "a", "/p", 300, Status::Working);
    let b = sess(BackendKind::Claude, "b", "/p", 200, Status::Working);
    let mut app = App::new(vec![a, b]);
    select(&mut app, "a");
    let target = (BackendKind::Codex, "a".to_string());

    // A refresh reorders (b becomes most-recent); then selection drifts to b.
    app.set_sessions(vec![
        sess(BackendKind::Claude, "b", "/p", 400, Status::Working),
        sess(BackendKind::Codex, "a", "/p", 300, Status::Working),
    ]);
    select(&mut app, "b");
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("b")); // selection left a

    // The rename target is still resolvable by key (the old selected()-filter path returned
    // None here, silently no-oping the rename).
    let found = app
        .session_for(&target)
        .expect("target still resolvable by key");
    assert_eq!(found.id, "a");
    assert_eq!(found.backend, BackendKind::Codex);

    // And re-pinning selection back onto the rename row works (keeps the edit under cursor).
    assert!(app.select_by_key(&target));
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("a"));
}

#[test]
fn expansion_collapses_when_selection_leaves_the_expanded_row() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a",
            "/synthetic/one",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b",
            "/synthetic/two",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    let key_a = (BackendKind::Codex, "a".to_string());

    // Toggle expands the selected row; toggle again collapses it.
    select(&mut app, "a");
    app.toggle_expanded();
    assert_eq!(app.expanded(), Some(&key_a));
    app.toggle_expanded();
    assert_eq!(app.expanded(), None);

    // Moving the selection off the expanded row collapses it (never renders b's peek under a).
    select(&mut app, "a");
    app.toggle_expanded();
    assert_eq!(app.expanded(), Some(&key_a));
    select(&mut app, "b");
    assert_eq!(app.expanded(), None);

    // A filter that drops the expanded row also collapses it (selection can't stay on a).
    select(&mut app, "a");
    app.toggle_expanded();
    assert_eq!(app.expanded(), Some(&key_a));
    app.set_filter("two".to_string()); // only b's cwd matches
    assert_eq!(app.expanded(), None);
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
    let ids: Vec<String> = session_rows(app.visible())
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
    let shown: Vec<String> = session_rows(app.visible())
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
    app.set_sessions(vec![sess(
        BackendKind::Codex,
        "a",
        "/p",
        300,
        Status::Working,
    )]);
    assert_eq!(app.selected().map(|s| s.id.as_str()), Some("a"));
}

// --- Row layout reserves the elapsed slot (finding 3) ---

#[test]
fn row_layout_reserves_right_cluster_for_long_titles() {
    let width = 40;
    let mark_width = 1;
    let right_len = 10; // e.g. "Working 3h"
    let long = "this-is-an-extremely-long-session-title-that-overflows-the-row";
    let (name, detail, pad) = row_layout(width, mark_width, long, "", right_len);
    // The title is truncated rather than clipping the right-aligned cluster.
    assert!(name.chars().count() < long.chars().count());
    assert!(detail.is_empty());
    // The whole row (flush left + name + pad + right cluster) fits exactly in width, so the
    // status word + time cluster is never pushed off the line.
    let left_fixed = 3 + mark_width;
    assert_eq!(left_fixed + name.chars().count() + pad + right_len, width);
}

#[test]
fn row_layout_fits_name_and_summary_with_flush_right_cluster() {
    let width = 60;
    let mark_width = 1;
    let right_len = 12; // "#315 Done 2h"
    let (name, detail, pad) = row_layout(width, mark_width, "sess", "a short summary", right_len);
    assert_eq!(name, "sess");
    assert_eq!(detail, "a short summary");
    let used = (3 + mark_width) + name.chars().count() + 2 + detail.chars().count();
    assert_eq!(used + pad + right_len, width);
}

#[test]
fn row_layout_drops_summary_when_no_room() {
    // Width fits the name and the reserved right cluster but leaves nothing for a summary.
    let mark_width = 1;
    let right_len = 8;
    let width = (3 + mark_width) + 4 + 1 + right_len; // left_fixed + name(4) + gap + cluster
    let (name, detail, pad) = row_layout(width, mark_width, "sess", "wont fit", right_len);
    assert_eq!(name, "sess");
    assert!(detail.is_empty());
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

// --- v2.4: "/" to composer, hidden search, per-spawn model ---

#[test]
fn composer_accepts_slash_command_as_text() {
    // "/" is no longer a filter hotkey — it (and a full slash command) is plain task text
    // that spawns as the prompt.
    let mut c = Composer::new();
    for ch in "/implement RS-123".chars() {
        c.push_char(ch);
    }
    assert_eq!(c.text(), "/implement RS-123");
    assert!(!c.is_empty());
}

#[test]
fn filter_search_covers_hidden_and_companion_sessions() {
    let mut hidden = sess(
        BackendKind::Codex,
        "archived-match",
        "/p",
        300,
        Status::Done,
    );
    hidden.hidden = true;
    let mut companion = sess(
        BackendKind::Codex,
        "companion-match",
        "/p",
        250,
        Status::Idle,
    );
    companion.companion = true;
    let visible = sess(
        BackendKind::Codex,
        "plain-match",
        "/p",
        200,
        Status::Working,
    );
    let mut app = App::new(vec![hidden, companion, visible]);

    // Default view hides the archived + companion rows (only the plain one shows).
    let shown = |a: &App| -> Vec<String> {
        session_rows(a.visible())
            .iter()
            .filter_map(|r| match r {
                Row::Session { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(shown(&app), vec!["plain-match".to_string()]);
    assert_eq!(app.hidden_count(), 2);

    // A filter that matches all three surfaces the hidden + companion ones too.
    app.set_filter("match".to_string());
    let ids = shown(&app);
    assert!(ids.contains(&"archived-match".to_string()));
    assert!(ids.contains(&"companion-match".to_string()));
    assert!(ids.contains(&"plain-match".to_string()));
    assert_eq!(app.hidden_count(), 0); // while filtering, nothing matching is hidden

    // Clearing the filter re-hides them.
    app.set_filter(String::new());
    assert_eq!(shown(&app), vec!["plain-match".to_string()]);
    assert_eq!(app.hidden_count(), 2);
}

#[test]
fn composer_set_models_selects_default_first() {
    // The caller installs the discovered list; the leading entry (index 0) becomes the
    // current selection, and the install key records the backend it was scanned for.
    let mut c = Composer::new();
    c.set_models(
        vec!["default".into(), "gpt-5.6-sol".into(), "gpt-5.5".into()],
        BackendKind::Codex,
    );
    assert_eq!(c.model(), "default");
    assert_eq!(c.models_key(), Some(BackendKind::Codex));
}

#[test]
fn composer_cycle_model_over_discovered_list() {
    // Shift+Tab advances through the caller-installed list, wrapping at the end.
    let mut c = Composer::new();
    c.cycle_backend(); // -> codex
    c.set_models(
        vec!["default".into(), "gpt-5.6-sol".into(), "gpt-5.5".into()],
        BackendKind::Codex,
    );
    assert_eq!(c.model(), "default");
    c.cycle_model();
    assert_eq!(c.model(), "gpt-5.6-sol");
    c.cycle_model();
    assert_eq!(c.model(), "gpt-5.5");
    c.cycle_model();
    assert_eq!(c.model(), "default"); // wraps
}

#[test]
fn composer_cycle_model_noop_on_opencode_and_single_item() {
    // Opencode's list is huge, so Shift+Tab is a NO-OP there (the picker is the way).
    let mut c = Composer::new();
    c.cycle_backend(); // codex
    c.cycle_backend(); // opencode
    assert_eq!(c.backend(), BackendKind::Opencode);
    c.set_models(
        vec![
            "default".into(),
            "anthropic/claude".into(),
            "openai/gpt".into(),
        ],
        BackendKind::Opencode,
    );
    assert_eq!(c.model(), "default");
    c.cycle_model();
    assert_eq!(c.model(), "default"); // unchanged despite a multi-item list

    // A single-item list is a no-op on any backend.
    let mut c2 = Composer::new();
    c2.set_models(vec!["opus[1m]".into()], BackendKind::Claude);
    assert_eq!(c2.model(), "opus[1m]");
    c2.cycle_model();
    assert_eq!(c2.model(), "opus[1m]");
}

// --- v2.6: /model picker ---

#[test]
fn composer_is_model_command_detection() {
    let mut c = Composer::new();
    let set = |c: &mut Composer, s: &str| {
        c.clear();
        for ch in s.chars() {
            c.push_char(ch);
        }
    };
    set(&mut c, "/model");
    assert!(c.is_model_command());
    set(&mut c, "/model gpt");
    assert!(c.is_model_command());
    set(&mut c, "/implement");
    assert!(!c.is_model_command());
    // "/models" is neither "/model" nor a "/model " prefix.
    set(&mut c, "/models");
    assert!(!c.is_model_command());
    c.clear();
    assert!(!c.is_model_command()); // empty text
}

#[test]
fn composer_model_suggestions_filter_and_cap() {
    let mut c = Composer::new();
    // 20 models, 10 containing "gpt" plus one uppercase match to prove case-insensitivity.
    let mut list: Vec<String> = (0..10).map(|i| format!("gpt-{i}")).collect();
    list.extend((0..10).map(|i| format!("claude-{i}")));
    list.push("GPT-SOL".to_string());
    c.set_models(list, BackendKind::Codex);

    // Bare "/model": first 8 of the whole list.
    for ch in "/model".chars() {
        c.push_char(ch);
    }
    let all = c.model_suggestions();
    assert_eq!(all.len(), 8);
    assert_eq!(all[0], "gpt-0");

    // "/model gpt": only substring matches (case-insensitive), capped at 8 (11 match).
    c.clear();
    for ch in "/model gpt".chars() {
        c.push_char(ch);
    }
    let gpt = c.model_suggestions();
    assert_eq!(gpt.len(), 8);
    assert!(gpt.iter().all(|m| m.to_lowercase().contains("gpt")));
}

#[test]
fn composer_accept_model_sets_model_and_clears_text() {
    let mut c = Composer::new();
    c.set_models(
        vec!["default".into(), "gpt-5.6-sol".into(), "gpt-5.5".into()],
        BackendKind::Codex,
    );
    for ch in "/model 5.5".chars() {
        c.push_char(ch);
    }
    // The single "5.5" substring match is highlighted at index 0.
    assert_eq!(c.model_suggestions(), vec!["gpt-5.5".to_string()]);
    assert!(c.accept_model());
    assert_eq!(c.model(), "gpt-5.5");
    assert!(c.is_empty()); // accept clears the composer text
}

#[test]
fn composer_move_suggestion_drives_model_picker() {
    // Up/Down share the highlight between the two popups; here it walks the model picker.
    let mut c = Composer::new();
    c.set_models(
        vec!["default".into(), "gpt-a".into(), "gpt-b".into()],
        BackendKind::Codex,
    );
    for ch in "/model gpt".chars() {
        c.push_char(ch);
    }
    assert_eq!(
        c.model_suggestions(),
        vec!["gpt-a".to_string(), "gpt-b".to_string()]
    );
    assert_eq!(c.suggestion_highlight(), 0);
    c.move_suggestion(1);
    assert_eq!(c.suggestion_highlight(), 1);
    // accept_model picks the highlighted (second) entry.
    assert!(c.accept_model());
    assert_eq!(c.model(), "gpt-b");
}

#[test]
fn composer_slash_suggestions_empty_while_model_command() {
    // "/model" is the model meta-command: the SLASH popup stays empty (only the model
    // picker shows), even when "model" is itself a registered slash command.
    let mut c = Composer::new();
    c.set_commands(
        vec!["model".into(), "modernize".into()],
        (BackendKind::Claude, None),
    );
    for ch in "/model".chars() {
        c.push_char(ch);
    }
    assert!(c.is_model_command());
    assert!(c.suggestions().is_empty());
    assert!(!c.suggestions_active());
    assert!(c.model_picking()); // the model picker is the active popup instead
}

// --- v2.5: codex models, slash-command completion ---

#[test]
fn composer_slash_suggestions_prefix_filter() {
    let mut c = Composer::new();
    c.set_commands(
        vec!["implement".into(), "improve".into(), "review".into()],
        (BackendKind::Claude, None),
    );
    // Non-slash text: no suggestions at all.
    for ch in "hello".chars() {
        c.push_char(ch);
    }
    assert!(c.suggestions().is_empty());
    assert!(!c.suggestions_active());

    // "/i" narrows to the two i-prefixed commands (order preserved).
    c.clear();
    for ch in "/i".chars() {
        c.push_char(ch);
    }
    assert_eq!(c.suggestions(), vec!["implement", "improve"]);
    // "/impl" narrows further to just implement.
    for ch in "mpl".chars() {
        c.push_char(ch);
    }
    assert_eq!(c.suggestions(), vec!["implement"]);
    // A space commits the command word -> popup closes.
    for ch in "ement ".chars() {
        c.push_char(ch);
    }
    assert!(c.suggestions().is_empty());
}

#[test]
fn composer_tab_accepts_suggestion_only_when_popup_open() {
    let mut c = Composer::new();
    c.set_commands(
        vec!["implement".into(), "improve".into()],
        (BackendKind::Claude, None),
    );
    // No slash: popup closed (Tab would cycle the backend in the key handler).
    assert!(!c.suggestions_active());
    assert!(!c.accept_suggestion());
    // Type "/im": popup open; Tab accepts the highlighted (first) command as "/name ".
    for ch in "/im".chars() {
        c.push_char(ch);
    }
    assert!(c.suggestions_active());
    assert!(c.accept_suggestion());
    assert_eq!(c.text(), "/implement ");
    assert!(!c.suggestions_active()); // the trailing space closes the popup

    // Esc-dismiss hides the popup without clearing the text; editing re-opens it.
    c.clear();
    for ch in "/im".chars() {
        c.push_char(ch);
    }
    c.dismiss_suggestions();
    assert!(!c.suggestions_active());
    assert_eq!(c.text(), "/im"); // text intact
    c.push_char('p');
    assert!(c.suggestions_active()); // editing re-opens
}

#[test]
fn dir_scan_helpers_handle_missing_and_present_dirs() {
    use agent_viewer_tui::app::{file_stems, subdir_names};
    // Missing dir -> empty, never an error.
    let missing = std::path::Path::new("/nonexistent/xyz-does-not-exist-agentviewer");
    assert!(subdir_names(missing).is_empty());
    assert!(file_stems(missing).is_empty());
    // A real dir with one subdir and one file: subdir_names sees the dir, file_stems the file.
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("myskill")).unwrap();
    std::fs::write(dir.path().join("cmd.md"), "x").unwrap();
    assert_eq!(subdir_names(dir.path()), vec!["myskill".to_string()]);
    assert_eq!(file_stems(dir.path()), vec!["cmd".to_string()]); // ".md" stripped
}

// --- v2.5: collapse/expand a group ---

/// Deterministically move the cursor onto the ProjectHeader row for `root`.
/// selected() is None on a header, so `select` cannot be used here; we arrow
/// from the top and stop when selected_index points at the matching header.
fn select_project_header(app: &mut App, root: &PathBuf) {
    app.move_selection(-100_000);
    for _ in 0..10_000 {
        if matches!(
            app.visible().get(app.selected_index()),
            Some(Row::ProjectHeader { root: r, .. }) if r == root
        ) {
            return;
        }
        app.move_selection(1);
    }
    panic!("project header {root:?} not selectable");
}

/// Deterministically move the cursor onto the SectionHeader row for `section`.
fn select_section_header(app: &mut App, section: Section) {
    app.move_selection(-100_000);
    for _ in 0..10_000 {
        if matches!(
            app.visible().get(app.selected_index()),
            Some(Row::SectionHeader { section: s, .. }) if *s == section
        ) {
            return;
        }
        app.move_selection(1);
    }
    panic!("section header {section:?} not selectable");
}

fn has_session(app: &App, id: &str) -> bool {
    app.visible()
        .iter()
        .any(|r| matches!(r, Row::Session { id: rid, .. } if rid == id))
}

#[test]
fn group_key_storage_roundtrip() {
    // Project keys survive to_storage/from_storage exactly.
    let project = GroupKey::Project(PathBuf::from("/synthetic/one"));
    assert_eq!(
        GroupKey::from_storage(&project.to_storage()),
        Some(project.clone())
    );

    // Every Section variant survives too.
    for section in [
        Section::NeedsInput,
        Section::Working,
        Section::Idle,
        Section::Done,
    ] {
        let key = GroupKey::State(section);
        assert_eq!(GroupKey::from_storage(&key.to_storage()), Some(key));
    }

    // Malformed strings yield None (no panic).
    assert_eq!(GroupKey::from_storage("garbage-no-prefix"), None);
    assert_eq!(GroupKey::from_storage("state:bogus"), None);
    assert_eq!(GroupKey::from_storage(""), None);
}

#[test]
fn collapse_hides_group_rows_and_marks_header() {
    // Two distinct project dirs. Group a is newest so it sorts first.
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    assert_eq!(app.group_mode(), GroupMode::ByProject);

    let root_a = PathBuf::from("/synthetic/a");
    let root_b = PathBuf::from("/synthetic/b");
    select_project_header(&mut app, &root_a);
    assert!(matches!(
        app.visible().get(app.selected_index()),
        Some(Row::ProjectHeader { root, .. }) if root == &root_a
    ));

    let toggled = app.toggle_selected_group();
    assert_eq!(toggled, Some((GroupKey::Project(root_a.clone()), true)));

    // Group a's session rows are gone; group b's remain.
    assert!(!has_session(&app, "a1"));
    assert!(has_session(&app, "b1"));

    // Header a now reports collapsed; header b is still expanded.
    assert!(app.visible().iter().any(|r| matches!(
        r,
        Row::ProjectHeader { root, collapsed, .. } if root == &root_a && *collapsed
    )));
    assert!(app.visible().iter().any(|r| matches!(
        r,
        Row::ProjectHeader { root, collapsed, .. } if root == &root_b && !*collapsed
    )));

    assert!(app.is_group_collapsed(&GroupKey::Project(root_a)));
}

#[test]
fn expand_restores_group_rows() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    let root_a = PathBuf::from("/synthetic/a");

    select_project_header(&mut app, &root_a);
    assert_eq!(
        app.toggle_selected_group(),
        Some((GroupKey::Project(root_a.clone()), true))
    );
    assert!(!has_session(&app, "a1"));

    // A second toggle (cursor still on the header) expands and restores the rows.
    assert_eq!(
        app.toggle_selected_group(),
        Some((GroupKey::Project(root_a.clone()), false))
    );
    assert!(has_session(&app, "a1"));
    assert!(!app.is_group_collapsed(&GroupKey::Project(root_a)));
}

#[test]
fn selection_stays_on_header_after_toggle() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    let root_a = PathBuf::from("/synthetic/a");

    select_project_header(&mut app, &root_a);
    app.toggle_selected_group();

    // The cursor is still on the same header, and selected() is None there.
    assert!(matches!(
        app.visible().get(app.selected_index()),
        Some(Row::ProjectHeader { root, .. }) if root == &root_a
    ));
    assert!(app.selected().is_none());
}

#[test]
fn navigation_skips_collapsed_group_rows() {
    // Group a has two sessions; collapsing it must remove both from navigation.
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "a2",
            "/synthetic/a",
            290,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    let root_a = PathBuf::from("/synthetic/a");

    select_project_header(&mut app, &root_a);
    assert_eq!(
        app.toggle_selected_group(),
        Some((GroupKey::Project(root_a.clone()), true))
    );

    // Sweep down then up over the whole list; selection must never land on a hidden row,
    // and the surviving group's session must still be reachable.
    let mut reached_b1 = false;
    app.move_selection(-100_000);
    for _ in 0..(app.visible().len() + 2) {
        if let Some(s) = app.selected() {
            assert_ne!(s.id, "a1");
            assert_ne!(s.id, "a2");
            reached_b1 |= s.id == "b1";
        }
        app.move_selection(1);
    }
    for _ in 0..(app.visible().len() + 2) {
        if let Some(s) = app.selected() {
            assert_ne!(s.id, "a1");
            assert_ne!(s.id, "a2");
            reached_b1 |= s.id == "b1";
        }
        app.move_selection(-1);
    }
    assert!(
        reached_b1,
        "the uncollapsed group's session must stay reachable"
    );
}

#[test]
fn headers_are_selectable_via_arrows() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);

    // Arrow from the top until a ProjectHeader is under the cursor.
    app.move_selection(-100_000);
    let mut found = false;
    for _ in 0..(app.visible().len() + 2) {
        if matches!(
            app.visible().get(app.selected_index()),
            Some(Row::ProjectHeader { .. })
        ) {
            found = true;
            break;
        }
        app.move_selection(1);
    }
    assert!(found, "arrows never landed on a ProjectHeader");

    // On a header, selected() is None but toggle_selected_group would act.
    assert!(app.selected().is_none());
    assert!(app.toggle_selected_group().is_some());
}

#[test]
fn toggle_selected_group_none_on_session_row() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    select(&mut app, "a1");

    let before = app.visible().to_vec();
    assert_eq!(app.toggle_selected_group(), None);
    // The row model is untouched when the selected row is not a header.
    assert_eq!(app.visible(), before.as_slice());
}

#[test]
fn collapse_keyed_by_state_in_by_state_mode() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "needs",
            "/p",
            600,
            Status::needs_input(),
        ),
        sess(BackendKind::Codex, "work", "/p", 500, Status::Working),
        sess(BackendKind::Codex, "done", "/p", 300, Status::Done),
    ];
    let mut app = App::new(sessions);
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);

    select_section_header(&mut app, Section::Working);
    let toggled = app.toggle_selected_group();
    assert_eq!(toggled, Some((GroupKey::State(Section::Working), true)));

    // The Working section's row hides; other sections keep their rows.
    assert!(!has_session(&app, "work"));
    assert!(has_session(&app, "needs"));
    assert!(app.visible().iter().any(|r| matches!(
        r,
        Row::SectionHeader { section: Section::Working, collapsed, .. } if *collapsed
    )));
    assert!(app.is_group_collapsed(&GroupKey::State(Section::Working)));
}

#[test]
fn collapsed_state_survives_refresh() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    let root_a = PathBuf::from("/synthetic/a");

    select_project_header(&mut app, &root_a);
    assert_eq!(
        app.toggle_selected_group(),
        Some((GroupKey::Project(root_a.clone()), true))
    );

    // A background refresh replaces the session list (same members, reordered).
    app.set_sessions(vec![
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            250,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
    ]);

    // The collapse survives the rebuild: still collapsed, header still marked, rows still hidden.
    assert!(app.is_group_collapsed(&GroupKey::Project(root_a.clone())));
    assert!(app.visible().iter().any(|r| matches!(
        r,
        Row::ProjectHeader { root, collapsed, .. } if root == &root_a && *collapsed
    )));
    assert!(!has_session(&app, "a1"));
}

#[test]
fn set_collapsed_seeds_collapsed_groups() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    let root_a = PathBuf::from("/synthetic/a");

    let mut seed = HashSet::new();
    seed.insert(GroupKey::Project(root_a.clone()));
    app.set_collapsed(seed);

    // The seeded group renders collapsed from the start.
    assert!(app.is_group_collapsed(&GroupKey::Project(root_a.clone())));
    assert!(app.collapsed().contains(&GroupKey::Project(root_a.clone())));
    assert!(!has_session(&app, "a1"));
    assert!(app.visible().iter().any(|r| matches!(
        r,
        Row::ProjectHeader { root, collapsed, .. } if root == &root_a && *collapsed
    )));
}

#[test]
fn spawn_target_falls_back_to_project_header_root() {
    let sessions = vec![
        sess(
            BackendKind::Codex,
            "a1",
            "/synthetic/a",
            300,
            Status::Working,
        ),
        sess(
            BackendKind::Codex,
            "b1",
            "/synthetic/b",
            200,
            Status::Working,
        ),
    ];
    let mut app = App::new(sessions);
    let root_a = PathBuf::from("/synthetic/a");

    // ByProject: a selected ProjectHeader spawns into its own root.
    select_project_header(&mut app, &root_a);
    assert_eq!(app.spawn_target(), Some(root_a.clone()));

    // ByState: a selected SectionHeader has no spawn dir.
    app.toggle_group_mode();
    assert_eq!(app.group_mode(), GroupMode::ByState);
    select_section_header(&mut app, Section::Working);
    assert_eq!(app.spawn_target(), None);
}

#[test]
fn composer_set_models_preserves_a_deliberate_pick_on_a_same_backend_refresh() {
    // A background catalog refresh lands mid-compose. Re-installing the list must not throw
    // away the model the user picked, or a slow probe silently rewrites the spawn.
    let mut c = Composer::new();
    c.cycle_backend(); // -> codex
    c.set_models(
        vec!["default".into(), "gpt-5.6-sol".into()],
        BackendKind::Codex,
    );
    c.cycle_model();
    assert_eq!(c.model(), "gpt-5.6-sol");

    c.set_models(
        vec!["default".into(), "gpt-5.6-sol".into(), "gpt-5.7".into()],
        BackendKind::Codex,
    );
    assert_eq!(c.model(), "gpt-5.6-sol");
}

#[test]
fn composer_set_models_resets_when_the_pick_vanishes_from_the_refreshed_list() {
    // The selection must always be an entry of the installed list, or Shift+Tab has no
    // anchor to advance from and the spawn carries a model the backend just dropped.
    let mut c = Composer::new();
    c.cycle_backend(); // -> codex
    c.set_models(
        vec!["default".into(), "gpt-5.6-sol".into()],
        BackendKind::Codex,
    );
    c.cycle_model();
    assert_eq!(c.model(), "gpt-5.6-sol");

    c.set_models(vec!["default".into(), "gpt-5.7".into()], BackendKind::Codex);
    assert_eq!(c.model(), "default");
}

#[test]
fn composer_set_models_resets_the_pick_when_the_backend_changes() {
    // Codex's pick is meaningless to opencode; a backend switch always lands on its default.
    let mut c = Composer::new();
    c.cycle_backend(); // -> codex
    c.set_models(
        vec!["default".into(), "gpt-5.6-sol".into()],
        BackendKind::Codex,
    );
    c.cycle_model();
    assert_eq!(c.model(), "gpt-5.6-sol");

    c.cycle_backend(); // -> opencode
    c.set_models(
        vec!["default".into(), "gpt-5.6-sol".into()],
        BackendKind::Opencode,
    );
    assert_eq!(c.model(), "default");
}
