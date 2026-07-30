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

/// Which header mascot is drawn. Cycled with Ctrl+G or picked from the command palette; every
/// one occupies the same 11x6 cell box, so the header layout never changes with the choice.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpriteKind {
    #[default]
    Lighthouse,
    Constellation,
    Turbine,
    Sailboat,
    Airplane,
    HotAirBalloon,
}

impl SpriteKind {
    pub fn next(self) -> Self {
        match self {
            SpriteKind::Lighthouse => SpriteKind::Constellation,
            SpriteKind::Constellation => SpriteKind::Turbine,
            SpriteKind::Turbine => SpriteKind::Sailboat,
            SpriteKind::Sailboat => SpriteKind::Airplane,
            SpriteKind::Airplane => SpriteKind::HotAirBalloon,
            SpriteKind::HotAirBalloon => SpriteKind::Lighthouse,
        }
    }

    /// Every sprite, in cycle order. Drives the palette listing so a new sprite shows up there
    /// by adding one variant.
    pub const ALL: [SpriteKind; 6] = [
        SpriteKind::Lighthouse,
        SpriteKind::Constellation,
        SpriteKind::Turbine,
        SpriteKind::Sailboat,
        SpriteKind::Airplane,
        SpriteKind::HotAirBalloon,
    ];

    /// Resolve a stored or env-supplied name. `None` for anything unknown, so a stale setting
    /// falls back to the default instead of wedging the header.
    pub fn from_name(value: Option<&str>) -> Option<Self> {
        let value = value?.trim();
        SpriteKind::ALL
            .into_iter()
            .find(|sprite| sprite.name() == value)
    }

    /// Both the display label and the persisted id; they are deliberately the same string so a
    /// setting written by one build reads back the same way in the next.
    pub fn name(self) -> &'static str {
        match self {
            SpriteKind::Lighthouse => "lighthouse",
            SpriteKind::Constellation => "constellation",
            SpriteKind::Turbine => "turbine",
            SpriteKind::Sailboat => "sailboat",
            SpriteKind::Airplane => "airplane",
            SpriteKind::HotAirBalloon => "hot air balloon",
        }
    }

    /// What the sprite's motion actually encodes, shown as the palette row's detail.
    pub fn detail(self) -> &'static str {
        match self {
            SpriteKind::Lighthouse => {
                "header sprite · beam sweeps outward, lamp warns on needs-input"
            }
            SpriteKind::Constellation => {
                "header sprite · one star per session, brightest need input"
            }
            SpriteKind::Turbine => "header sprite · spins with the working count, parks when idle",
            SpriteKind::Sailboat => "header sprite · bobs as the waves advance",
            SpriteKind::Airplane => "header sprite · crosses a field of clouds",
            SpriteKind::HotAirBalloon => "header sprite · drifts gently past clouds",
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
        paint(
            area,
            buffer,
            &CONSTELLATION_PIXELS,
            |pixel, _| match pixel {
                b'0'..=b'8' => self.star_color(usize::from(pixel - b'0')),
                _ => self.theme.bg,
            },
        );
    }
}

// --- Turbine --------------------------------------------------------------------

