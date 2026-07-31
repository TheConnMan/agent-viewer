//! Guards the per-tick transcript parsing cost for a large, actively-written session.
//!
//! What this guards: the refresh loop touches a session's JSONL on every 2s tick. Two shapes
//! of read exist and they are three orders of magnitude apart, so a regression that silently
//! swaps one for the other is invisible except here.
//!
//! Baseline measured on this box (2026-07-31, release build, 20 MB fixtures):
//!
//! | bench | median |
//! | --- | --- |
//! | `codex_tail_state_20mb` (final 64 KiB only) | **142 us** |
//! | `codex_pending_approval_20mb` (final 64 KiB only) | **117 us** |
//! | `codex_read_transcript_20mb` (whole file) | **42 ms** (472 MiB/s) |
//! | `claude_read_transcript_20mb` (whole file) | **29 ms** (683 MiB/s) |
//!
//! Absolute times move ~2x run to run when this box is under agent load; the **~300x** gap
//! between the two groups is what to actually guard, and it is stable.
//!
//! The tail-window reads are flat in file size by construction. If either ever starts
//! scaling with the fixture, the 64 KiB window has been lost and the status resolver is
//! reading whole rollouts on every tick.
//!
//! The full parses are linear in file size and must stay OFF the tick path - they are
//! peek-pane reads. A regression here is either a slower JSON path or, far worse, a caller
//! that started invoking them per tick: the reviewed defect was an 18.6 MB transcript fully
//! re-parsed every 2 s while a session worked, which at this baseline is ~30-40 ms of the
//! 2 s budget burned per working session, per tick.
//!
//! All fixtures are generated into a temp dir from a fixed LCG seed. No network, no real
//! `~/.codex` or `~/.claude` access.

use agent_viewer_core::claude::read_claude_transcript;
use agent_viewer_core::codex::rollout::{pending_approval, read_transcript, tail_state};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::fs::File;
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Synthetic transcript size, matched to the scale the reviews measured on this box (an
/// 18.6 MB Claude transcript).
const TARGET_BYTES: usize = 20 * 1024 * 1024;

/// Deterministic PRNG so every run generates byte-identical fixtures.
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

/// JSON-safe filler words (no quotes, no backslashes) so generated lines need no escaping.
const WORDS: [&str; 24] = [
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
    "daemon",
    "attach",
    "interrupt",
    "generation",
    "lease",
    "fixture",
    "baseline",
    "regression",
];

/// `words` filler tokens, joined by spaces.
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

/// A variable-length paragraph, drawn then generated (two statements so the rng is borrowed
/// once at a time).
fn paragraph(rng: &mut Lcg, low: usize, high: usize) -> String {
    let words = rng.range(low, high);
    prose(rng, words)
}

/// A valid RFC 3339 UTC stamp that advances without pulling in a date library.
fn stamp(tick: usize) -> String {
    let minute = (tick / 60) % 60;
    let second = tick % 60;
    format!("2026-07-10T21:{minute:02}:{second:02}.000Z")
}

fn emit(writer: &mut BufWriter<File>, line: &str, written: &mut usize) {
    writer.write_all(line.as_bytes()).expect("write line");
    writer.write_all(b"\n").expect("write newline");
    *written += line.len() + 1;
}

/// A realistic Codex rollout JSONL: a fat `session_meta` first line, then a repeating mix of
/// user/assistant messages, function calls, `token_count` noise, and the `developer`
/// scaffolding the parser skips, terminated by a `task_complete` so the tail reads Complete.
fn write_codex_rollout(path: &Path, target: usize) -> usize {
    let mut writer = BufWriter::new(File::create(path).expect("create codex rollout fixture"));
    let mut rng = Lcg::new(0x5EED_C0DE);
    let mut written = 0usize;

    // The real first line carries the whole base_instructions blob; it is why
    // `read_session_meta` must not cap its read.
    let instructions =
        "You are a helpful coding agent operating in a sandboxed workspace. ".repeat(180);
    let meta = format!(
        r#"{{"timestamp":"{}","type":"session_meta","payload":{{"session_id":"019f4dda-fecb-7b71-adba-9fda570e4cdb","id":"019f4dda-fecb-7b71-adba-9fda570e4cdb","cwd":"/home/bench/projects/agent-viewer","originator":"codex_exec","cli_version":"0.144.1","source":"exec","base_instructions":"{instructions}"}}}}"#,
        stamp(0)
    );
    emit(&mut writer, &meta, &mut written);

    let mut tick = 1usize;
    while written < target {
        let time = stamp(tick);

        let user = paragraph(&mut rng, 12, 90);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"response_item","payload":{{"type":"message","id":"user_{tick}","role":"user","content":[{{"type":"input_text","text":"{user}"}}]}}}}"#
            ),
            &mut written,
        );

        let assistant = paragraph(&mut rng, 40, 400);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"response_item","payload":{{"type":"message","id":"assistant_{tick}","role":"assistant","content":[{{"type":"output_text","text":"{assistant}"}}]}}}}"#
            ),
            &mut written,
        );

        let command = prose(&mut rng, 6);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"response_item","payload":{{"type":"function_call","id":"fc_{tick}","name":"exec_command","arguments":"{{\"cmd\": \"{command}\"}}","call_id":"call_{tick}"}}}}"#
            ),
            &mut written,
        );

        // Harness scaffolding the parser must skip - a real rollout is roughly half this.
        let developer = paragraph(&mut rng, 30, 200);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"response_item","payload":{{"type":"message","id":"dev_{tick}","role":"developer","content":[{{"type":"input_text","text":"{developer}"}}]}}}}"#
            ),
            &mut written,
        );

        let tokens = tick * 137;
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_tokens":{tokens}}},"rate_limits":null}}}}"#
            ),
            &mut written,
        );

        if tick.is_multiple_of(24) {
            emit(
                &mut writer,
                &format!(
                    r#"{{"timestamp":"{time}","type":"event_msg","payload":{{"type":"task_started","turn_id":"turn_{tick}"}}}}"#
                ),
                &mut written,
            );
        }
        tick += 1;
    }

    let complete = format!(
        r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn_final","last_agent_message":"DONE","completed_at":1783716494,"duration_ms":5000}}}}"#,
        stamp(tick)
    );
    emit(&mut writer, &complete, &mut written);
    writer.flush().expect("flush codex rollout fixture");
    written
}

