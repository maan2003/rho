//! A text system for a cell grid: every font is the terminal's font, a
//! glyph's id is its code point, and each character advances one cell (two
//! when it is wide). Weight and slant survive as the font id, so the grid
//! can still draw bold and italic.

use std::borrow::Cow;

use gpui::{
    Bounds, DevicePixels, Font, FontId, FontMetrics, FontRun, FontStyle, FontWeight, GlyphId,
    LineLayout, Pixels, PlatformTextSystem, RenderGlyphParams, Result, ShapedGlyph, ShapedRun,
    Size, TextRenderingMode, point, px, size,
};
use unicode_width::UnicodeWidthChar;

use crate::{CELL_H, CELL_W};

/// Advance of one cell in font units, at `units_per_em` = 1000. Exact at
/// the 16 px font size the editor is configured with.
const UNITS_PER_EM: u32 = 1000;

pub(crate) struct CellTextSystem;

pub(crate) fn font_style(font_id: FontId) -> (bool, bool) {
    (font_id.0 & 1 != 0, font_id.0 & 2 != 0)
}

pub(crate) fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

impl PlatformTextSystem for CellTextSystem {
    fn add_fonts(&self, _fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        Ok(())
    }

    fn all_font_names(&self) -> Vec<String> {
        vec!["Terminal".into()]
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let bold = font.weight >= FontWeight::SEMIBOLD;
        let italic = !matches!(font.style, FontStyle::Normal);
        Ok(FontId(bold as usize | (italic as usize) << 1))
    }

    fn font_metrics(&self, _font_id: FontId) -> FontMetrics {
        FontMetrics {
            units_per_em: UNITS_PER_EM,
            ascent: 750.0,
            descent: -250.0,
            line_gap: 0.0,
            underline_position: -100.0,
            underline_thickness: 60.0,
            cap_height: 700.0,
            x_height: 500.0,
            bounding_box: Bounds {
                origin: point(0.0, -250.0),
                size: size(500.0, 1000.0),
            },
        }
    }

    fn typographic_bounds(&self, _font_id: FontId, _glyph_id: GlyphId) -> Result<Bounds<f32>> {
        Ok(Bounds {
            origin: point(0.0, -250.0),
            size: size(500.0, 1000.0),
        })
    }

    fn advance(&self, _font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let cells = char::from_u32(glyph_id.0).map_or(1, |c| char_width(c).max(1));
        Ok(size(500.0 * cells as f32, 0.0))
    }

    fn glyph_for_char(&self, _font_id: FontId, ch: char) -> Option<GlyphId> {
        Some(GlyphId(ch as u32))
    }

    // Every glyph covers its cell, so its sprite's centre names the cell it
    // was painted in. The sprite's origin is the baseline; the cell's top
    // sits three quarters of a line above it.
    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let cells = char::from_u32(params.glyph_id.0).map_or(1, |c| char_width(c).max(1));
        Ok(Bounds {
            origin: point(DevicePixels(0), DevicePixels(-(CELL_H * 0.75) as i32)),
            size: size(
                DevicePixels((CELL_W * cells as f32) as i32),
                DevicePixels(CELL_H as i32),
            ),
        })
    }

    fn rasterize_glyph(
        &self,
        _params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        Ok((raster_bounds.size, Vec::new()))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let mut x = px(0.);
        let mut shaped: Vec<ShapedRun> = Vec::new();
        let mut start = 0;
        for run in runs {
            let end = (start + run.len).min(text.len());
            let mut glyphs = Vec::new();
            for (ix, ch) in text[start..end].char_indices() {
                let width = char_width(ch);
                if width == 0 && ch != '\t' {
                    // Combining marks and controls take no cell.
                    continue;
                }
                glyphs.push(ShapedGlyph {
                    id: GlyphId(ch as u32),
                    position: point(x, px(0.)),
                    index: start + ix,
                    is_emoji: width == 2 && is_emoji(ch),
                });
                x += px(CELL_W * width.max(1) as f32);
            }
            if !glyphs.is_empty() {
                shaped.push(ShapedRun {
                    font_id: run.font_id,
                    glyphs,
                });
            }
            start = end;
        }
        LineLayout {
            font_size,
            width: x,
            ascent: px(CELL_H * 0.75),
            descent: px(CELL_H * 0.25),
            runs: shaped,
            len: text.len(),
        }
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Grayscale
    }
}

fn is_emoji(ch: char) -> bool {
    matches!(ch as u32, 0x1F300..=0x1FAFF | 0x2600..=0x27BF)
}
