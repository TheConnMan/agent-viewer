use super::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;
use std::f32::consts::TAU;

pub(super) const WIDTH: u16 = 11;
pub(super) const HEIGHT: u16 = 6;

/// Pixel rows per sprite: two stacked pixels share one terminal row via the upper half block.
const ROWS: usize = HEIGHT as usize * 2;
const COLS: usize = WIDTH as usize;

type Grid = [[u8; COLS]; ROWS];

/// Which header mascot is drawn. Cycled live with Ctrl+G so the candidates can be compared in
/// one build; all of them occupy the same 11x6 cell box, so the header layout never changes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpriteKind {
    #[default]
    Lighthouse,
    Constellation,
    Turbine,
}

impl SpriteKind {
    pub fn next(self) -> Self {
        match self {
            SpriteKind::Lighthouse => SpriteKind::Constellation,
            SpriteKind::Constellation => SpriteKind::Turbine,
            SpriteKind::Turbine => SpriteKind::Lighthouse,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SpriteKind::Lighthouse => "lighthouse",
            SpriteKind::Constellation => "constellation",
            SpriteKind::Turbine => "turbine",
        }
    }
}

/// The fleet state a sprite is allowed to encode. Counts only, so this module stays free of
/// `App`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Fleet {
    pub needs_input: usize,
    pub working: usize,
    pub done: usize,
}

// --- Lighthouse -----------------------------------------------------------------

const LIGHTHOUSE_PIXELS: Grid = [
    *b"...........",
    *b"...........",
    *b"yyyy.L.yyyy",
    *b".....S.....",
    *b".....T.....",
    *b".....r.....",
    *b".....T.....",
    *b".....r.....",
    *b".....T.....",
    *b"....TrT....",
    *b"....BBB....",
    *b"...BBBBB...",
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
            intensity(self.now_ms, delay_ms, minimum, PERIOD_MS),
        )
    }
}

impl Widget for Lighthouse<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        paint(area, buffer, &LIGHTHOUSE_PIXELS, |pixel, x| {
            self.pixel_color(pixel, x)
        });
    }
}

// --- Constellation --------------------------------------------------------------

/// A fixed star field: one slot per digit, claimed in index order by the fleet. The shape reads
/// as a constellation whether or not every slot is claimed, because dark slots still render
/// faint rather than vanishing.
const CONSTELLATION_PIXELS: Grid = [
    *b"...........",
    *b"...........",
    *b"..........6",
    *b"....2......",
    *b".1......5..",
    *b"...........",
    *b"......4....",
    *b"0..........",
    *b"...3.....8.",
    *b"...........",
    *b".......7...",
    *b"...........",
];

const STARS: usize = 9;
const TWINKLE_STEP_MS: i64 = 311;

/// What a star slot is carrying, in the order slots are handed out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    NeedsInput,
    Working,
    Done,
    Dark,
}

pub(super) struct Constellation<'a> {
    theme: &'a Theme,
    fleet: Fleet,
    now_ms: i64,
}

impl<'a> Constellation<'a> {
    pub(super) fn new(theme: &'a Theme, fleet: Fleet, now_ms: i64) -> Self {
        Self {
            theme,
            fleet,
            now_ms,
        }
    }

    /// Needs-input sessions claim the first slots, then working, then completed. A fleet larger
    /// than the field simply fills it.
    fn slot(&self, index: usize) -> Slot {
        let alert = self.fleet.needs_input;
        let busy = alert + self.fleet.working;
        let all = busy + self.fleet.done;
        if index < alert {
            Slot::NeedsInput
        } else if index < busy {
            Slot::Working
        } else if index < all {
            Slot::Done
        } else {
            Slot::Dark
        }
    }

    fn star_color(&self, index: usize) -> Color {
        debug_assert!(index < STARS, "star {index} is off the field");
        let (base, minimum, period) = match self.slot(index) {
            Slot::NeedsInput => (self.theme.warn, 40, 900),
            Slot::Working => (self.theme.accent, 110, 2_600),
            Slot::Done => (self.theme.muted, 150, 3_400),
            Slot::Dark => return self.theme.faint,
        };
        if !self.theme.animation {
            return base;
        }
        blend(
            base,
            self.theme.bg,
            intensity(self.now_ms, index as i64 * TWINKLE_STEP_MS, minimum, period),
        )
    }
}