/// A realistic Claude session JSONL: string-content user turns, block-list assistant turns
/// carrying thinking/text/tool_use, plus the tool_result and attachment records the parser
/// skips.
fn write_claude_transcript(path: &Path, target: usize) -> usize {
    let mut writer = BufWriter::new(File::create(path).expect("create claude transcript fixture"));
    let mut rng = Lcg::new(0xC1A0_DE01);
    let mut written = 0usize;

    emit(
        &mut writer,
        r#"{"type":"system","subtype":"init","content":"session initialized"}"#,
        &mut written,
    );

    let mut tick = 1usize;
    while written < target {
        let time = stamp(tick);

        let user = paragraph(&mut rng, 12, 90);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"user","message":{{"role":"user","content":"{user}"}}}}"#
            ),
            &mut written,
        );

        let thinking = paragraph(&mut rng, 40, 300);
        let text = paragraph(&mut rng, 40, 300);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"assistant","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"{thinking}"}},{{"type":"text","text":"{text}"}}]}}}}"#
            ),
            &mut written,
        );

        let preamble = paragraph(&mut rng, 10, 60);
        let command = prose(&mut rng, 8);
        let description = prose(&mut rng, 6);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{preamble}"}},{{"type":"tool_use","name":"Bash","input":{{"command":"{command}","description":"{description}"}}}}]}}}}"#
            ),
            &mut written,
        );

        let result = paragraph(&mut rng, 40, 400);
        emit(
            &mut writer,
            &format!(
                r#"{{"timestamp":"{time}","type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_{tick}","content":"{result}"}}]}}}}"#
            ),
            &mut written,
        );

        emit(
            &mut writer,
            &format!(r#"{{"type":"attachment","path":"/tmp/pasted_{tick}.png"}}"#),
            &mut written,
        );
        tick += 1;
    }
    writer.flush().expect("flush claude transcript fixture");
    written
}

struct Fixtures {
    _dir: tempfile::TempDir,
    codex: PathBuf,
    codex_bytes: usize,
    claude: PathBuf,
    claude_bytes: usize,
}

fn fixtures() -> Fixtures {
    let dir = tempfile::TempDir::new().expect("create bench tempdir");
    let codex = dir.path().join("rollout-bench.jsonl");
    let claude = dir.path().join("claude-bench.jsonl");
    let codex_bytes = write_codex_rollout(&codex, TARGET_BYTES);
    let claude_bytes = write_claude_transcript(&claude, TARGET_BYTES);
    Fixtures {
        _dir: dir,
        codex,
        codex_bytes,
        claude,
        claude_bytes,
    }
}

fn bench_transcripts(criterion: &mut Criterion) {
    let fixtures = fixtures();

    // Tail-window reads: flat in file size by construction, so this group is cheap.
    let mut tail = criterion.benchmark_group("transcript_tail_window");
    tail.sample_size(50);
    tail.measurement_time(std::time::Duration::from_secs(6));
    tail.bench_function("codex_tail_state_20mb", |bencher| {
        bencher.iter(|| black_box(tail_state(black_box(&fixtures.codex)).unwrap()));
    });
    tail.bench_function("codex_pending_approval_20mb", |bencher| {
        bencher.iter(|| black_box(pending_approval(black_box(&fixtures.codex)).unwrap()));
    });
    tail.finish();

    // Full-file parses: linear in file size, so minimum samples keep the wall clock sane.
    let mut full = criterion.benchmark_group("transcript_full_parse");
    full.sample_size(10);
    full.measurement_time(std::time::Duration::from_secs(12));
    full.throughput(Throughput::Bytes(fixtures.codex_bytes as u64));
    full.bench_function("codex_read_transcript_20mb", |bencher| {
        bencher.iter(|| black_box(read_transcript(black_box(&fixtures.codex)).unwrap()));
    });
    full.throughput(Throughput::Bytes(fixtures.claude_bytes as u64));
    full.bench_function("claude_read_transcript_20mb", |bencher| {
        bencher
            .iter(|| black_box(read_claude_transcript(black_box(&fixtures.claude), 200).unwrap()));
    });
    full.finish();
}

criterion_group!(benches, bench_transcripts);
criterion_main!(benches);
