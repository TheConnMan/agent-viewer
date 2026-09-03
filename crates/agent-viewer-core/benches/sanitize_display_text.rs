//! Guards the per-tick sanitizer cost over a fleet-shaped title corpus.
//!
//! What this guards: `CodexBackend::list()` and `App::set_sessions` run
//! `sanitize_display_text` on every title (and list() also on every preview) each
//! 2 s tick. On this box that is ~11k strings averaging ~2 KB, ~20 MB per field.
//! The pre-change `chars().filter(is_display_safe).collect()` path spent ~105 ms
//! per field; the ASCII byte fast path must stay at least 5x faster on this mixed
//! corpus.
//!
//! Baseline measured on this box (2026-09-03, release build, 10,000 x 2 KiB):
//!
//! | bench | median |
//! | --- | --- |
//! | `prechange_chars` (frozen char-filter) | **101.96 ms** |
//! | `ascii_fast_path` | **12.56 ms** (**8.1x**) |
//! | `in_place` | **9.81 ms** |
//!
//! If `ascii_fast_path` ever converges on `prechange_chars`, the byte predicate has
//! been lost and every 2 s tick is back to walking Unicode `is_control` on ASCII
//! titles. Absolute times move ~2x under agent load; the ratio is the durable signal.
//!
//! The corpus is 10,000 strings of 2 KiB (~20 MB): 6,600 clean ASCII, 3,000 dirty
//! ASCII (C0/DEL sprinkled), 400 non-ASCII (bidi/format plus CJK). Mix matches the
//! live fleet shape (most titles ASCII, about a third dirty, a few percent
//! non-ASCII). Deterministic LCG seed. No network and no real session store.

use agent_viewer_core::{sanitize_display_text, sanitize_display_text_in_place};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

const STRINGS: usize = 10_000;
const STRING_BYTES: usize = 2 * 1024;
const CLEAN_ASCII: usize = 6_600;
const DIRTY_ASCII: usize = 3_000;
const NON_ASCII: usize = 400;

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

    fn next_byte(&mut self) -> u8 {
        (self.next_u64() & 0xff) as u8
    }
}

/// Frozen copy of the pre-change sanitizer. Keep this byte-identical to the
/// `chars().filter(is_display_safe).collect()` implementation this bench exists
/// to beat; do not "optimize" it in place or the 5x claim becomes a no-op.
fn sanitize_display_text_prechange(text: &str) -> String {
    text.chars()
        .filter(|character| is_display_safe_prechange(*character))
        .collect()
}

fn is_display_safe_prechange(character: char) -> bool {
    !character.is_control()
        && !matches!(
            character,
            '\u{00AD}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08E2}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
        )
}

const PRINTABLE: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 -_/.";
const CONTROLS: &[u8] = &[0x00, 0x01, 0x09, 0x0a, 0x0d, 0x1b, 0x7f];

fn fill_printable(rng: &mut Lcg, buf: &mut Vec<u8>, nbytes: usize) {
    for _ in 0..nbytes {
        buf.push(PRINTABLE[rng.next_byte() as usize % PRINTABLE.len()]);
    }
}

fn corpus() -> Vec<String> {
    let mut rng = Lcg::new(0x5341_4e49_5449_5a45);
    let mut out = Vec::with_capacity(STRINGS);
    for index in 0..STRINGS {
        let mut buf = Vec::with_capacity(STRING_BYTES + 16);
        if index < CLEAN_ASCII {
            fill_printable(&mut rng, &mut buf, STRING_BYTES);
        } else if index < CLEAN_ASCII + DIRTY_ASCII {
            fill_printable(&mut rng, &mut buf, STRING_BYTES);
            for offset in (0..STRING_BYTES).step_by(64) {
                buf[offset] = CONTROLS[rng.next_byte() as usize % CONTROLS.len()];
            }
        } else {
            fill_printable(&mut rng, &mut buf, STRING_BYTES.saturating_sub(24));
            buf.extend_from_slice("東京".as_bytes());
            buf.extend_from_slice("\u{202e}".as_bytes());
            buf.extend_from_slice("\u{200b}".as_bytes());
            buf.extend_from_slice("\u{00ad}".as_bytes());
            buf.extend_from_slice("Café".as_bytes());
            while buf.len() < STRING_BYTES {
                buf.push(PRINTABLE[rng.next_byte() as usize % PRINTABLE.len()]);
            }
            buf.truncate(STRING_BYTES);
            // Truncating mid-code-point would yield invalid UTF-8; keep a valid prefix.
            while std::str::from_utf8(&buf).is_err() {
                buf.pop();
            }
        }
        out.push(String::from_utf8(buf).expect("corpus bytes are valid UTF-8"));
    }
    debug_assert_eq!(out.len(), STRINGS);
    debug_assert_eq!(CLEAN_ASCII + DIRTY_ASCII + NON_ASCII, STRINGS);
    out
}

fn bench_sanitize(criterion: &mut Criterion) {
    let strings = corpus();
    let bytes: u64 = strings.iter().map(|text| text.len() as u64).sum();
    let mut group = criterion.benchmark_group("sanitize_display_text_20mb");
    group.throughput(Throughput::Bytes(bytes));
    group.bench_function("prechange_chars", |bencher| {
        bencher.iter(|| {
            let mut n = 0usize;
            for text in &strings {
                n += black_box(sanitize_display_text_prechange(text)).len();
            }
            black_box(n)
        })
    });
    group.bench_function("ascii_fast_path", |bencher| {
        bencher.iter(|| {
            let mut n = 0usize;
            for text in &strings {
                n += black_box(sanitize_display_text(text)).len();
            }
            black_box(n)
        })
    });
    group.bench_function("in_place", |bencher| {
        bencher.iter_batched(
            || strings.clone(),
            |mut owned| {
                for text in &mut owned {
                    sanitize_display_text_in_place(text);
                }
                black_box(owned)
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

criterion_group!(benches, bench_sanitize);
criterion_main!(benches);