const HUB_X: i32 = 5;
const HUB_Y: i32 = 4;
const BLADE_LEN: i32 = 4;
const BLADES: i32 = 3;
/// One blade straight up: the pose a stopped rotor parks in.
const PARKED_ANGLE: f32 = -TAU / 4.0;
const FASTEST_REVOLUTION_MS: i64 = 2_400;
const BASE_REVOLUTION_MS: i64 = 6_000;

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
/// concurrent sessions spin it faster, but the curve flattens quickly - dividing straight by the
/// working count pegs a real fleet at the floor and the rotor just looks frantic.
fn revolution_ms(working: usize) -> Option<i64> {
    match working {
        0 => None,
        n => Some((BASE_REVOLUTION_MS * 2 / (n as i64 + 1)).max(FASTEST_REVOLUTION_MS)),
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

const SAILBOAT_PIXELS: [&[u8]; 7] = [
    b".....T.....",
    b"....ST.....",
    b"...SST.....",
    b"..SSST.....",
    b".....T.....",
    b"..HHHHHH...",
    b"...HHHH....",
];
const SAILBOAT_BOB_PERIOD_MS: i64 = 2_400;
const WAVE_PERIOD_MS: i64 = 600;
const SAILBOAT_Y: usize = 3;

pub(super) struct Sailboat<'a> {
    theme: &'a Theme,
    now_ms: i64,
}

impl<'a> Sailboat<'a> {
    pub(super) fn new(theme: &'a Theme, now_ms: i64) -> Self {
        Self { theme, now_ms }
    }

    fn bob(&self) -> usize {
        if !self.theme.animation {
            return 0;
        }
        usize::from(self.now_ms.rem_euclid(SAILBOAT_BOB_PERIOD_MS) >= 1_200)
    }

    fn wave_phase(&self) -> usize {
        if !self.theme.animation {
            return 0;
        }
        (self.now_ms.rem_euclid(WAVE_PERIOD_MS) / 300) as usize
    }

    fn pixel_color(&self, pixel: u8) -> Color {
        match pixel {
            b'S' => self.theme.warn,
            b'T' => self.theme.text,
            b'H' => self.theme.accent,
            b'W' => self.theme.faint,
            _ => self.theme.bg,
        }
    }
}

impl Widget for Sailboat<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let grid = sailboat_grid(self.bob(), self.wave_phase());
        paint(area, buffer, &grid, |pixel, _| self.pixel_color(pixel));
    }
}

fn sailboat_grid(bob: usize, wave_phase: usize) -> Grid {
    let mut grid = [[b'.'; COLS]; ROWS];
    for (source_y, row) in SAILBOAT_PIXELS.iter().enumerate() {
        for (x, pixel) in row.iter().enumerate() {
            if *pixel != b'.' {
                grid[SAILBOAT_Y + source_y + bob][x] = *pixel;
            }
        }
    }
    for (crest, row) in grid[ROWS - 2..].iter_mut().enumerate() {
        for pixel in row
            .iter_mut()
            .skip((crest + wave_phase) % 2)
            .step_by(2)
        {
            *pixel = b'W';
        }
    }
    grid
}

const AIRPLANE_PIXELS: [&[u8]; 5] = [
    b"......P..",
    b"P....PPP.",
    b"PPPPPPPPN",
    b"..PPPP...",
    b"...PP....",
];
const AIRPLANE_WIDTH: i32 = 9;
const AIRPLANE_STEP_MS: i64 = 250;
const AIRPLANE_PERIOD_MS: i64 =
    (COLS as i64 + AIRPLANE_WIDTH as i64) * AIRPLANE_STEP_MS;

pub(super) struct Airplane<'a> {
    theme: &'a Theme,
    now_ms: i64,
}

impl<'a> Airplane<'a> {
    pub(super) fn new(theme: &'a Theme, now_ms: i64) -> Self {
        Self { theme, now_ms }
    }

    fn x(&self) -> i32 {
        if !self.theme.animation {
            return 2;
        }
        (self.now_ms.rem_euclid(AIRPLANE_PERIOD_MS) / AIRPLANE_STEP_MS) as i32
            - AIRPLANE_WIDTH
    }

    fn pixel_color(&self, pixel: u8) -> Color {
        match pixel {
            b'N' => self.theme.text,
            b'P' => self.theme.accent,
            b'C' => self.theme.faint,
            _ => self.theme.bg,
        }
    }
}

impl Widget for Airplane<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let grid = airplane_grid(self.x());
        paint(area, buffer, &grid, |pixel, _| self.pixel_color(pixel));
    }
}

