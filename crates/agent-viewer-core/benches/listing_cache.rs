//! Guards the shared-listing snapshot round trip that runs on the viewer's 2 s refresh tick.
//!
//! What this guards: `backend_listing_cache` stores the whole backend listing as one JSON
//! TEXT column. At this box's scale (thousands of sessions with real previews) that column
//! was measured at 24.4 MB, and the reviewed defect was the coordinator re-serializing and
//! rewriting all of it every single tick even when nothing had changed.
//!
//! Baseline measured on this box (2026-07-31, release build, 5,000 sessions serializing to
//! ~21 MiB of JSON):
//!
//! | bench | median |
//! | --- | --- |
//! | `serialize_5000` (`ListingCacheSnapshot::from_sessions`) | **16.4 ms** (1.27 GiB/s) |
//! | `deserialize_5000` (`read_listing_snapshot`) | **24.0 ms** (890 MiB/s) |
//! | `cycle_unchanged_5000` (matching generation, still fresh) | **5.0 ms** |
//! | `cycle_unchanged_small` (same claim, 10-session scope) | **4.9 us** |
//! | `store_changed_5000` (claim + publish) | **250 ms** |
//! | `cycle_changed_5000` (claim + serialize + publish + read) | **235 ms** |
//!
//! Read the two `cycle_unchanged` rows together - they are the point of this file. The
//! unchanged claim reads only the four small metadata columns and should not care how big
//! the snapshot is, yet it costs 5.0 ms against a ~21 MiB row and 4.9 us against a 40 KB one:
//! a **1000x** spread on a query that touches none of the payload. `snapshot_json` is
//! declared BEFORE `refreshed_at_ms`/`generation`/`lease_owner`/`lease_until_ms` in
//! `backend_listing_cache`, so SQLite walks the entire overflow chain to reach them. Even a
//! perfectly short-circuited idle tick still pays for the blob. A fix (column reorder, or
//! moving the blob to its own table) should collapse `cycle_unchanged_5000` onto
//! `cycle_unchanged_small`.
//!
//! `store_changed_5000` and `cycle_changed_5000` are the ~250 ms per-tick cost the reviewed
//! defect was paying unconditionally (a measured 24.4 MB snapshot rewritten every 2 s). A
//! change-detecting publish should make those two disappear from an idle tick entirely, not
//! get faster. Absolute times move ~2x run to run under agent load on this box; the ratios
//! above are the durable signal.
//!
//! Deterministic (fixed LCG seed), temp dir only, no network and no real viewer.db.

use agent_viewer_core::state::{LISTING_CACHE_FRESHNESS_MS, LISTING_CACHE_LEASE_MS};
use agent_viewer_core::{
    BackendKind, ListingCacheClaim, ListingCacheLease, ListingCacheScope, ListingCacheSnapshot,
    ListingCacheWrite, PrRef, Session, SessionOrigin, Status, ViewerDb,
};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::PathBuf;

/// Fleet size on this box's order (the status resolver's own doc comment cites 2,883
/// threads; 5,000 is the headroom the 1.0 target is sized against).
const SESSIONS: usize = 5_000;

/// Deterministic PRNG so every run builds byte-identical sessions.
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

/// `count` synthetic Codex rows whose `summary` field carries the long `threads.preview`
/// text that makes the real snapshot column multi-megabyte.
fn synthetic_sessions(count: usize) -> Vec<Session> {
    let mut rng = Lcg::new(0x0115_71C6);
    (0..count)
        .map(|index| {
            let project = index % 120;
            let cwd = PathBuf::from(format!(
                "/home/bench/projects/project-{project:03}/crates/module-{}",
                index % 9
            ));
            Session {
                backend: BackendKind::Codex,
                id: format!("019f4dda-fecb-7b71-adba-{index:012}"),
                short_id: None,
                origin: SessionOrigin::Background,
                title: paragraph(&mut rng, 3, 12),
                cwd,
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
                // The real cost driver: codex stores threads.preview here and it is long.
                summary: paragraph(&mut rng, 40, 900),
                pid: (index % 3 == 0).then_some(10_000 + index as u32),
                rollout_path: Some(PathBuf::from(format!(
                    "/home/bench/.codex/sessions/2026/07/31/rollout-2026-07-31T12-00-00-019f4dda-fecb-7b71-adba-{index:012}.jsonl"
                ))),
                pr_refs: if index % 13 == 0 {
                    vec![PrRef {
                        id: format!("{}", 300 + index % 400),
                        href: Some(format!(
                            "https://github.com/TheConnMan/agent-viewer/pull/{}",
                            300 + index % 400
                        )),
                    }]
                } else {
                    Vec::new()
                },
                daemon_hosted: index % 4 == 0,
            }
        })
        .collect()
}

fn scope() -> ListingCacheScope {
    ListingCacheScope::new(BackendKind::Codex, "bench-scope").expect("build listing cache scope")
}

fn open_db() -> (tempfile::TempDir, ViewerDb) {
    let dir = tempfile::TempDir::new().expect("create bench tempdir");
    let db = ViewerDb::open(&dir.path().join("viewer.db")).expect("open bench viewer.db");
    (dir, db)
}

/// Claim the scope with a stale clock so the claim always lands as `Claimed`.
fn claim(db: &ViewerDb, scope: &ListingCacheScope, now_ms: i64) -> ListingCacheLease {
    match db
        .claim_listing_refresh(
            Some(scope),
            None,
            now_ms,
            LISTING_CACHE_FRESHNESS_MS,
            LISTING_CACHE_LEASE_MS,
        )
        .expect("claim listing refresh")
    {
        ListingCacheClaim::Claimed(lease) => lease,
        other => panic!("expected a claim, got {other:?}"),
    }
}

