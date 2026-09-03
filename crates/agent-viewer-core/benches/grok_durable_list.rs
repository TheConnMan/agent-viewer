//! Guards the Grok listing cache: warm `list()` must not re-read every
//! `chat_history.jsonl` and `updates.jsonl` on an idle refresh tick.
//!
//! What this guards: `GrokLifecycle::list` previously opened and parsed every
//! session's chat history (up to 2 MiB each) and the updates tail on every 2 s
//! tick. The (mtime, len) cache should make a warm pass a pair of `stat`s per
//! session. If warm ever converges on cold, the keys have stopped matching.
//!
//! Baseline measured on this box (2026-09-03, release build, 84 sessions of
//! ~200 KB histories each, temp durable store, no leader):
//!
//! | bench | median |
//! | --- | --- |
//! | `list_cold_84` (fresh `GrokLifecycle`, every entry a miss) | **39.678 ms** |
//! | `list_warm_84` (cache hit on every session) | **0.983 ms** |
//!
//! The cache is worth **~40x** here. Absolute times move under agent load on
//! this box; the ratio is the durable signal.
//!
//! Deterministic (fixed filler), temp dir only, no Grok home and no leader.

use agent_viewer_core::GrokLifecycle;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::path::Path;

const SESSIONS: usize = 84;
const HISTORY_BYTES: usize = 200 * 1024;

fn write_line(writer: &mut BufWriter<std::fs::File>, line: &str, written: &mut usize) {
    writer.write_all(line.as_bytes()).expect("write line");
    writer.write_all(b"\n").expect("write newline");
    *written += line.len() + 1;
}

fn write_session(home: &Path, index: usize) {
    let session_id = format!("session-bench-{index:02}");
    let dir = home
        .join("sessions")
        .join(format!("project-{:02}", index % 12))
        .join(&session_id);
    std::fs::create_dir_all(&dir).expect("create bench session directory");
    let summary = format!(
        r#"{{"info":{{"id":"{session_id}","cwd":"/home/bench/project"}},"session_summary":"Bench session {index}","generated_title":"Bench {index}","created_at":"2026-08-20T01:02:03.004Z","updated_at":"2026-08-21T04:05:06.007Z","num_messages":2,"current_model_id":"grok-4.6"}}"#
    );
    std::fs::write(dir.join("summary.json"), summary).expect("write bench summary");

    let mut history =
        BufWriter::new(std::fs::File::create(dir.join("chat_history.jsonl")).expect("history"));
    let mut written = 0usize;
    let mut tick = 0usize;
    while written < HISTORY_BYTES {
        let line = format!(
            r#"{{"type":"assistant","content":"bench filler {index} tick {tick} resolver rollout session transcript backend snapshot listing worktree refresh status approval spawn registry viewer parser capability"}}"#
        );
        write_line(&mut history, &line, &mut written);
        tick += 1;
    }
    write_line(
        &mut history,
        r#"{"type":"assistant","content":"Creating the pull request.","tool_calls":[{"id":"call-create-1","name":"run_terminal_command","arguments":"{\"command\": \"gh pr create --title t --body b\", \"description\": \"Create GitHub pull request\"}"}]}"#,
        &mut written,
    );
    let result = format!(
        r#"{{"type":"tool_result","tool_call_id":"call-create-1","content":"exit: 0\nhttps://github.com/TheConnMan/agent-viewer/pull/{}"}}"#,
        300 + index
    );
    write_line(&mut history, &result, &mut written);
    history.flush().expect("flush history");

    let updates = format!(
        "{{\"timestamp\":1787330130,\"method\":\"session/update\",\"params\":{{\"sessionId\":\"unused\",\"update\":{{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{{\"type\":\"text\",\"text\":\"working\"}}}}}}}}\n{{\"timestamp\":1787330131,\"method\":\"_x.ai/session/update\",\"params\":{{\"sessionId\":\"{session_id}\",\"update\":{{\"sessionUpdate\":\"turn_completed\",\"prompt_id\":\"prompt-one\",\"stop_reason\":\"end_turn\"}}}}}}\n"
    );
    std::fs::write(dir.join("updates.jsonl"), updates).expect("write bench updates");
}

fn fixtures() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().expect("create bench tempdir");
    let home = dir.path().join("grok-home");
    for index in 0..SESSIONS {
        write_session(&home, index);
    }
    (dir, home)
}

fn bench_grok_durable_list(criterion: &mut Criterion) {
    let (_dir, home) = fixtures();

    let mut group = criterion.benchmark_group("grok_durable_list_84");
    group.sample_size(20);
    group.measurement_time(std::time::Duration::from_secs(10));

    group.bench_function("list_cold_84", |bencher| {
        bencher.iter(|| {
            let lifecycle = GrokLifecycle::new("missing-grok", &home);
            black_box(lifecycle.list().expect("cold durable list"))
        });
    });

    let lifecycle = GrokLifecycle::new("missing-grok", &home);
    let warmed = lifecycle.list().expect("warm durable list");
    assert_eq!(
        warmed.len(),
        SESSIONS,
        "bench store must list every session"
    );

    group.bench_function("list_warm_84", |bencher| {
        bencher.iter(|| black_box(lifecycle.list().expect("warm durable list")));
    });

    group.finish();
}

criterion_group!(benches, bench_grok_durable_list);
criterion_main!(benches);
