//! Optional brand logo marks rasterized once at startup from the backend SVGs.
//! List rows use fixed two cell protocols. Kitty, iTerm2, and Sixel composers use separate
//! three cell protocols whose artwork is offset by half a cell. Halfblocks composers reuse
//! the two cell list protocols because that fallback cannot represent a horizontal half cell.
//! Startup always attempts construction. Any failure leaves the textual marks in place.

use agent_viewer_core::BackendKind;
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;

/// Favicon-style SVGs (white glyph on a brand-colored rounded square), embedded at build
/// time so there is no runtime asset path to resolve.
const CLAUDE_SVG: &str = include_str!("../assets/logos/claude.svg");
const CODEX_SVG: &str = include_str!("../assets/logos/codex.svg");
/// The Auto spawn entry is not a backend, so it carries a neutral robot mark instead of a brand.
const AUTO_SVG: &str = include_str!("../assets/logos/auto.svg");

/// Pixel size the SVGs are rasterized to before `new_protocol` downsizes them into the
/// two cell slot; oversampling keeps the half blocks fallback from looking chunky.
const RASTER_PX: u32 = 64;

/// Fixed two cell list protocols for each backend, plus separate three cell composer protocols
/// when the graphics format supports a half cell offset. Halfblocks composers reuse the list
/// protocols. The `Image` widget borrows these immutably, so one set serves every frame.
pub struct LogoMarks {
    claude: Protocol,
    codex: Protocol,
    auto: Protocol,
    composer_claude: Option<Protocol>,
    composer_codex: Option<Protocol>,
    composer_auto: Option<Protocol>,
}

impl LogoMarks {
    /// Query the terminal for its graphics protocol and font size, then build the list and
    /// supported composer protocols. `from_query_stdio` performs terminal I/O and temporarily
    /// toggles raw mode on stdin, so call this before crossterm takes the alternate screen.
    /// When stdin is not a terminal, it errors and the caller keeps the textual marks.
    pub fn build() -> anyhow::Result<LogoMarks> {
        let picker = Picker::from_query_stdio().map_err(|e| anyhow::anyhow!("{e}"))?;
        Self::from_picker(&picker)
    }

    #[cfg(test)]
    pub(crate) fn halfblocks_for_test() -> anyhow::Result<LogoMarks> {
        let picker = Picker::halfblocks();
        Self::from_picker(&picker)
    }

    fn from_picker(picker: &Picker) -> anyhow::Result<LogoMarks> {
        let (claude, composer_claude) = build_protocols(picker, CLAUDE_SVG)?;
        let (codex, composer_codex) = build_protocols(picker, CODEX_SVG)?;
        let (auto, composer_auto) = build_protocols(picker, AUTO_SVG)?;
        Ok(LogoMarks {
            claude,
            codex,
            auto,
            composer_claude,
            composer_codex,
            composer_auto,
        })
    }

    /// The fixed protocol for a backend's mark, for wrapping in `Image::new`.
    pub fn image(&self, backend: BackendKind) -> &Protocol {
        match backend {
            BackendKind::Claude => &self.claude,
            BackendKind::Codex => &self.codex,
            BackendKind::AgentRunner => &self.codex,
        }
    }

    /// The composer protocol for a backend's mark. Graphics protocols use a three cell
    /// canvas; halfblocks reuse the fixed two cell list protocol.
    pub fn composer_image(&self, backend: BackendKind) -> &Protocol {
        match backend {
            BackendKind::Claude => self.composer_claude.as_ref().unwrap_or(&self.claude),
            BackendKind::Codex => self.composer_codex.as_ref().unwrap_or(&self.codex),
            BackendKind::AgentRunner => self.composer_codex.as_ref().unwrap_or(&self.codex),
        }
    }

    /// The composer protocol for the Auto entry's robot mark, on the same three cell canvas
    /// as the backend composer marks and with the same halfblocks fallback.
    pub fn composer_auto_image(&self) -> &Protocol {
        self.composer_auto.as_ref().unwrap_or(&self.auto)
    }
}