fn airplane_grid(x: i32) -> Grid {
    let mut grid = [[b'.'; COLS]; ROWS];
    grid[4][1] = b'C';
    grid[5][0..=2].fill(b'C');
    grid[8][8..=10].fill(b'C');
    grid[9][9] = b'C';

    for (source_y, row) in AIRPLANE_PIXELS.iter().enumerate() {
        for (source_x, pixel) in row.iter().enumerate() {
            let target_x = x + source_x as i32;
            if *pixel != b'.' && (0..COLS as i32).contains(&target_x) {
                grid[source_y + 3][target_x as usize] = *pixel;
            }
        }
    }
    grid
}

const BALLOON_PIXELS: [&[u8]; 6] = [
    b".BBB.",
    b"BBBBB",
    b"BBBBB",
    b".BBB.",
    b"..R..",
    b"..K..",
];
const BALLOON_WIDTH: i32 = 5;
const BALLOON_STEP_MS: i64 = 700;
const BALLOON_PERIOD_MS: i64 =
    (COLS as i64 + BALLOON_WIDTH as i64) * BALLOON_STEP_MS;
const BALLOON_DRIFT_PERIOD_MS: i64 = 4_000;

pub(super) struct HotAirBalloon<'a> {
    theme: &'a Theme,
    now_ms: i64,
}

impl<'a> HotAirBalloon<'a> {
    pub(super) fn new(theme: &'a Theme, now_ms: i64) -> Self {
        Self { theme, now_ms }
    }

    fn position(&self) -> (i32, usize) {
        if !self.theme.animation {
            return (3, 2);
        }
        let x =
            (self.now_ms.rem_euclid(BALLOON_PERIOD_MS) / BALLOON_STEP_MS) as i32
                - BALLOON_WIDTH;
        let y = 2
            + usize::from(
                self.now_ms.rem_euclid(BALLOON_DRIFT_PERIOD_MS)
                    >= BALLOON_DRIFT_PERIOD_MS / 2,
            );
        (x, y)
    }

    fn pixel_color(&self, pixel: u8) -> Color {
        match pixel {
            b'B' => self.theme.warn,
            b'R' => self.theme.muted,
            b'K' => self.theme.text,
            b'C' => self.theme.faint,
            _ => self.theme.bg,
        }
    }
}

impl Widget for HotAirBalloon<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let (x, y) = self.position();
        let grid = hot_air_balloon_grid(x, y);
        paint(area, buffer, &grid, |pixel, _| self.pixel_color(pixel));
    }
}

