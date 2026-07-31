//! Guards the TUI row-model rebuild at fleet scale.
//!
//! What this guards: `App::rebuild_rows` runs on every background refresh tick
//! (`set_sessions`), on every filter keystroke (`set_filter`), and on every grouping or
//! show-all toggle. It is not incremental - it re-filters, re-sorts, and re-groups the whole
//! session list, and in ByProject mode it folds each cwd to a project root, which touches the
//! filesystem on a cache miss. A regression here is felt directly as keystroke lag while
//! typing into the filter.
//!
//! Baseline measured on this box (2026-07-31, release build, 5,000 sessions across 120 real
//! project dirs):
//!
//! | bench | median |
//! | --- | --- |
//! | `app_new_5000` (cold: fresh root cache, 120 real `project_root` walks) | **6.4 ms** |
//! | `set_sessions_5000` (warm root cache, the refresh tick) | **6.5 ms** |
//! | `set_filter_one_char_5000` (one keystroke, ByProject) | **4.7 ms** |
//! | `set_filter_one_char_by_state_5000` (one keystroke, ByState) | **6.6 ms** |
//!
//! Every one of these is a whole-list rebuild, so they all land in the same few-milliseconds
//! band and the useful signal is the SHAPE, not the absolute time (which moves ~2x run to run
//! under agent load on this box).
//!
//! `set_sessions_5000` should sit AT OR BELOW `app_new_5000`: it does the same rebuild with a
//! warm `root_cache`, paying for selection re-anchoring instead of 120 filesystem walks. If
//! it drifts materially above, either the anchor bookkeeping grew or the `root_cache` memo
//! stopped hitting and every tick is walking the filesystem again.
//!
//! `set_filter_one_char_5000` is the per-keystroke budget. It is not cheaper than a full
//! refresh even though it looks like less work, because a non-empty filter lifts the
//! hidden/companion exclusion (so the rebuild spans the FULL set, not the visible one) and
//! because `passes_filter` lowercases the needle, the title, and the cwd once PER SESSION,
//! allocating three Strings per row per keystroke.
//!
//! Deterministic (fixed LCG seed). Project dirs are created in a temp dir so `project_root`
//! does real work without touching the user's home.

use agent_viewer_core::{BackendKind, PrRef, Session, SessionOrigin, Status};
use agent_viewer_tui::app::App;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::PathBuf;

/// Fleet size on this box's order (the codex status resolver's doc comment cites 2,883
/// threads; 5,000 is the headroom the 1.0 target is sized against).
const SESSIONS: usize = 5_000;

/// Distinct project roots the sessions fan out across.
const PROJECTS: usize = 120;

struct Lcg(u64);

impl Lcg {
    const fn new(seed: u64) -> Lcg {
        Lcg(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn range(&mut self, low: usize, high: usize) -> usize {
        low + (self.next_u64() as usize) % (high - low)
    }
}

const WORDS: [&str; 16] = [
    "resolver",
    "rollout",
    "session",
    "transcript",
    "backend",
    "snapshot",
    "listing",
    "worktree",
    "refresh",
    "status",
    "approval",
    "spawn",
    "registry",
    "viewer",
    "parser",
    "capability",
];

fn prose(rng: &mut Lcg, words: usize) -> String {
    let mut text = String::with_capacity(words * 9);
    for index in 0..words {
        if index > 0 {
            text.push(' ');
        }
        text.push_str(WORDS[rng.range(0, WORDS.len())]);
    }
    text
}

fn paragraph(rng: &mut Lcg, low: usize, high: usize) -> String {
    let words = rng.range(low, high);
    prose(rng, words)
}

/// `PROJECTS` real directories, each a git repo root with a nested subdir, so `project_root`
/// resolves through an actual filesystem walk exactly as it does in production.
fn project_dirs(root: &std::path::Path) -> Vec<PathBuf> {
    (0..PROJECTS)
        .map(|index| {
            let project = root.join(format!("project-{index:03}"));
            std::fs::create_dir_all(project.join(".git")).expect("create project .git dir");
            let nested = project.join("crates").join(format!("module-{}", index % 9));
            std::fs::create_dir_all(&nested).expect("create nested project dir");
            nested
        })
        .collect()
}

fn synthetic_sessions(dirs: &[PathBuf]) -> Vec<Session> {
    let mut rng = Lcg::new(0x701_5EED);
    (0..SESSIONS)
        .map(|index| Session {
            backend: match index % 2 {
                0 => BackendKind::Codex,
                _ => BackendKind::Claude,
            },
            id: format!("019f4dda-fecb-7b71-adba-{index:012}"),
            short_id: None,
            origin: SessionOrigin::Background,
            title: paragraph(&mut rng, 3, 12),
            cwd: dirs[index % dirs.len()].clone(),
            git_branch: Some(format!("task/bench-{index:04}")),
            status: match index % 5 {
                0 => Status::Working,
                1 => Status::needs_input(),
                2 => Status::Idle,
                3 => Status::Done,
                _ => Status::Unknown,
            },
            created_at_ms: 1_783_000_000_000 + index as i64 * 1_000,
            updated_at_ms: 1_783_500_000_000 + index as i64 * 997,
            hidden: index % 7 == 0,
            companion: index % 11 == 0,
            subagent: false,
            summary: paragraph(&mut rng, 6, 40),
            pid: (index % 3 == 0).then_some(10_000 + index as u32),
            rollout_path: None,
            pr_refs: if index % 13 == 0 {
                vec![PrRef {
                    id: format!("{}", 300 + index % 400),
                    href: None,
                }]
            } else {
                Vec::new()
            },
            daemon_hosted: index % 4 == 0,
        })
        .collect()
}

fn bench_rows(criterion: &mut Criterion) {
    let dir = tempfile::TempDir::new().expect("create bench tempdir");
    let dirs = project_dirs(dir.path());
    let sessions = synthetic_sessions(&dirs);

    let mut group = criterion.benchmark_group("tui_rows_5000");
    group.sample_size(30);
    group.measurement_time(std::time::Duration::from_secs(10));

    group.bench_function("app_new_5000", |bencher| {
        bencher.iter_batched(
            || sessions.clone(),
            |owned| black_box(App::new(owned)),
            criterion::BatchSize::LargeInput,
        );
    });

    let mut app = App::new(sessions.clone());
    assert!(!app.visible().is_empty());
    group.bench_function("set_sessions_5000", |bencher| {
        bencher.iter_batched(
            || sessions.clone(),
            |owned| {
                app.set_sessions(owned);
                black_box(app.visible().len())
            },
            criterion::BatchSize::LargeInput,
        );
    });

    // One keystroke into the filter. A non-empty filter also lifts the hidden/companion
    // exclusion, so this rebuild spans the whole session set.
    let mut filtered = App::new(sessions.clone());
    group.bench_function("set_filter_one_char_5000", |bencher| {
        bencher.iter(|| {
            filtered.set_filter(black_box("s".to_string()));
            black_box(filtered.visible().len())
        });
    });

    let mut by_state = App::new(sessions.clone());
    by_state.toggle_group_mode();
    group.bench_function("set_filter_one_char_by_state_5000", |bencher| {
        bencher.iter(|| {
            by_state.set_filter(black_box("s".to_string()));
            black_box(by_state.visible().len())
        });
    });

    group.finish();
}

criterion_group!(benches, bench_rows);
criterion_main!(benches);