fn build_protocols(picker: &Picker, svg: &str) -> anyhow::Result<(Protocol, Option<Protocol>)> {
    let image = rasterize(svg)?;
    let composer = match picker.protocol_type() {
        ProtocolType::Halfblocks => None,
        ProtocolType::Kitty | ProtocolType::Sixel | ProtocolType::Iterm2 => {
            Some(build_composer_protocol(picker, &image)?)
        }
    };

    Ok((build_protocol(picker, image)?, composer))
}

/// Hand a rasterized mark to the picker as a fixed two cell protocol.
fn build_protocol(picker: &Picker, image: image::DynamicImage) -> anyhow::Result<Protocol> {
    // Fit the oversampled bitmap into exactly the 2x1 mark slot; Resize::Fit downscales.
    picker
        .new_protocol(image, Size::new(2, 1), Resize::Fit(None))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn build_composer_protocol(
    picker: &Picker,
    image: &image::DynamicImage,
) -> anyhow::Result<Protocol> {
    let canvas = build_composer_canvas(image, picker.font_size());

    picker
        .new_protocol(canvas, Size::new(3, 1), Resize::Fit(None))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn build_composer_canvas(
    image: &image::DynamicImage,
    font_size: ratatui_image::FontSize,
) -> image::DynamicImage {
    use image::imageops::overlay;

    let logo = Resize::Fit(None).resize(image, font_size, Size::new(2, 1), None);
    let mut canvas =
        image::DynamicImage::new_rgba8(u32::from(font_size.width) * 3, u32::from(font_size.height));
    overlay(
        &mut canvas,
        &logo,
        i64::from(font_size.width.div_ceil(2)),
        0,
    );

    canvas
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

#[cfg(test)]
mod tests {
    use super::{LogoMarks, build_composer_canvas};
    use agent_viewer_core::BackendKind;
    use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};
    use ratatui::layout::Size;
    use ratatui_image::picker::{Picker, ProtocolType};
    use ratatui_image::{FontSize, Resize};

    #[test]
    fn graphics_composer_image_uses_its_own_three_cell_canvas() {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let logos = LogoMarks::from_picker(&picker).unwrap();

        assert_eq!(logos.image(BackendKind::Codex).size(), Size::new(2, 1));
        assert_eq!(
            logos.composer_image(BackendKind::Codex).size(),
            Size::new(3, 1)
        );
        assert_eq!(logos.auto.size(), Size::new(2, 1));
        assert_eq!(logos.composer_auto_image().size(), Size::new(3, 1));
    }

    #[test]
    fn composer_canvas_matches_list_bitmap_at_half_cell_offset() {
        let logo = DynamicImage::ImageRgba8(RgbaImage::from_fn(3, 3, |x, y| {
            Rgba([(x * 70) as u8, (y * 80) as u8, ((x + y) * 40) as u8, 255])
        }));

        for font_width in [10, 9] {
            let font_size = FontSize {
                width: font_width,
                height: 20,
            };
            let fitted = Resize::Fit(None).resize(&logo, font_size, Size::new(2, 1), None);
            let canvas = build_composer_canvas(&logo, font_size);
            let offset = u32::from(font_width).div_ceil(2);

            assert_eq!(canvas.dimensions(), (u32::from(font_width) * 3, 20));
            for y in 0..canvas.height() {
                for x in 0..canvas.width() {
                    let expected = if (offset..offset + fitted.width()).contains(&x) {
                        fitted.get_pixel(x - offset, y)
                    } else {
                        Rgba([0, 0, 0, 0])
                    };
                    assert_eq!(
                        canvas.get_pixel(x, y),
                        expected,
                        "unexpected pixel at font width {font_width}, x {x}, y {y}"
                    );
                }
            }
        }
    }
}