impl Widget for Constellation<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        paint(area, buffer, &CONSTELLATION_PIXELS, |pixel, _| match pixel {
            b'0'..=b'8' => self.star_color(usize::from(pixel - b'0')),
            _ => self.theme.bg,
        });
    }
}

// --- Turbine --------------------------------------------------------------------

const HUB_X: i32 = 5;
const HUB_Y: i32 = 4;
const BLADE_LEN: i32 = 4;
const BLADES: i32 = 3;
/// One blade straight up: the pose a stopped rotor parks in.
const PARKED_ANGLE: f32 = -TAU / 4.0;
const FASTEST_REVOLUTION_MS: i64 = 800;
const BASE_REVOLUTION_MS: i64 = 2_600;

pub(super) struct Turbine<'a> {
    theme: &'a Theme,
    lamp: Color,
    working: usize,
    now_ms: i64,
}

impl<'a> Turbine<'a> {
    pub(super) fn new(theme: &'a Theme, lamp: Color, working: usize, now_ms: i64) -> Self {
        Self {
            theme,
            lamp,
            working,
            now_ms,
        }
    }

    fn angle(&self) -> f32 {
        let Some(revolution) = revolution_ms(self.working) else {
            return PARKED_ANGLE;
        };
        if !self.theme.animation {
            return PARKED_ANGLE;
        }
        let phase = self.now_ms.rem_euclid(revolution) as f32 / revolution as f32;
        PARKED_ANGLE + phase * TAU
    }

    fn pixel_color(&self, pixel: u8) -> Color {
        match pixel {
            b'L' => {
                if self.theme.id == "mono16" {
                    self.theme.selfg
                } else {
                    self.lamp
                }
            }
            b'N' => self.theme.text,
            b'T' => self.theme.muted,
            b'B' => self.theme.faint,
            _ => self.theme.bg,
        }
    }
}

impl Widget for Turbine<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let grid = turbine_grid(self.angle());
        paint(area, buffer, &grid, |pixel, _| self.pixel_color(pixel));
    }
}

/// Revolution period for a fleet: `None` when nothing is working, so the rotor parks. More
/// concurrent sessions spin it faster, down to a floor.
fn revolution_ms(working: usize) -> Option<i64> {
    match working {
        0 => None,
        n => Some((BASE_REVOLUTION_MS / n as i64).max(FASTEST_REVOLUTION_MS)),
    }
}

/// Mast and base first, then the three blades laid over them, so a blade passes in front of the
/// tower the way a real rotor does.
fn turbine_grid(angle: f32) -> Grid {
    let mut grid = [[b'.'; COLS]; ROWS];
    for row in grid.iter_mut().take(ROWS - 2).skip(HUB_Y as usize + 1) {
        row[HUB_X as usize] = b'T';
    }
    grid[ROWS - 2][4..=6].fill(b'B');
    grid[ROWS - 1][3..=7].fill(b'B');
    for blade in 0..BLADES {
        let theta = angle + TAU * blade as f32 / BLADES as f32;
        let (sin, cos) = theta.sin_cos();
        // Sampling the blade radially leaves holes and stray fragments at most angles, so each
        // blade is drawn as one rasterized line from the hub to its tip instead.
        let tip_x = HUB_X + (BLADE_LEN as f32 * cos).round() as i32;
        let tip_y = HUB_Y + (BLADE_LEN as f32 * sin).round() as i32;
        draw_line(&mut grid, HUB_X, HUB_Y, tip_x, tip_y);
    }
    grid[HUB_Y as usize][HUB_X as usize] = b'N';
    grid
}

