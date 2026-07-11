//! Pure peek-panel model: turns the focused session's last message (plus optional pending
//! ask / error / metadata fallback) into a flat list of width-wrapped, kind-tagged lines the
//! renderer maps straight to styled rows. No ratatui types here so the whole thing is unit-
//! tested. The `ask` argument is a hook for Part B (pending-ask); callers pass None for now.

use agent_viewer_core::codex::rollout::TranscriptItem;

/// The role each peek line plays, so the renderer can color it without re-parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeekKind {
    Ask,
    Role,
    Body,
    Meta,
    Error,
}

/// One rendered peek line: its text and what it represents.
#[derive(Debug, Clone, PartialEq)]
pub struct PeekLine {
    pub text: String,
    pub kind: PeekKind,
}

/// Hard cap on peek height (a preview, not a scrollback); overflow ends in a "…" Meta line.
const MAX_PEEK_LINES: usize = 16;
/// How many prior items render as one-line Role context above the focused body.
const TAIL_ITEMS: usize = 3;

/// Word-wrap `text` to `width` columns. Explicit '\n' always breaks (a blank source line
/// yields one empty output line, preserving paragraph breaks); each source line is then
/// greedy-wrapped on ASCII whitespace, and any single word longer than `width` is hard-split
/// into width-sized chunks. Width is measured by `char` count. `width == 0` is a no-op guard.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    for source_line in text.split('\n') {
        wrap_one(source_line, width, &mut out);
    }
    out
}

/// Greedy-wrap a single (newline-free) source line into `out`. Always emits at least one line
/// (an empty string for an empty source line, preserving blank paragraph breaks).
fn wrap_one(line: &str, width: usize, out: &mut Vec<String>) {
    let start_len = out.len();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in line.split_ascii_whitespace() {
        let word_w = word.chars().count();
        if word_w > width {
            // Flush the pending line, then hard-split the overlong word into chunks.
            if current_w > 0 {
                out.push(std::mem::take(&mut current));
                current_w = 0;
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                chunk.push(ch);
                if chunk.chars().count() == width {
                    out.push(std::mem::take(&mut chunk));
                }
            }
            if !chunk.is_empty() {
                current = chunk;
                current_w = current.chars().count();
            }
            continue;
        }
        // +1 for the joining space when the line is non-empty.
        let extra = if current_w == 0 { word_w } else { word_w + 1 };
        if current_w + extra > width {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
            current_w = word_w;
        } else {
            if current_w > 0 {
                current.push(' ');
            }
            current.push_str(word);
            current_w += extra;
        }
    }
    // Emit the trailing line; if this source line produced nothing (blank/all-whitespace),
    // still emit one empty line so it yields >=1 output line (preserves paragraph breaks).
    if current_w > 0 || out.len() == start_len {
        out.push(current);
    }
}

/// Char-truncate `s` to `width` (no ellipsis — the tail one-liners just clip).
fn truncate(s: &str, width: usize) -> String {
    if width == 0 || s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width).collect()
}

/// The first non-empty source line of `text` with inner newlines collapsed to spaces — the
/// one-line form for a prior (tail) item.
fn first_line_collapsed(text: &str) -> String {
    text.replace('\n', " ")
}

/// Build the peek panel lines for the focused session. Priority: an `error` alone; else an
/// optional pending `ask`, then the transcript tail (up to TAIL_ITEMS one-line Role items
/// above the focused body, whose full text is word-wrapped); when there are no items, the
/// `meta_fallback` lines (opencode-style status/cwd) or a placeholder. Capped to
/// MAX_PEEK_LINES with a trailing "…".
pub fn build(
    ask: Option<&str>,
    items: &[TranscriptItem],
    error: Option<&str>,
    meta_fallback: &[String],
    width: usize,
) -> Vec<PeekLine> {
    if let Some(err) = error {
        let mut lines: Vec<PeekLine> = wrap(err, width)
            .into_iter()
            .map(|text| PeekLine {
                text,
                kind: PeekKind::Error,
            })
            .collect();
        cap(&mut lines);
        return lines;
    }

    let mut lines: Vec<PeekLine> = Vec::new();

    if let Some(ask) = ask
        && !ask.is_empty()
    {
        for text in wrap(ask, width) {
            lines.push(PeekLine {
                text,
                kind: PeekKind::Ask,
            });
        }
    }

    if !items.is_empty() {
        // Up to TAIL_ITEMS one-line context items plus the focused body (the last item).
        let shown = &items[items.len().saturating_sub(TAIL_ITEMS + 1)..];
        let body = shown.last().expect("shown is non-empty");
        let tail = &shown[..shown.len() - 1];
        for item in tail {
            let text = truncate(
                &format!("{}: {}", item.role, first_line_collapsed(&item.text)),
                width,
            );
            lines.push(PeekLine {
                text,
                kind: PeekKind::Role,
            });
        }
        lines.push(PeekLine {
            text: format!("{}:", body.role),
            kind: PeekKind::Role,
        });
        for text in wrap(&body.text, width) {
            lines.push(PeekLine {
                text,
                kind: PeekKind::Body,
            });
        }
    } else if !meta_fallback.is_empty() {
        for line in meta_fallback {
            for text in wrap(line, width) {
                lines.push(PeekLine {
                    text,
                    kind: PeekKind::Meta,
                });
            }
        }
    } else {
        lines.push(PeekLine {
            text: "(no transcript yet)".to_string(),
            kind: PeekKind::Meta,
        });
    }

    cap(&mut lines);
    lines
}

