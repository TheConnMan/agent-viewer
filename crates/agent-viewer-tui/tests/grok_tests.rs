use agent_viewer_core::{BackendKind, Session, SessionOrigin, Status};
use agent_viewer_tui::app::{App, Composer, GroupMode, Row, Section, SpawnRoute};
use agent_viewer_tui::pr_cache::PrStatusCache;
use agent_viewer_tui::shared_listing::TargetRequest;
use agent_viewer_tui::ui::wall::wall_sessions;
use agent_viewer_tui::ui::{Draw, ListHit, Mode, Pulses, ThemeState, draw};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::cell::RefCell;
use std::path::{Path, PathBuf};

fn session(
    backend: BackendKind,
    id: &str,
    title: &str,
    cwd: &str,
    created_at_ms: i64,
    status: Status,
) -> Session {
    Session {
        backend,
        id: id.to_string(),
        short_id: None,
        origin: SessionOrigin::Interactive,
        title: title.to_string(),
        cwd: PathBuf::from(cwd),
        git_branch: None,
        status,
        created_at_ms,
        updated_at_ms: created_at_ms,
        hidden: false,
        companion: false,
        subagent: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

#[test]
fn grok_identity_is_available_to_the_public_composer_surface() {
    let mut composer = Composer::new();
    composer.set_available_backends(vec![
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Grok,
    ]);
    composer.select_backend(BackendKind::Grok);
    composer.set_models(
        vec!["default".to_string(), "grok-4".to_string()],
        BackendKind::Grok,
    );
    composer.cycle_model();

    assert_eq!(composer.backend(), BackendKind::Grok);
    assert_eq!(composer.provider_name(), "grok");
    assert_eq!(composer.model(), "grok-4");
    assert_eq!(BackendKind::Grok.tag(), "[gx]");
}

#[test]
fn pinned_grok_spawns_directly_while_auto_and_existing_pins_remain_routed() {
    let mut composer = Composer::new();
    composer.set_available_backends(vec![
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Grok,
    ]);
    composer.set_auto_available(true);

    composer.select_backend(BackendKind::Grok);
    assert_eq!(composer.spawn_route(false), SpawnRoute::DirectBackend);

    composer.select_backend(BackendKind::Codex);
    assert_eq!(composer.spawn_route(false), SpawnRoute::Router);
    composer.select_backend(BackendKind::Claude);
    assert_eq!(composer.spawn_route(false), SpawnRoute::Router);

    composer.default_to_auto();
    assert!(composer.is_auto());
    assert_eq!(composer.spawn_route(false), SpawnRoute::Router);
}

#[test]
fn grok_rows_join_project_and_state_groups_in_backend_order() {
    let shared = "/synthetic/grok-shared";
    let mut app = App::new(vec![
        session(
            BackendKind::Grok,
            "grok",
            "Grok row",
            shared,
            100,
            Status::Working,
        ),
        session(
            BackendKind::Claude,
            "claude",
            "Claude row",
            shared,
            100,
            Status::Working,
        ),
        session(
            BackendKind::Codex,
            "codex",
            "Codex row",
            shared,
            100,
            Status::Working,
        ),
    ]);

    assert_eq!(app.group_mode(), GroupMode::ByProject);
    assert!(app.visible().iter().any(|row| matches!(
        row,
        Row::ProjectHeader { root, count: 3, .. } if root == Path::new(shared)
    )));
    assert!(app.visible().iter().any(|row| matches!(
        row,
        Row::Session { backend: BackendKind::Grok, id, .. } if id == "grok"
    )));

    app.toggle_group_mode();

    assert_eq!(app.group_mode(), GroupMode::ByState);
    assert!(matches!(
        app.visible().first(),
        Some(Row::SectionHeader {
            section: Section::Working,
            count: 3,
            ..
        })
    ));
    let backends = app
        .visible()
        .iter()
        .filter_map(|row| match row {
            Row::Session { backend, .. } => Some(*backend),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        backends,
        vec![BackendKind::Codex, BackendKind::Claude, BackendKind::Grok]
    );
}

#[test]
fn grok_filter_and_action_request_keep_the_exact_backend_identity() {
    let shared_id = "same-id";
    let mut app = App::new(vec![
        session(
            BackendKind::Codex,
            shared_id,
            "ordinary row",
            "/synthetic/shared-id",
            100,
            Status::Idle,
        ),
        session(
            BackendKind::Grok,
            shared_id,
            "Grok Search Needle",
            "/synthetic/shared-id",
            200,
            Status::Idle,
        ),
    ]);

    app.set_filter("search needle".to_string());
    let filtered = app
        .visible()
        .iter()
        .filter_map(|row| match row {
            Row::Session { backend, id, .. } => Some((*backend, id.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(filtered, vec![(BackendKind::Grok, shared_id)]);

    app.set_filter(String::new());
    assert!(app.select_by_key(&(BackendKind::Grok, shared_id.to_string())));
    let selected = app.selected().expect("selected Grok row");
    assert_eq!(selected.backend, BackendKind::Grok);
    assert_eq!(selected.id, shared_id);
    let request = TargetRequest::from(selected);
    assert_eq!(request.backend(), BackendKind::Grok);
    assert_eq!(request.id(), shared_id);
}

#[test]
fn grok_text_mark_renders_with_the_theme_text_color() {
    let app = App::new(vec![session(
        BackendKind::Grok,
        "grok-mark",
        "Grok mark",
        "/synthetic/grok-mark",
        100,
        Status::Idle,
    )]);
    let mode = Mode::Normal;
    let composer = Composer::new();
    let pulses = Pulses::new();
    let pr_status = PrStatusCache::new();
    let list_hit = RefCell::new(ListHit::default());
    let themes = ThemeState::default();
    let expected_color = themes.active().text;
    let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
    terminal
        .draw(|frame| {
            draw(
                frame,
                Draw {
                    app: &app,
                    workspace: Path::new("/synthetic/grok-mark"),
                    mode: &mode,
                    notice: "",
                    composer: &composer,
                    pulses: &pulses,
                    now_ms: 1_000,
                    attach: None,
                    pr_status: &pr_status,
                    logos: None,
                    list_hit: &list_hit,
                    themes: &themes,
                    sprite: Default::default(),
                    age_ramp: false,
                    tail: None,
                    wall: None,
                    wall_rects: &RefCell::new(Vec::new()),
                },
            );
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let mark = (0..buffer.area.height)
        .find_map(|y| {
            (0..buffer.area.width.saturating_sub(3)).find_map(|x| {
                let rendered = (x..x + 4)
                    .map(|column| buffer[(column, y)].symbol())
                    .collect::<String>();
                (rendered == "[gx]").then_some((x, y))
            })
        })
        .expect("Grok text mark");
    for x in mark.0..mark.0 + 4 {
        assert_eq!(buffer[(x, mark.1)].fg, expected_color);
    }
}

#[test]
fn grok_working_session_is_included_in_the_deterministic_wall_order() {
    let app = App::new(vec![
        session(
            BackendKind::Grok,
            "grok-wall",
            "Grok wall",
            "/synthetic/wall",
            100,
            Status::Working,
        ),
        session(
            BackendKind::Claude,
            "claude-wall",
            "Claude wall",
            "/synthetic/wall",
            100,
            Status::Working,
        ),
        session(
            BackendKind::Codex,
            "codex-wall",
            "Codex wall",
            "/synthetic/wall",
            100,
            Status::Working,
        ),
    ]);

    assert_eq!(
        wall_sessions(&app, 1_000),
        vec![
            (BackendKind::Codex, "codex-wall".to_string()),
            (BackendKind::Claude, "claude-wall".to_string()),
            (BackendKind::Grok, "grok-wall".to_string()),
        ]
    );
}
