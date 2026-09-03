//! Guards `ui::draw` list-frame cost at fleet scale.
//!
//! `draw_list` runs every frame at 8–10 Hz. The default by-project view is a short
//! on-screen window (~38 rows at 220x60), but show-all and a one-character filter
//! currently build a `ListItem` for every visible row. This bench draws a 12,000-row
//! list and a 38-row list into a `TestBackend` at 220x60 so a per-row regression
//! cannot hide behind the default view.
//!
//! Titles are ~2 KB so the desired-title-width scan matches the real store (avg
//! 1.9 KB on this box) rather than the short strings in `row_rebuild`.

use agent_viewer_core::{BackendKind, PrRef, Session, SessionOrigin, Status};
use agent_viewer_tui::app::{App, Composer};
use agent_viewer_tui::pr_cache::PrStatusCache;
use agent_viewer_tui::ui::{Draw, ListHit, Mode, Pulses, ThemeState};
use criterion::{Criterion, criterion_group, criterion_main};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::cell::RefCell;
use std::hint::black_box;
use std::path::{Path, PathBuf};

const TERMINAL_WIDTH: u16 = 220;
const TERMINAL_HEIGHT: u16 = 60;
const LONG_TITLE_WORDS: usize = 400;
const ROWS_12K: usize = 12_000;
const ROWS_38: usize = 38;

fn long_title(index: usize) -> String {
    let mut title = format!("session-{index:05} ");
    title.reserve(LONG_TITLE_WORDS * 6);
    for word in 0..LONG_TITLE_WORDS {
        if word > 0 {
            title.push(' ');
        }
        title.push_str("title");
    }
    title
}

fn synthetic_sessions(count: usize) -> Vec<Session> {
    (0..count)
        .map(|index| Session {
            backend: match index % 3 {
                0 => BackendKind::Codex,
                1 => BackendKind::Claude,
                _ => BackendKind::Grok,
            },
            id: format!("019f4dda-draw-{index:012}"),
            short_id: None,
            origin: SessionOrigin::Background,
            title: long_title(index),
            cwd: PathBuf::from(format!("/tmp/agent-viewer-draw-bench/p{}", index % 3)),
            git_branch: Some(format!("task/draw-bench-{index:04}")),
            status: match index % 5 {
                0 => Status::Working,
                1 => Status::needs_input(),
                2 => Status::Idle,
                3 => Status::Done,
                _ => Status::Unknown,
            },
            created_at_ms: 1_783_000_000_000 + index as i64 * 1_000,
            updated_at_ms: 1_783_500_000_000 + index as i64 * 997,
            hidden: false,
            companion: false,
            subagent: false,
            summary: format!("summary for session {index} with enough text to fill the row"),
            pid: index.is_multiple_of(3).then_some(10_000 + index as u32),
            rollout_path: None,
            pr_refs: if index.is_multiple_of(13) {
                vec![PrRef {
                    id: format!("{}", 300 + index % 400),
                    href: None,
                }]
            } else {
                Vec::new()
            },
            daemon_hosted: index.is_multiple_of(4),
        })
        .collect()
}

fn draw_frame(app: &App, terminal: &mut Terminal<TestBackend>) {
    let composer = Composer::new();
    let pulses = Pulses::new();
    let pr_status = PrStatusCache::default();
    let list_hit = RefCell::new(ListHit::default());
    let themes = ThemeState::default();
    let wall_rects = RefCell::new(Vec::new());
    terminal
        .draw(|frame| {
            agent_viewer_tui::ui::draw(
                frame,
                Draw {
                    app,
                    workspace: Path::new("/tmp"),
                    mode: &Mode::Normal,
                    notice: "",
                    composer: &composer,
                    pulses: &pulses,
                    now_ms: 1_783_600_000_000,
                    attach: None,
                    pr_status: &pr_status,
                    logos: None,
                    list_hit: &list_hit,
                    themes: &themes,
                    sprite: Default::default(),
                    age_ramp: false,
                    tail: None,
                    wall: None,
                    wall_rects: &wall_rects,
                },
            );
        })
        .expect("draw list frame");
}

fn bench_draw_list(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("tui_draw_list");
    group.sample_size(20);
    group.measurement_time(std::time::Duration::from_secs(15));

    let app_12k = App::new(synthetic_sessions(ROWS_12K));
    let visible_12k = app_12k.visible().len();
    assert!(
        visible_12k >= ROWS_12K,
        "12k bench must draw at least {ROWS_12K} rows, got {visible_12k}"
    );

    group.bench_function("draw_12000_rows_220x60", |bencher| {
        let mut terminal =
            Terminal::new(TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT)).expect("backend");
        bencher.iter(|| {
            draw_frame(black_box(&app_12k), &mut terminal);
            black_box(terminal.backend().buffer().area());
        });
    });

    let app_38 = App::new(synthetic_sessions(ROWS_38));
    group.bench_function("draw_38_rows_220x60", |bencher| {
        let mut terminal =
            Terminal::new(TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT)).expect("backend");
        bencher.iter(|| {
            draw_frame(black_box(&app_38), &mut terminal);
            black_box(terminal.backend().buffer().area());
        });
    });

    group.finish();
}

criterion_group!(benches, bench_draw_list);
criterion_main!(benches);
