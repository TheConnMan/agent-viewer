//! The age ramp: an optional mode that fades a finished session's row toward the active
//! theme's `faint` token as the row gets older, so today's work pops and Tuesday's recedes.
//!
//! Both endpoints come from the theme. There are no literal colors here: `ramp` takes two
//! tokens and returns a point between them.

use ratatui::style::Color;

/// Age at which a finished row has fully receded to `faint`. PUNTED asks for a fade "over
/// hours" that leaves today's work popping and Tuesday's receded; 24h is the reading that
/// makes both true, so anything from yesterday or earlier sits at the faint endpoint.
const HORIZON_MS: i64 = 24 * 60 * 60 * 1_000;

/// Linear sRGB interpolation from `start` to `end`.
///
/// **The truecolor gate lives here.** If either endpoint is not a `Color::Rgb` (the `terminal`
/// theme builds itself from named ANSI colors), there is nowhere to interpolate to and the
/// ramp would degrade to noise, so `start` comes back untouched at every progress value.
pub fn ramp(start: Color, end: Color, progress: f32) -> Color {
    let (Color::Rgb(start_red, start_green, start_blue), Color::Rgb(end_red, end_green, end_blue)) =
        (start, end)
    else {
        return start;
    };
    // NaN fails both comparisons and falls through to 0.0, which is the no-op end.
    let progress = if progress > 1.0 {
        1.0
    } else if progress > 0.0 {
        progress
    } else {
        0.0
    };
    if progress == 0.0 {
        return start;
    }
    if progress == 1.0 {
        return end;
    }
    Color::Rgb(
        channel(start_red, end_red, progress),
        channel(start_green, end_green, progress),
        channel(start_blue, end_blue, progress),
    )
}

/// One channel of the interpolation, rounding half away from zero (`f32::round`).
fn channel(start: u8, end: u8, progress: f32) -> u8 {
    let start = f32::from(start);
    (start + (f32::from(end) - start) * progress).round() as u8
}

/// How far along the ramp a row of this age sits. Clamped at both ends: a row stamped in the
/// future (clock skew) reads as brand new rather than wrapping.
pub fn age_progress(age_ms: i64) -> f32 {
    if age_ms <= 0 {
        return 0.0;
    }
    if age_ms >= HORIZON_MS {
        return 1.0;
    }
    age_ms as f32 / HORIZON_MS as f32
}

#[cfg(test)]
mod tests {
    use super::{HORIZON_MS, age_progress, ramp};
    use crate::ui::theme::{amber, terminal};
    use ratatui::style::Color;

    #[test]
    fn endpoints_are_exact_theme_tokens() {
        let theme = amber(false);
        assert_eq!(ramp(theme.text, theme.faint, 0.0), theme.text);
        assert_eq!(ramp(theme.text, theme.faint, 1.0), theme.faint);
    }

    #[test]
    fn documented_midpoint_between_amber_text_and_faint() {
        let theme = amber(false);
        // amber text #e6dfcc -> amber faint #5c563f at 0.5, rounding half away from zero:
        //   red   230 + (92  - 230) * 0.5 = 161.0 -> 161 = 0xa1
        //   green 223 + (86  - 223) * 0.5 = 154.5 -> 155 = 0x9b
        //   blue  204 + (63  - 204) * 0.5 = 133.5 -> 134 = 0x86
        assert_eq!(
            ramp(theme.text, theme.faint, 0.5),
            Color::Rgb(0xa1, 0x9b, 0x86)
        );
    }

    #[test]
    fn quarter_ramp_moves_every_channel_toward_faint() {
        let theme = amber(false);
        // 230 + (92 - 230) * 0.25 = 195.5 -> 196; 223 - 34.25 = 188.75 -> 189;
        // 204 - 35.25 = 168.75 -> 169.
        assert_eq!(
            ramp(theme.text, theme.faint, 0.25),
            Color::Rgb(0xc4, 0xbd, 0xa9)
        );
    }

    #[test]
    fn non_truecolor_theme_never_ramps_at_any_age() {
        let theme = terminal(false);
        // The `terminal` theme is built from named ANSI colors, so there is no endpoint to
        // interpolate toward. Every sampled age must hand back the unramped start token.
        for age_ms in [
            0,
            HORIZON_MS / 4,
            HORIZON_MS / 2,
            HORIZON_MS,
            HORIZON_MS * 7,
        ] {
            let progress = age_progress(age_ms);
            assert_eq!(
                ramp(theme.text, theme.faint, progress),
                theme.text,
                "terminal theme ramped at age {age_ms}ms"
            );
            assert_eq!(
                ramp(theme.ok, theme.faint, progress),
                theme.ok,
                "terminal theme ramped the ok token at age {age_ms}ms"
            );
        }
    }

    #[test]
    fn progress_clamps_at_both_ends() {
        assert_eq!(age_progress(-HORIZON_MS), 0.0);
        assert_eq!(age_progress(0), 0.0);
        assert_eq!(age_progress(HORIZON_MS), 1.0);
        assert_eq!(age_progress(HORIZON_MS * 30), 1.0);
        assert_eq!(age_progress(HORIZON_MS / 2), 0.5);
    }

    #[test]
    fn out_of_range_progress_lands_on_the_endpoints_rather_than_wrapping() {
        let theme = amber(false);
        assert_eq!(ramp(theme.text, theme.faint, -4.0), theme.text);
        assert_eq!(ramp(theme.text, theme.faint, 9.0), theme.faint);
        assert_eq!(ramp(theme.text, theme.faint, f32::NAN), theme.text);
    }
}
