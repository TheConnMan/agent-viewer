//! Guards fleet-scale Codex status resolution: the per-tick cost of turning 5,000 registry
//! rows into six-state statuses.
//!
//! What this guards: `StatusResolver` exists so the 2 s tick does not re-read every rollout
//! tail. Its cache holds the TAIL STATE keyed by `(mtime, len)`, so a warm tick should cost
//! one `stat` per session and nothing more, while status is still recomputed from the fd
//! signal (that is what lets Idle -> Done fire with no file change).
//!
//! Baseline measured on this box (2026-07-31, release build, 5,000 rollouts of 16 KiB each):
//!
//! | bench | median |
//! | --- | --- |
//! | `pure_resolve_status_5000` (no cache, tail re-read every time) | **303 ms** |
//! | `resolver_cold_5000` (fresh `StatusResolver`, every entry a miss) | **315 ms** |
//! | `resolver_warm_5000` (cache hit on every session) | **5.2 ms** |
//! | `resolver_warm_closed_5000` (warm cache, opposite fd signal) | **6.1 ms** |
//!
//! The cache is worth **~60x** here, and that ratio is the thing to guard: cold tracks the
//! pure number because a cold resolver does the same work, while warm is 5,000 `fs::metadata`
//! calls and nothing else. If warm ever converges on cold, the `(mtime, len)` key has stopped
//! matching and every 2 s tick is re-reading 5,000 tails. `resolver_warm_closed_5000` staying
//! level with `resolver_warm_5000` is the proof that status is still RECOMPUTED from the fd
//! signal on a cache hit rather than memoized (that is what lets Idle -> Done fire with no
//! file change); if it ever went to zero, the cache would have swallowed the transition.
//!
//! Deterministic (fixed LCG seed), temp dir only, no procfs scan and no real `~/.codex`.

use agent_viewer_core::backend::Status;
use agent_viewer_core::codex::status::{FdSignal, RolloutOwner, StatusResolver, resolve_status};
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Fleet size on this box's order (the resolver's own doc comment cites 2,883 threads).
const SESSIONS: usize = 5_000;

/// Per-rollout size. Bigger than the 64 KiB tail window would make the fixture set 300+ MB;
/// 16 KiB keeps the whole set page-cache resident while still exercising a real read.
const ROLLOUT_BYTES: usize = 16 * 1024;

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

const WORDS: [&str; 12] = [
    "resolver", "rollout", "session", "backend", "snapshot", "listing", "refresh", "status",
    "approval", "spawn", "registry", "viewer",
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

fn stamp(tick: usize) -> String {
    let minute = (tick / 60) % 60;
    let second = tick % 60;
    format!("2026-07-10T21:{minute:02}:{second:02}.000Z")
}

fn emit(writer: &mut BufWriter<std::fs::File>, line: &str, written: &mut usize) {
    writer.write_all(line.as_bytes()).expect("write line");
    writer.write_all(b"\n").expect("write newline");
    *written += line.len() + 1;
}

/// One synthetic rollout. `terminal` decides whether the tail classifies Complete (the row
/// resolves Idle/Done) or MidTurn (Working/Error), so the bench spans both decision paths.
fn write_rollout(path: &Path, seed: u64, terminal: bool) {
    let mut writer = BufWriter::new(std::fs::File::create(path).expect("create rollout fixture"));
    let mut rng = Lcg::new(seed);
    let mut written = 0usize;
    let mut tick = 1usize;

    emit(
        &mut writer,
        &format!(
            r#"{{"timestamp":"{}","type":"session_meta","payload":{{"id":"{seed:016x}","cwd":"/home/bench/projects/p","originator":"codex_exec","cli_version":"0.144.1","source":"exec"}}}}"#,
            stamp(0)
        ),
        &mut written,
    );

    while written < ROLLOUT_BYTES {
        let time = stamp(tick);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"event_msg","payload":{{"type":"task_started","turn_id":"turn_{tick}"}}}}"#
            ),
            &mut written,
        );
        let text = paragraph(&mut rng, 20, 120);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"response_item","payload":{{"type":"message","id":"a_{tick}","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}}}"#
            ),
            &mut written,
        );
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_tokens":{}}},"rate_limits":null}}}}"#,
                tick * 91
            ),
            &mut written,
        );
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn_{tick}","completed_at":1783716494,"duration_ms":5000}}}}"#
            ),
            &mut written,
        );
        tick += 1;
    }

    if !terminal {
        // A started turn with nothing after it: the tail reads MidTurn.
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"task_started","turn_id":"turn_open"}}}}"#,
                stamp(tick)
            ),
            &mut written,
        );
    }
    writer.flush().expect("flush rollout fixture");
}

fn held(pid: u32) -> FdSignal {
    FdSignal::Held(RolloutOwner { pid, daemon: false })
}

fn fixtures() -> (tempfile::TempDir, Vec<PathBuf>) {
    let dir = tempfile::TempDir::new().expect("create bench tempdir");
    let paths: Vec<PathBuf> = (0..SESSIONS)
        .map(|index| {
            let path = dir.path().join(format!("rollout-{index:05}.jsonl"));
            write_rollout(
                &path,
                0xB00B_5EED_u64.wrapping_add(index as u64),
                index % 3 != 0,
            );
            path
        })
        .collect();
    (dir, paths)
}

fn bench_status(criterion: &mut Criterion) {
    let (_dir, paths) = fixtures();

    let mut group = criterion.benchmark_group("status_resolve_5000");
    group.sample_size(20);
    group.measurement_time(std::time::Duration::from_secs(10));

    group.bench_function("pure_resolve_status_5000", |bencher| {
        bencher.iter(|| {
            let mut working = 0usize;
            for (index, path) in paths.iter().enumerate() {
                let status = resolve_status(black_box(path), held(4_000 + index as u32), false);
                if status == Status::Working {
                    working += 1;
                }
            }
            black_box(working)
        });
    });

    group.bench_function("resolver_cold_5000", |bencher| {
        bencher.iter_batched(
            StatusResolver::new,
            |mut resolver| {
                let mut working = 0usize;
                for (index, path) in paths.iter().enumerate() {
                    let (status, _, _) =
                        resolver.resolve(black_box(path), held(4_000 + index as u32), false);
                    if status == Status::Working {
                        working += 1;
                    }
                }
                black_box(working)
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // One resolver, pre-warmed: every lookup is a (mtime, len) cache hit.
    let mut warm = StatusResolver::new();
    for (index, path) in paths.iter().enumerate() {
        warm.resolve(path, held(4_000 + index as u32), false);
    }

    group.bench_function("resolver_warm_5000", |bencher| {
        bencher.iter(|| {
            let mut working = 0usize;
            for (index, path) in paths.iter().enumerate() {
                let (status, _, _) =
                    warm.resolve(black_box(path), held(4_000 + index as u32), false);
                if status == Status::Working {
                    working += 1;
                }
            }
            black_box(working)
        });
    });

    // Same warm cache, opposite fd signal: proves status is recomputed, not memoized.
    group.bench_function("resolver_warm_closed_5000", |bencher| {
        bencher.iter(|| {
            let mut done = 0usize;
            for path in &paths {
                let (status, _, _) = warm.resolve(black_box(path), FdSignal::Closed, false);
                if status == Status::Done {
                    done += 1;
                }
            }
            black_box(done)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_status);
criterion_main!(benches);
