//! Glyphs drawn straight from their outlines on the GPU, after Eric Lengyel's Slug algorithm
//! (<https://jcgt.org/published/0006/02/02/>). Each pixel casts one ray along x and one along y
//! through the glyph's quadratic curves and turns the crossings into coverage, so one encoding
//! of a glyph serves every size and subpixel position.

use crate::{Bounds, FontId, GlyphId, Point, point, size};

/// Identifies a glyph outline. A [`FontId`] already pins the font's variations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[expect(missing_docs)]
pub struct VectorGlyphKey {
    pub font_id: FontId,
    pub glyph_id: GlyphId,
}

/// A glyph's encoding in the atlas glyph buffer.
#[derive(Clone, Copy, Debug)]
pub struct VectorGlyph {
    /// Word offset of the encoding.
    pub offset: u32,
    /// Bounds of the outline in ems, y up.
    pub bounds: Bounds<f32>,
}

/// A quadratic Bézier curve in ems, y up.
pub type QuadraticCurve = [Point<f32>; 3];

const MAX_BANDS: usize = 16;

/// Encodes an outline for the vector glyph shader, or returns `None` for an empty outline.
///
/// Word layout, with offsets relative to the start of the encoding:
/// - 0..4: bounds min x, min y, max x, max y.
/// - 4, 5: horizontal and vertical band counts. Bands split the bounds evenly.
/// - Per band, horizontal then vertical: its curve count and the offset of its curve list.
/// - Curve lists: the offset of each curve. Horizontal bands list curves by descending max x
///   and vertical bands by descending max y, so the shader stops at the first curve that lies
///   wholly behind the pixel.
/// - Curves: six floats each.
pub fn encode_vector_glyph(curves: &[QuadraticCurve]) -> Option<(Bounds<f32>, Vec<u32>)> {
    let (first, rest) = curves.split_first()?;
    let (mut min, mut max) = (first[0], first[0]);
    for p in rest.iter().chain([first]).flatten() {
        min = point(min.x.min(p.x), min.y.min(p.y));
        max = point(max.x.max(p.x), max.y.max(p.y));
    }
    if max.x <= min.x || max.y <= min.y {
        return None;
    }

    let band_count = (curves.len() / 4).clamp(1, MAX_BANDS);
    let header_len = 6 + 4 * band_count;
    let curve_lists = |axis: fn(Point<f32>) -> f32, cross: fn(Point<f32>) -> f32| {
        let (lo, hi) = (axis(min), axis(max));
        let height = (hi - lo) / band_count as f32;
        let epsilon = height * 1e-3;
        (0..band_count)
            .map(|band| {
                let band_lo = lo + band as f32 * height - epsilon;
                let band_hi = lo + (band + 1) as f32 * height + epsilon;
                let mut list: Vec<usize> = (0..curves.len())
                    .filter(|&i| {
                        let ys = curves[i].map(axis);
                        let (y0, y1) = (ys[0].min(ys[1]).min(ys[2]), ys[0].max(ys[1]).max(ys[2]));
                        // A curve flat along the band never crosses a ray cast along it.
                        y0 < y1 && y0 <= band_hi && y1 >= band_lo
                    })
                    .collect();
                let reach = |i: usize| curves[i].map(cross).into_iter().fold(f32::MIN, f32::max);
                list.sort_by(|&a, &b| reach(b).total_cmp(&reach(a)));
                list
            })
            .collect::<Vec<_>>()
    };
    let bands: Vec<Vec<usize>> = curve_lists(|p| p.y, |p| p.x)
        .into_iter()
        .chain(curve_lists(|p| p.x, |p| p.y))
        .collect();

    let lists_len: usize = bands.iter().map(Vec::len).sum();
    let curves_start = header_len + lists_len;
    let mut words = Vec::with_capacity(curves_start + 6 * curves.len());
    words.extend([min.x, min.y, max.x, max.y].map(f32::to_bits));
    words.extend([band_count as u32, band_count as u32]);
    let mut list_start = header_len;
    for band in &bands {
        words.extend([band.len() as u32, list_start as u32]);
        list_start += band.len();
    }
    for band in &bands {
        words.extend(band.iter().map(|&i| (curves_start + 6 * i) as u32));
    }
    for curve in curves {
        words.extend(curve.iter().flat_map(|p| [p.x.to_bits(), p.y.to_bits()]));
    }
    Some((Bounds::new(min, size(max.x - min.x, max.y - min.y)), words))
}