/// Bresenham, clipped to the sprite box: every blade comes out as one unbroken run of pixels.
fn draw_line(grid: &mut Grid, from_x: i32, from_y: i32, to_x: i32, to_y: i32) {
    let (mut x, mut y) = (from_x, from_y);
    let dx = (to_x - x).abs();
    let dy = -(to_y - y).abs();
    let step_x = if x < to_x { 1 } else { -1 };
    let step_y = if y < to_y { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        if (0..COLS as i32).contains(&x) && (0..ROWS as i32).contains(&y) {
            grid[y as usize][x as usize] = b'L';
        }
        if x == to_x && y == to_y {
            return;
        }
        let doubled = error * 2;
        if doubled >= dy {
            error += dy;
            x += step_x;
        }
        if doubled <= dx {
            error += dx;
            y += step_y;
        }
    }
}

// --- Shared rendering -----------------------------------------------------------

/// Two pixel rows per terminal row: the upper half block's foreground is the top pixel and its
/// background the bottom one. `color` receives the pixel token and its column.
fn paint(area: Rect, buffer: &mut Buffer, grid: &Grid, color: impl Fn(u8, u16) -> Color) {
    let height = area.height.min(HEIGHT);
    let width = area.width.min(WIDTH);
    for row in 0..height {
        let top = usize::from(row) * 2;
        for column in 0..width {
            let index = usize::from(column);
            buffer[(area.x + column, area.y + row)]
                .set_char('▀')
                .set_fg(color(grid[top][index], column))
                .set_bg(color(grid[top + 1][index], column));
        }
    }
}