fn hot_air_balloon_grid(x: i32, y: usize) -> Grid {
    let mut grid = [[b'.'; COLS]; ROWS];
    grid[1][0..=2].fill(b'C');
    grid[2][1] = b'C';
    grid[9][8..=10].fill(b'C');
    grid[10][9] = b'C';

    for (source_y, row) in BALLOON_PIXELS.iter().enumerate() {
        for (source_x, pixel) in row.iter().enumerate() {
            let target_x = x + source_x as i32;
            if *pixel != b'.' && (0..COLS as i32).contains(&target_x) {
                grid[y + source_y][target_x as usize] = *pixel;
            }
        }
    }
    grid
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
        Constellation::new(theme, fleet, now_ms)
            .render(Rect::new(0, 0, WIDTH, HEIGHT), &mut buffer);
        buffer
    }

    fn turbine(theme: &Theme, working: usize, now_ms: i64) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        Turbine::new(theme, theme.accent, working, now_ms)
            .render(Rect::new(0, 0, WIDTH, HEIGHT), &mut buffer);
        buffer
    }

    fn sailboat(theme: &Theme, now_ms: i64) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        Sailboat::new(theme, now_ms).render(Rect::new(0, 0, WIDTH, HEIGHT), &mut buffer);
        buffer
    }

    fn airplane(theme: &Theme, now_ms: i64) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        Airplane::new(theme, now_ms).render(Rect::new(0, 0, WIDTH, HEIGHT), &mut buffer);
        buffer
    }

    fn hot_air_balloon(theme: &Theme, now_ms: i64) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        HotAirBalloon::new(theme, now_ms).render(Rect::new(0, 0, WIDTH, HEIGHT), &mut buffer);
        buffer
    }

    fn pixel_color(buffer: &Buffer, x: u16, y: u16) -> Color {
        let cell = &buffer[(x, y / 2)];
        if y % 2 == 0 { cell.fg } else { cell.bg }
    }

    fn foreground_points(buffer: &Buffer, theme: &Theme) -> Vec<(i32, i32)> {
        (0..ROWS as u16)
            .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
            .filter(|(x, y)| {
                let color = pixel_color(buffer, *x, *y);
                color != theme.bg && color != theme.faint
            })
            .map(|(x, y)| (i32::from(x), i32::from(y)))
            .collect()
    }

    fn bounds(points: &[(i32, i32)]) -> Option<(i32, i32, i32, i32)> {
        Some((
            points.iter().map(|(x, _)| *x).min()?,
            points.iter().map(|(x, _)| *x).max()?,
            points.iter().map(|(_, y)| *y).min()?,
            points.iter().map(|(_, y)| *y).max()?,
        ))
    }

    fn horizontal_changes(frames: &[Buffer], theme: &Theme) -> usize {
        let positions: Vec<_> = frames
            .iter()
            .map(|frame| {
                bounds(&foreground_points(frame, theme)).map(|(left, right, _, _)| left + right)
            })
            .collect();
        positions
            .windows(2)
            .filter(|pair| pair[0] != pair[1])
            .count()
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

    /// A busy fleet must not peg the rotor at its floor: the whole point of the speed mapping is
    /// that the difference between a quiet fleet and a busy one stays visible.
    #[test]
    fn a_realistic_fleet_stays_well_off_the_speed_floor() {
        let busy = revolution_ms(9).unwrap();

        assert!(
            busy >= 2_000,
            "a sub-2s revolution reads as frantic at the 10fps redraw, got {busy}ms"
        );
        assert!(revolution_ms(1).unwrap() >= busy * 2);
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

    #[test]
    fn sailboat_hull_bobs_while_wave_crests_advance() {
        let theme = theme::amber(false);
        let frames: Vec<_> = (0..=120).map(|step| sailboat(&theme, step * 100)).collect();
        let top_edges: Vec<_> = frames
            .iter()
            .map(|frame| {
                foreground_points(frame, &theme)
                    .iter()
                    .map(|(_, y)| *y)
                    .min()
                    .unwrap()
            })
            .collect();
        let hull_edges: Vec<_> = frames
            .iter()
            .map(|frame| {
                foreground_points(frame, &theme)
                    .iter()
                    .map(|(_, y)| *y)
                    .max()
                    .unwrap()
            })
            .collect();
        let wave_rows: Vec<_> = frames
            .iter()
            .map(|frame| {
                (ROWS as u16 - 2..ROWS as u16)
                    .flat_map(|y| (0..WIDTH).map(move |x| pixel_color(frame, x, y)))
                    .collect::<Vec<_>>()
            })
            .collect();

        assert_eq!(
            top_edges.iter().max().unwrap() - top_edges.iter().min().unwrap(),
            1
        );
        assert_eq!(*top_edges.iter().min().unwrap(), SAILBOAT_Y as i32);
        assert_eq!(
            hull_edges.iter().max().unwrap() - hull_edges.iter().min().unwrap(),
            1
        );
        assert_eq!(*hull_edges.iter().max().unwrap(), ROWS as i32 - 2);
        assert!(wave_rows.iter().skip(1).any(|phase| phase != &wave_rows[0]));
    }

    #[test]
    fn sailboat_holds_still_when_animation_is_disabled() {
        let mut theme = theme::amber(false);
        theme.animation = false;

        assert_eq!(sailboat(&theme, 0), sailboat(&theme, 1_500));
    }

    #[test]
    fn airplane_crosses_clouds_and_wraps_through_clipped_edges() {
        let theme = theme::amber(false);
        let frames: Vec<_> = (0..=200).map(|step| airplane(&theme, step * 100)).collect();
        let planes: Vec<_> = frames
            .iter()
            .map(|frame| foreground_points(frame, &theme))
            .collect();
        let visible: Vec<_> = planes.iter().filter(|plane| !plane.is_empty()).collect();
        let counts: Vec<_> = visible.iter().map(|plane| plane.len()).collect();
        let cloud_cells = |frame: &Buffer| {
            (0..ROWS as u16).flat_map(|y| {
                (0..WIDTH)
                    .filter(move |x| pixel_color(frame, *x, y) == theme.faint)
                    .map(move |x| (i32::from(x), i32::from(y)))
            })
            .collect::<std::collections::BTreeSet<_>>()
        };
        let clouds_at_start = cloud_cells(&airplane(&theme, 0));

        assert_eq!(clouds_at_start, cloud_cells(&airplane(&theme, 3_000)));

        assert!(
            visible
                .iter()
                .filter_map(|plane| bounds(plane))
                .any(|(left, _, _, _)| left == 0)
        );
        assert!(
            visible
                .iter()
                .filter_map(|plane| bounds(plane))
                .any(|(_, right, _, _)| right == i32::from(WIDTH - 1))
        );
        assert!(counts.iter().min().unwrap() < counts.iter().max().unwrap());
        assert!(
            visible
                .iter()
                .any(|plane| plane.iter().any(|point| clouds_at_start.contains(point)))
        );
    }

    #[test]
    fn airplane_holds_still_when_animation_is_disabled() {
        let mut theme = theme::amber(false);
        theme.animation = false;

        assert_eq!(airplane(&theme, 0), airplane(&theme, 12_000));
    }

    #[test]
    fn hot_air_balloon_drifts_more_slowly_with_one_pixel_vertical_change() {
        let theme = theme::amber(false);
        let times: Vec<_> = (0..=200).map(|step| step * 100).collect();
        let airplane_frames: Vec<_> = times
            .iter()
            .map(|now_ms| airplane(&theme, *now_ms))
            .collect();
        let balloon_frames: Vec<_> = times
            .iter()
            .map(|now_ms| hot_air_balloon(&theme, *now_ms))
            .collect();
        let visible_bounds: Vec<_> = balloon_frames
            .iter()
            .filter_map(|frame| bounds(&foreground_points(frame, &theme)))
            .filter(|(left, right, _, _)| *left > 0 && *right < i32::from(WIDTH - 1))
            .collect();
        let top_edges: Vec<_> = visible_bounds.iter().map(|(_, _, top, _)| *top).collect();
        let bottom_edges: Vec<_> = visible_bounds
            .iter()
            .map(|(_, _, _, bottom)| *bottom)
            .collect();
        let horizontal_positions: Vec<_> = visible_bounds
            .iter()
            .map(|(left, right, _, _)| left + right)
            .collect();

        assert!(
            horizontal_positions
                .iter()
                .any(|position| *position != horizontal_positions[0])
        );
        assert_eq!(
            top_edges.iter().max().unwrap() - top_edges.iter().min().unwrap(),
            1
        );
        assert_eq!(
            bottom_edges.iter().max().unwrap() - bottom_edges.iter().min().unwrap(),
            1
        );
        assert!(
            horizontal_changes(&airplane_frames, &theme)
                > horizontal_changes(&balloon_frames, &theme)
        );
    }

    #[test]
    fn hot_air_balloon_holds_still_when_animation_is_disabled() {
        let mut theme = theme::amber(false);
        theme.animation = false;

        assert_eq!(hot_air_balloon(&theme, 0), hot_air_balloon(&theme, 12_000));
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
