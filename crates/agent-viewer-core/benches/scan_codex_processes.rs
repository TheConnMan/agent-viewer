//! Guards the per-tick Codex process scan used by status detection.
//!
//! `scan_codex_processes` runs once per 2 s refresh tick. This bench is impure
//! (it reads live /proc) but the gap is stable. Measured on this box
//! (2026-09-03, release Criterion, `live_box`):
//!
//! | impl | median |
//! | --- | --- |
//! | sysinfo `refresh_processes_specifics` with cmd | **26.26 ms** |
//! | direct `/proc` walk (comm for every pid, cmdline and fd/* only for comm starting with `codex`) | **1.32 ms** |
//!
//! About **20x**. Absolute times move with pid count and load; if this row
//! ever climbs back toward tens of milliseconds, the scan is doing all-process
//! work again.

use agent_viewer_core::codex::status::scan_codex_processes;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn bench_scan(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("scan_codex_processes");
    group.sample_size(40);
    group.measurement_time(std::time::Duration::from_secs(8));
    group.bench_function("live_box", |bencher| {
        bencher.iter(|| black_box(scan_codex_processes()));
    });
    group.finish();
}

criterion_group!(benches, bench_scan);
criterion_main!(benches);
