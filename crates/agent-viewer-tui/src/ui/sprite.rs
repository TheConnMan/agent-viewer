use super::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

pub(super) const WIDTH: u16 = 11;
pub(super) const HEIGHT: u16 = 6;

const PIXEL_ROWS: [&[u8; WIDTH as usize]; HEIGHT as usize * 2] = [
    b"...........",
    b"...........",
    b"yyyy.L.yyyy",
    b".....S.....",
    b".....T.....",
    b".....r.....",
    b".....T.....",
    b".....r.....",
    b".....T.....",
    b"....TrT....",
    b"....BBB....",
    b"...BBBBB...",
];

const PERIOD_MS: i64 = 2_800;
const BEAM_STEP_MS: i64 = 160;

pub(super) struct Lighthouse<'a> {
    theme: &'a Theme,
    lamp: Color,
    now_ms: i64,
}

impl<'a> Lighthouse<'a> {
    pub(super) fn new(theme: &'a Theme, lamp: Color, now_ms: i64) -> Self {
        Self {
            theme,
            lamp,
            now_ms,
        }
    }

    fn pixel_color(&self, pixel: u8, x: u16) -> Color {
        match pixel {
            b'.' => self.theme.bg,
            b'L' => self.animated_lamp(128, 0),
            b'y' => {
                let distance = x.abs_diff(WIDTH / 2);
                let delay = i64::from(distance.saturating_sub(2)) * BEAM_STEP_MS;
                self.animated_lamp(0, delay)
            }
            b'S' => self.theme.muted,
            b'T' => self.theme.text,
            b'r' => self.theme.err,
            b'B' => self.theme.faint,
            _ => self.theme.bg,
        }
    }

    fn animated_lamp(&self, minimum: u8, delay_ms: i64) -> Color {
        let lamp = if self.theme.id == "mono16" {
            self.theme.selfg
        } else {
            self.lamp
        };
        if !self.theme.animation {
            return lamp;
        }
        blend(
            lamp,
            self.theme.bg,
            intensity(self.now_ms, delay_ms, minimum),
        )
    }
}

impl Widget for Lighthouse<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let height = area.height.min(HEIGHT);
        let width = area.width.min(WIDTH);
        for row in 0..height {
            let top = PIXEL_ROWS[usize::from(row) * 2];
            let bottom = PIXEL_ROWS[usize::from(row) * 2 + 1];
            for column in 0..width {
                buffer[(area.x + column, area.y + row)]
                    .set_char('▀')
                    .set_fg(self.pixel_color(top[usize::from(column)], column))
                    .set_bg(self.pixel_color(bottom[usize::from(column)], column));
            }
        }
    }
}

fn intensity(now_ms: i64, delay_ms: i64, minimum: u8) -> u8 {
    let phase = (now_ms - delay_ms).rem_euclid(PERIOD_MS);
    let half = PERIOD_MS / 2;
    let rising = if phase <= half {
        phase
    } else {
        PERIOD_MS - phase
    };
    let range = i64::from(u8::MAX - minimum);
    minimum + ((range * rising + half / 2) / half) as u8
}

fn blend(source: Color, background: Color, strength: u8) -> Color {
    let (Color::Rgb(sr, sg, sb), Color::Rgb(br, bg, bb)) = (source, background) else {
        return source;
    };
    Color::Rgb(
        mix_channel(sr, br, strength),
        mix_channel(sg, bg, strength),
        mix_channel(sb, bb, strength),
    )
}

fn mix_channel(source: u8, background: u8, strength: u8) -> u8 {
    let source = u32::from(source);
    let background = u32::from(background);
    let strength = u32::from(strength);
    ((source * strength + background * (255 - strength) + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme;

    fn rendered(theme: &Theme, needs_input: bool, now_ms: i64) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH + 2, HEIGHT + 2));
        let lamp = if needs_input {
            theme.warn
        } else {
            theme.accent
        };
        Lighthouse::new(theme, lamp, now_ms).render(Rect::new(1, 1, WIDTH, HEIGHT), &mut buffer);
        buffer
    }

    #[test]
    fn bitmap_renders_exactly_six_rows_of_eleven_cells() {
        let theme = theme::mono16(false);
        let buffer = rendered(&theme, false, 0);
        let blocks = buffer
            .content
            .iter()
            .filter(|cell| cell.symbol() == "▀")
            .count();

        assert_eq!((WIDTH, HEIGHT), (11, 6));
        assert_eq!(blocks, 66);
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(WIDTH + 1, HEIGHT + 1)].symbol(), " ");
    }

    #[test]
    fn upper_and_lower_pixels_set_foreground_and_background() {
        let mut theme = theme::amber(false);
        theme.animation = false;
        let buffer = rendered(&theme, false, 0);
        let lamp_over_shutter = &buffer[(6, 2)];

        assert_eq!(lamp_over_shutter.fg, theme.accent);
        assert_eq!(lamp_over_shutter.bg, theme.muted);
    }

    #[test]
    fn disabled_animation_is_identical_at_different_phases() {
        let mut theme = theme::amber(false);
        theme.animation = false;

        assert_eq!(rendered(&theme, false, 0), rendered(&theme, false, 1_400));
    }

    #[test]
    fn lamp_pulses_and_beam_phase_moves_outward() {
        let theme = theme::amber(false);
        let dim = rendered(&theme, false, 0);
        let bright = rendered(&theme, false, 1_400);
        assert_ne!(dim[(6, 2)].fg, bright[(6, 2)].fg);

        let traveling = rendered(&theme, false, 480);
        assert_ne!(traveling[(4, 2)].fg, traveling[(1, 2)].fg);
    }
}