fn bench_listing_cache(criterion: &mut Criterion) {
    let sessions = synthetic_sessions(SESSIONS);
    let scope = scope();
    let snapshot_bytes = serde_json::to_string(&sessions)
        .expect("serialize bench sessions")
        .len() as u64;

    let mut group = criterion.benchmark_group("listing_cache_5000");
    group.sample_size(20);
    group.measurement_time(std::time::Duration::from_secs(10));
    group.throughput(Throughput::Bytes(snapshot_bytes));

    group.bench_function("serialize_5000", |bencher| {
        bencher.iter_batched(
            || sessions.clone(),
            |owned| black_box(ListingCacheSnapshot::from_sessions(owned).unwrap()),
            criterion::BatchSize::LargeInput,
        );
    });

    // A published snapshot to read back. `now_ms` is fixed so the row stays fresh.
    let (_read_dir, read_db) = open_db();
    let published_at_ms = 1_783_600_000_000_i64;
    let lease = claim(&read_db, &scope, published_at_ms);
    assert_eq!(
        read_db
            .publish_listing(
                &lease,
                ListingCacheSnapshot::from_sessions(sessions.clone()).unwrap(),
                published_at_ms,
            )
            .expect("publish bench snapshot"),
        ListingCacheWrite::Published
    );
    let published_generation = lease.next_published_generation();

    group.bench_function("deserialize_5000", |bencher| {
        bencher.iter(|| {
            black_box(
                read_db
                    .read_listing_snapshot(black_box(&scope), published_at_ms)
                    .unwrap(),
            )
        });
    });

    // The idle-tick short circuit: same generation, still inside the freshness window.
    group.bench_function("cycle_unchanged_5000", |bencher| {
        bencher.iter(|| {
            let claim = read_db
                .claim_listing_refresh(
                    Some(black_box(&scope)),
                    Some(published_generation),
                    published_at_ms,
                    LISTING_CACHE_FRESHNESS_MS,
                    LISTING_CACHE_LEASE_MS,
                )
                .unwrap();
            assert!(matches!(claim, ListingCacheClaim::Unchanged));
            black_box(claim)
        });
    });

    // CONTROL for the line above: the identical unchanged claim against a scope holding a
    // TINY snapshot. The two must be the same cost - the unchanged path reads only the four
    // metadata columns and never touches `snapshot_json`. They are NOT, and the gap is the
    // finding: `snapshot_json` is declared BEFORE `refreshed_at_ms`/`generation`/
    // `lease_owner`/`lease_until_ms` in `backend_listing_cache`, so SQLite walks the whole
    // multi-megabyte overflow chain to reach them. Whatever fixes that (reordering the
    // columns, or splitting the blob into its own table) should collapse
    // `cycle_unchanged_5000` onto this number.
    let small = synthetic_sessions(10);
    let small_bytes = serde_json::to_string(&small)
        .expect("serialize small bench sessions")
        .len() as u64;
    let small_scope =
        ListingCacheScope::new(BackendKind::Codex, "bench-scope-small").expect("build small scope");
    let (_small_dir, small_db) = open_db();
    let small_lease = claim(&small_db, &small_scope, published_at_ms);
    small_db
        .publish_listing(
            &small_lease,
            ListingCacheSnapshot::from_sessions(small).unwrap(),
            published_at_ms,
        )
        .expect("publish small bench snapshot");
    let small_generation = small_lease.next_published_generation();
    group.throughput(Throughput::Bytes(small_bytes));
    group.bench_function("cycle_unchanged_small", |bencher| {
        bencher.iter(|| {
            let claim = small_db
                .claim_listing_refresh(
                    Some(black_box(&small_scope)),
                    Some(small_generation),
                    published_at_ms,
                    LISTING_CACHE_FRESHNESS_MS,
                    LISTING_CACHE_LEASE_MS,
                )
                .unwrap();
            assert!(matches!(claim, ListingCacheClaim::Unchanged));
            black_box(claim)
        });
    });
    group.throughput(Throughput::Bytes(snapshot_bytes));

    // Store-only: claim then publish an already-serialized snapshot.
    let (_store_dir, store_db) = open_db();
    let mut store_clock = 1_800_000_000_000_i64;
    group.bench_function("store_changed_5000", |bencher| {
        bencher.iter_batched(
            || {
                store_clock += 10_000;
                (
                    store_clock,
                    ListingCacheSnapshot::from_sessions(sessions.clone()).unwrap(),
                )
            },
            |(now_ms, snapshot)| {
                let lease = claim(&store_db, &scope, now_ms);
                black_box(store_db.publish_listing(&lease, snapshot, now_ms).unwrap())
            },
            criterion::BatchSize::LargeInput,
        );
    });

    // The full changed-tick cost: claim, serialize, publish, read back.
    let (_cycle_dir, cycle_db) = open_db();
    let mut cycle_clock = 1_800_000_000_000_i64;
    group.bench_function("cycle_changed_5000", |bencher| {
        bencher.iter_batched(
            || {
                cycle_clock += 10_000;
                (cycle_clock, sessions.clone())
            },
            |(now_ms, owned)| {
                let lease = claim(&cycle_db, &scope, now_ms);
                let snapshot = ListingCacheSnapshot::from_sessions(owned).unwrap();
                cycle_db
                    .publish_listing(&lease, snapshot, now_ms)
                    .expect("publish");
                black_box(cycle_db.read_listing_snapshot(&scope, now_ms).unwrap())
            },
            criterion::BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_listing_cache);
criterion_main!(benches);