fn intensity(now_ms: i64, delay_ms: i64, minimum: u8, period_ms: i64) -> u8 {
    let phase = (now_ms - delay_ms).rem_euclid(period_ms);
    let half = period_ms / 2;
    let rising = if phase <= half {
        phase
    } else {
        period_ms - phase
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

    fn stars(theme: &Theme, fleet: Fleet, now_ms: i64) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        Constellation::new(theme, fleet, now_ms).render(Rect::new(0, 0, WIDTH, HEIGHT), &mut buffer);
        buffer
    }

    fn turbine(theme: &Theme, working: usize, now_ms: i64) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        Turbine::new(theme, theme.accent, working, now_ms)
            .render(Rect::new(0, 0, WIDTH, HEIGHT), &mut buffer);
        buffer
    }

    /// Star slot `index` as (column, terminal row, is_upper_half), read off the bitmap so a
    /// layout tweak cannot silently point the assertions at empty sky.
    fn star_cell(index: usize) -> (u16, u16, bool) {
        let digit = b'0' + index as u8;
        for (row, pixels) in CONSTELLATION_PIXELS.iter().enumerate() {
            if let Some(column) = pixels.iter().position(|pixel| *pixel == digit) {
                return (column as u16, (row / 2) as u16, row % 2 == 0);
            }
        }
        panic!("star {index} missing from the field");
    }

    fn star_color(buffer: &Buffer, index: usize) -> Color {
        let (column, row, upper) = star_cell(index);
        let cell = &buffer[(column, row)];
        if upper { cell.fg } else { cell.bg }
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

    #[test]
    fn terminal_match_renders_blank_halves_with_an_opaque_background() {
        let theme = theme::terminal(false);
        let buffer = rendered(&theme, false, 0);
        let blank = &buffer[(1, 1)];

        assert_eq!(blank.symbol(), "▀");
        assert_ne!(blank.fg, Color::Reset);
        assert_eq!(blank.fg, blank.bg);
    }

    #[test]
    fn mono16_lighthouse_changes_between_animation_phases() {
        let theme = theme::mono16(false);

        assert_ne!(rendered(&theme, false, 0), rendered(&theme, false, 1_400));
    }

    #[test]
    fn star_slots_take_their_color_from_the_fleet() {
        let mut theme = theme::amber(false);
        theme.animation = false;
        let fleet = Fleet {
            needs_input: 1,
            working: 2,
            done: 1,
        };
        let buffer = stars(&theme, fleet, 0);

        assert_eq!(star_color(&buffer, 0), theme.warn);
        assert_eq!(star_color(&buffer, 1), theme.accent);
        assert_eq!(star_color(&buffer, 2), theme.accent);
        assert_eq!(star_color(&buffer, 3), theme.muted);
        assert_eq!(star_color(&buffer, 4), theme.faint);
        assert_eq!(star_color(&buffer, STARS - 1), theme.faint);
    }

    #[test]
    fn every_star_slot_is_present_exactly_once() {
        for index in 0..STARS {
            let digit = b'0' + index as u8;
            let count = CONSTELLATION_PIXELS
                .iter()
                .flatten()
                .filter(|pixel| **pixel == digit)
                .count();
            assert_eq!(count, 1, "star {index} appears {count} times");
        }
    }

    #[test]
    fn lit_stars_twinkle_on_their_own_phase_and_dark_ones_hold_still() {
        let theme = theme::amber(false);
        let fleet = Fleet {
            needs_input: 1,
            working: 1,
            done: 0,
        };
        let early = stars(&theme, fleet, 0);
        let late = stars(&theme, fleet, 450);

        assert_ne!(star_color(&early, 0), star_color(&late, 0));
        assert_ne!(star_color(&early, 0), star_color(&early, 1));
        assert_eq!(star_color(&early, 8), star_color(&late, 8));
    }

    #[test]
    fn constellation_holds_still_when_the_theme_disables_animation() {
        let mut theme = theme::amber(false);
        theme.animation = false;
        let fleet = Fleet {
            needs_input: 1,
            working: 3,
            done: 2,
        };

        assert_eq!(stars(&theme, fleet, 0), stars(&theme, fleet, 1_400));
    }

    #[test]
    fn rotor_speed_tracks_the_working_count() {
        assert_eq!(revolution_ms(0), None);
        assert_eq!(revolution_ms(1), Some(BASE_REVOLUTION_MS));
        assert!(revolution_ms(3).unwrap() < revolution_ms(1).unwrap());
        assert_eq!(revolution_ms(50), Some(FASTEST_REVOLUTION_MS));
    }

    #[test]
    fn rotor_turns_only_while_work_is_running() {
        let theme = theme::amber(false);
        let busy_early = turbine(&theme, 2, 0);
        let busy_later = turbine(&theme, 2, 400);
        let idle_early = turbine(&theme, 0, 0);
        let idle_later = turbine(&theme, 0, 400);

        assert_ne!(busy_early, busy_later);
        assert_eq!(idle_early, idle_later);
    }

    #[test]
    fn rotor_holds_still_when_the_theme_disables_animation() {
        let mut theme = theme::amber(false);
        theme.animation = false;

        assert_eq!(turbine(&theme, 4, 0), turbine(&theme, 4, 400));
    }

    /// The parked pose is one blade up and two down at 120 degrees, so its three tips are fixed
    /// points: a rotation-math regression moves them.
    #[test]
    fn parked_rotor_draws_a_mast_a_base_and_three_blade_tips() {
        let grid = turbine_grid(PARKED_ANGLE);

        assert_eq!(grid[HUB_Y as usize][HUB_X as usize], b'N');
        assert_eq!(grid[0][HUB_X as usize], b'L');
        assert_eq!(grid[6][8], b'L');
        assert_eq!(grid[6][2], b'L');
        assert_eq!(grid[ROWS - 3][HUB_X as usize], b'T');
        assert_eq!(grid[ROWS - 1][3], b'B');
    }

    /// Every blade is a solid run from hub to tip; a hole means the rotor reads as dashes.
    #[test]
    fn blades_have_no_gaps_at_any_angle() {
        for tick in 0..24 {
            let angle = PARKED_ANGLE + TAU * tick as f32 / 24.0;
            let grid = turbine_grid(angle);
            let lit: Vec<(i32, i32)> = (0..ROWS as i32)
                .flat_map(|y| (0..COLS as i32).map(move |x| (x, y)))
                .filter(|(x, y)| grid[*y as usize][*x as usize] == b'L')
                .collect();
            for (x, y) in &lit {
                let touching = lit
                    .iter()
                    .filter(|(ox, oy)| {
                        (ox, oy) != (x, y) && (ox - x).abs() <= 1 && (oy - y).abs() <= 1
                    })
                    .count();
                assert!(
                    touching > 0,
                    "blade pixel ({x},{y}) is isolated at tick {tick}"
                );
            }
        }
    }
}

