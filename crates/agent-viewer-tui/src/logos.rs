//! Optional brand-logo marks: the three backend SVGs rasterized once at startup into
//! fixed 2x1-cell graphics protocols (Kitty/iTerm2/Sixel, else Unicode half-blocks), drawn
//! over the reserved 2-column mark slot on list rows and in the composer. Always attempted at
//! startup; any failure here (non-tty, no graphics protocol) leaves the textual marks in place.

use agent_viewer_core::BackendKind;
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;

/// Favicon-style SVGs (white glyph on a brand-colored rounded square), embedded at build
/// time so there is no runtime asset path to resolve.
const CLAUDE_SVG: &str = include_str!("../assets/logos/claude.svg");
const CODEX_SVG: &str = include_str!("../assets/logos/codex.svg");
const OPENCODE_SVG: &str = include_str!("../assets/logos/opencode.svg");

/// Pixel size the SVGs are rasterized to before `new_protocol` downsizes them into the
/// 2x1 cell slot; oversampling keeps the half-blocks fallback from looking chunky.
const RASTER_PX: u32 = 64;

/// The three fixed protocols (one per backend), sized to a 2-column, 1-row cell. The
/// non-stateful `Image` widget borrows these immutably, so a single set serves every frame.
pub struct LogoMarks {
    claude: Protocol,
    codex: Protocol,
    opencode: Protocol,
}

impl LogoMarks {
    /// Query the terminal for its graphics protocol + font size, then build the three fixed
    /// protocols. `from_query_stdio` does terminal I/O (raw-mode toggle on stdin), so call
    /// this before crossterm takes the alt screen; on a non-tty it errors and the caller
    /// keeps the textual marks.
    pub fn build() -> anyhow::Result<LogoMarks> {
        let picker = Picker::from_query_stdio().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(LogoMarks {
            claude: build_protocol(&picker, CLAUDE_SVG)?,
            codex: build_protocol(&picker, CODEX_SVG)?,
            opencode: build_protocol(&picker, OPENCODE_SVG)?,
        })
    }

    #[cfg(test)]
    pub(crate) fn halfblocks_for_test() -> anyhow::Result<LogoMarks> {
        let picker = Picker::halfblocks();
        Ok(LogoMarks {
            claude: build_protocol(&picker, CLAUDE_SVG)?,
            codex: build_protocol(&picker, CODEX_SVG)?,
            opencode: build_protocol(&picker, OPENCODE_SVG)?,
        })
    }

    /// The fixed protocol for a backend's mark, for wrapping in `Image::new`.
    pub fn image(&self, backend: BackendKind) -> &Protocol {
        match backend {
            BackendKind::Claude => &self.claude,
            BackendKind::Codex => &self.codex,
            BackendKind::Opencode => &self.opencode,
        }
    }
}

/// Rasterize one SVG with resvg and hand it to the picker as a 2x1-cell fixed protocol.
fn build_protocol(picker: &Picker, svg: &str) -> anyhow::Result<Protocol> {
    let image = rasterize(svg)?;
    // Fit the oversampled bitmap into exactly the 2x1 mark slot; Resize::Fit downscales.
    picker
        .new_protocol(image, Size::new(2, 1), Resize::Fit(None))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// SVG bytes -> an `RGBA` `DynamicImage` at `RASTER_PX` square. Assets are pure paths, so no
/// font database is needed.
fn rasterize(svg: &str) -> anyhow::Result<image::DynamicImage> {
    use resvg::tiny_skia;
    use resvg::usvg;

    let tree = usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default())?;
    let size = tree.size();
    let scale = RASTER_PX as f32 / size.width().max(1.0);

    let mut pixmap = tiny_skia::Pixmap::new(RASTER_PX, RASTER_PX)
        .ok_or_else(|| anyhow::anyhow!("pixmap alloc failed"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    let rgba = image::RgbaImage::from_raw(RASTER_PX, RASTER_PX, pixmap.data().to_vec())
        .ok_or_else(|| anyhow::anyhow!("rgba buffer mismatch"))?;
    Ok(image::DynamicImage::ImageRgba8(rgba))
}