/// Truncate to MAX_PEEK_LINES, replacing the overflow with a single "…" Meta line.
fn cap(lines: &mut Vec<PeekLine>) {
    if lines.len() > MAX_PEEK_LINES {
        lines.truncate(MAX_PEEK_LINES - 1);
        lines.push(PeekLine {
            text: "…".to_string(),
            kind: PeekKind::Meta,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(role: &str, text: &str) -> TranscriptItem {
        TranscriptItem {
            role: role.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn wrap_breaks_a_long_line_into_width_bounded_lines() {
        let text = "the quick brown fox jumps over the lazy dog";
        let lines = wrap(text, 10);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
        // No word is lost: rejoining recovers the words.
        let joined = lines.join(" ");
        assert!(joined.contains("quick"));
        assert!(joined.contains("dog"));
    }

    #[test]
    fn wrap_preserves_explicit_newlines_and_blank_lines() {
        let lines = wrap("alpha\n\nbeta", 20);
        assert_eq!(lines, vec!["alpha", "", "beta"]);
    }

    #[test]
    fn wrap_hard_splits_an_overlong_word() {
        let lines = wrap("supercalifragilistic", 5);
        assert!(lines.iter().all(|l| l.chars().count() <= 5));
        assert_eq!(lines.concat(), "supercalifragilistic");
    }

    #[test]
    fn wrap_width_zero_is_a_no_op_guard() {
        assert_eq!(wrap("anything at all", 0), vec!["anything at all"]);
    }

    #[test]
    fn build_body_wraps_and_keeps_last_item_text_untruncated() {
        // A multi-line body whose LAST word must survive in some Body line.
        let body = "one two three four five six seven eight nine zebra";
        let lines = build(None, &[item("assistant", body)], None, &[], 12);
        // A Role prefix line then multiple Body lines.
        assert_eq!(lines[0].kind, PeekKind::Role);
        assert_eq!(lines[0].text, "assistant:");
        let body_lines: Vec<_> = lines.iter().filter(|l| l.kind == PeekKind::Body).collect();
        assert!(body_lines.len() > 1);
        assert!(body_lines.iter().any(|l| l.text.contains("zebra")));
    }

    #[test]
    fn build_shows_prior_items_as_role_lines() {
        let items = [
            item("user", "please summarize the report"),
            item("assistant", "here is the summary of everything"),
        ];
        let lines = build(None, &items, None, &[], 40);
        // First a one-line Role for the user item, then the assistant body block.
        assert_eq!(lines[0].kind, PeekKind::Role);
        assert!(lines[0].text.starts_with("user: "));
        assert!(lines.iter().any(|l| l.kind == PeekKind::Body));
    }

    #[test]
    fn build_empty_items_uses_meta_fallback() {
        let meta = vec!["status: working".to_string(), "cwd: /home/user".to_string()];
        let lines = build(None, &[], None, &meta, 40);
        assert!(lines.iter().all(|l| l.kind == PeekKind::Meta));
        assert!(lines.iter().any(|l| l.text == "status: working"));
    }

    #[test]
    fn build_empty_items_and_empty_meta_is_placeholder() {
        let lines = build(None, &[], None, &[], 40);
        assert_eq!(
            lines,
            vec![PeekLine {
                text: "(no transcript yet)".to_string(),
                kind: PeekKind::Meta,
            }]
        );
    }

    #[test]
    fn build_error_yields_error_lines() {
        let lines = build(None, &[item("user", "hi")], Some("transcript unavailable"), &[], 40);
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|l| l.kind == PeekKind::Error));
    }

    #[test]
    fn build_ask_is_first_line() {
        let lines = build(Some("Approve the migration?"), &[item("assistant", "done")], None, &[], 40);
        assert_eq!(lines[0].kind, PeekKind::Ask);
        assert!(lines[0].text.contains("Approve"));
    }

    #[test]
    fn build_caps_at_max_lines_with_trailing_ellipsis() {
        // A body that word-wraps into far more than MAX_PEEK_LINES lines.
        let body = (0..100).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ");
        let lines = build(None, &[item("assistant", &body)], None, &[], 6);
        assert_eq!(lines.len(), MAX_PEEK_LINES);
        assert_eq!(lines.last().unwrap().text, "…");
        assert_eq!(lines.last().unwrap().kind, PeekKind::Meta);
    }
}
