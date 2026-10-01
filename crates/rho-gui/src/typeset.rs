//! How Rho Font sets text: letters by their ink, lines by their paragraph.
//!
//! Inside a word, each letter sits so the white between it and its neighbour
//! is one optical gap, the font's own between two `n`s: the mean gap across
//! the x-height between their ink profiles, capped where a letter opens like
//! `c` or `r`. A word takes whatever width that gives; nothing is on a grid.
//!
//! Every letter of a line takes the line's MONO value, its stretch, as in
//! font expansion. The line wrapper asks this typesetter to break each
//! paragraph, choosing the breaks and each line's MONO together to keep the
//! right edge even (Knuth–Plass), and remembers each row's MONO so drawing the
//! row sets it the same way. Upright `i` is iA Writer Duo's, carried by the
//! font at U+E000, and `m` and `w` take their sans form.

use std::collections::HashMap;
use std::sync::Mutex;

use gpui::{
    FontId, FontRun, GlyphId, LineFragment, LineLayout, LineTypesetter, Pixels, PlatformTextSystem,
    ShapedRun, px,
};

const MONO: [u8; 4] = *b"MONO";
/// The MONO values a line may take; it prefers `PREFERRED`.
const MONOS: [f32; 5] = [0.5, 0.55, 0.6, 0.65, 0.7];
const PREFERRED: f32 = 0.6;
/// Cost of a line's MONO a tenth from `PREFERRED`, against a line an em short
/// of the right edge.
const STRETCH: f32 = 1.;
const WIDE: &str = "mwMW";
const DUO_I: char = '\u{E000}';
/// Scanlines across the x-height for ink profiles.
const SCANLINES: usize = 12;
/// Scanlines from descender to ascender, where ink must keep `CLEAR` apart.
const WHOLE: usize = 24;
const CLEAR: f32 = 0.04;
/// How far into an opening the optical gap looks, in ems.
const DEPTH: f32 = 0.1;
/// Rows remembered per font size before they are all forgotten.
const ROWS: usize = 100_000;

#[derive(Default)]
pub struct RhoTypesetter(Mutex<Cache>);

#[derive(Default)]
struct Cache {
    faces: HashMap<FontId, Option<Face>>,
    glyphs: HashMap<(FontId, GlyphId), Glyph>,
    /// Each wrapped row's MONO by font size and the row's trimmed text.
    rows: HashMap<u32, HashMap<String, f32>>,
    /// Each word's widths at `MONOS` by font, font size and the word.
    words: HashMap<(FontId, u32, String), Vec<f32>>,
}

/// A Rho Font face (some weight, upright or italic).
#[derive(Clone)]
struct Face {
    /// The face at each MONO value a letter takes.
    at: Vec<(f32, FontId)>,
    duo_i: Option<GlyphId>,
    /// The optical gap between letters, in ems.
    gap: f32,
}

impl Face {
    fn at(&self, mono: f32) -> FontId {
        self.at
            .iter()
            .find(|(m, _)| *m == mono)
            .map(|(_, id)| *id)
            .expect("a line's MONO value")
    }
}

/// A glyph in one font instance: its advance and ink edges at each scanline, in
/// ems.
#[derive(Clone)]
struct Glyph {
    advance: f32,
    left: [Option<f32>; SCANLINES],
    right: [Option<f32>; SCANLINES],
    /// Ink's left and right edges at scanlines from descender to ascender.
    whole: [Option<(f32, f32)>; WHOLE],
}

/// A glyph of a line, flattened out of its run.
struct Placed {
    font: FontId,
    glyph: gpui::ShapedGlyph,
}

impl Cache {
    fn face(&mut self, ts: &dyn PlatformTextSystem, font: FontId) -> Option<Face> {
        if let Some(face) = self.faces.get(&font) {
            return face.clone();
        }
        let face = (|| {
            let mut at = Vec::new();
            for mono in MONOS.iter().chain(&[0.]) {
                at.push((*mono, ts.font_with_axis(font, MONO, *mono)?));
            }
            let n = ts.glyph_for_char(font, 'n')?;
            let preferred = at.iter().find(|(m, _)| *m == PREFERRED)?.1;
            let n = self.glyph(ts, preferred, n).clone();
            let gap = n.advance + optical(&n, &n)?;
            Some(Face {
                at,
                duo_i: ts.glyph_for_char(font, DUO_I),
                gap,
            })
        })();
        self.faces.insert(font, face.clone());
        face
    }

    fn glyph(&mut self, ts: &dyn PlatformTextSystem, font: FontId, glyph: GlyphId) -> &Glyph {
        self.glyphs.entry((font, glyph)).or_insert_with(|| {
            let metrics = ts.font_metrics(font);
            let upm = metrics.units_per_em as f32;
            let advance = ts.advance(font, glyph).map_or(0., |a| a.width / upm);
            let outline = ts
                .glyph_outline(font, glyph)
                .ok()
                .flatten()
                .unwrap_or_default();
            let edges = |y: f32| {
                let xs = outline.iter().flat_map(|curve| crossings(curve, y));
                xs.fold(None, |e: Option<(f32, f32)>, x| {
                    Some(e.map_or((x, x), |(l, r)| (l.min(x), r.max(x))))
                })
            };
            let (mut left, mut right) = ([None; SCANLINES], [None; SCANLINES]);
            for s in 0..SCANLINES {
                let y = metrics.x_height / upm * (s as f32 + 0.5) / SCANLINES as f32;
                (left[s], right[s]) = edges(y).unzip();
            }
            let (low, high) = (metrics.descent / upm, metrics.ascent / upm);
            let whole = std::array::from_fn(|s| {
                edges(low + (high - low) * (s as f32 + 0.5) / WHOLE as f32)
            });
            Glyph {
                advance,
                left,
                right,
                whole,
            }
        })
    }

    /// Sets `line`, shaped from `text` and `width` wide, at `mono`, returning
    /// its new width. Words of Rho Font letters are set by their ink;
    /// everything else keeps its advance.
    fn set(
        &mut self,
        ts: &dyn PlatformTextSystem,
        text: &str,
        line: &mut [Placed],
        width: f32,
        em: f32,
        mono: f32,
    ) -> f32 {
        let advances: Vec<f32> = (0..line.len())
            .map(|i| {
                let end = line
                    .get(i + 1)
                    .map_or(width, |p| f32::from(p.glyph.position.x));
                end - f32::from(line[i].glyph.position.x)
            })
            .collect();
        let (mut pen, mut start) = (0., 0);
        while start < line.len() {
            let font = line[start].font;
            let letter = |p: &Placed| {
                p.font == font
                    && !p.glyph.is_emoji
                    && !text[p.glyph.index..].starts_with(char::is_whitespace)
            };
            let face = if letter(&line[start]) {
                self.face(ts, font)
            } else {
                None
            };
            let Some(face) = face else {
                line[start].glyph.position.x = px(pen);
                pen += advances[start];
                start += 1;
                continue;
            };
            let end = (start..line.len())
                .find(|&i| !letter(&line[i]))
                .unwrap_or(line.len());
            let mut prev: Option<Glyph> = None;
            let mut x = 0.;
            for placed in &mut line[start..end] {
                let ch = text[placed.glyph.index..].chars().next().unwrap_or(' ');
                let (form, id) = match (ch, face.duo_i) {
                    ('i', Some(duo)) => (font, duo),
                    _ if WIDE.contains(ch) => (face.at(0.), placed.glyph.id),
                    _ => (face.at(mono), placed.glyph.id),
                };
                let glyph = self.glyph(ts, form, id).clone();
                if let Some(prev) = &prev {
                    let even = optical(prev, &glyph).map_or(prev.advance, |c| face.gap - c);
                    x += even.max(clearance(prev, &glyph));
                }
                placed.font = form;
                placed.glyph.id = id;
                placed.glyph.position.x = px(pen + x * em);
                prev = Some(glyph);
            }
            pen += (x + prev.map_or(0., |p| p.advance)) * em;
            start = end;
        }
        pen
    }

    /// The width of `text` set at each of `MONOS`, shaped alone in `font`.
    fn widths(
        &mut self,
        ts: &dyn PlatformTextSystem,
        text: &str,
        font: FontId,
        em: Pixels,
    ) -> Vec<f32> {
        let key = (font, f32::from(em).to_bits(), text.to_owned());
        if let Some(widths) = self.words.get(&key) {
            return widths.clone();
        }
        let layout = ts.layout_line(
            text,
            em,
            &[FontRun {
                len: text.len(),
                font_id: font,
            }],
        );
        let widths: Vec<f32> = MONOS
            .iter()
            .map(|&mono| {
                let mut line = flatten(layout.runs.clone());
                self.set(
                    ts,
                    text,
                    &mut line,
                    f32::from(layout.width),
                    f32::from(em),
                    mono,
                )
            })
            .collect();
        if self.words.len() > ROWS {
            self.words.clear();
        }
        self.words.insert(key, widths.clone());
        widths
    }
}

/// Where a quadratic curve crosses height `y`.
fn crossings(curve: &gpui::QuadraticCurve, y: f32) -> impl Iterator<Item = f32> {
    let [p0, p1, p2] = *curve;
    let (a, b, c) = (p0.y - 2. * p1.y + p2.y, 2. * (p1.y - p0.y), p0.y - y);
    let roots = if a.abs() < 1e-6 {
        [(b.abs() > 1e-9).then(|| -c / b), None]
    } else {
        let d = b * b - 4. * a * c;
        if d < 0. {
            [None, None]
        } else {
            let r = d.sqrt();
            [Some((-b - r) / (2. * a)), Some((-b + r) / (2. * a))]
        }
    };
    roots
        .into_iter()
        .flatten()
        .filter(|t| (0. ..1.).contains(t))
        .map(move |t| (1. - t) * (1. - t) * p0.x + 2. * t * (1. - t) * p1.x + t * t * p2.x)
}

/// The optical gap between `a` and `b` drawn with origins one em-unit apart,
/// minus that distance, or `None` when either has ink on fewer than half the
/// scanlines, like a hyphen or a period, whose missing scanlines would read as
/// white.
fn optical(a: &Glyph, b: &Glyph) -> Option<f32> {
    let inked = |edges: &[Option<f32>]| edges.iter().flatten().count() * 2 >= SCANLINES;
    if !inked(&a.right) || !inked(&b.left) {
        return None;
    }
    let gaps: Vec<Option<f32>> = (0..SCANLINES)
        .map(|s| Some(b.left[s]? - a.right[s]?))
        .collect();
    let closest = gaps.iter().flatten().copied().reduce(f32::min)?;
    let cap = closest + DEPTH;
    Some(
        gaps.iter()
            .map(|g| g.map_or(cap, |g| g.min(cap)))
            .sum::<f32>()
            / SCANLINES as f32,
    )
}

/// The least distance between the origins of `a` and `b` that keeps their ink
/// `CLEAR` apart at every height.
fn clearance(a: &Glyph, b: &Glyph) -> f32 {
    a.whole
        .iter()
        .zip(&b.whole)
        .filter_map(|(a, b)| Some(a.as_ref()?.1 - b.as_ref()?.0 + CLEAR))
        .fold(f32::MIN, f32::max)
}

fn flatten(runs: Vec<ShapedRun>) -> Vec<Placed> {
    runs.into_iter()
        .flat_map(|run| {
            run.glyphs.into_iter().map(move |glyph| Placed {
                font: run.font_id,
                glyph,
            })
        })
        .collect()
}

/// A piece of a paragraph that can't be broken: a word, an inline element, or
/// part of a word too long for a row.
struct Piece {
    start: usize,
    /// Its width at each of `MONOS`, in pixels.
    widths: Vec<f32>,
    /// The width of the spaces after it.
    spaces: f32,
    /// Whether a row may start with it.
    breakable: bool,
}

impl LineTypesetter for RhoTypesetter {
    fn typeset(&self, ts: &dyn PlatformTextSystem, text: &str, layout: &mut LineLayout) {
        let mut cache = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let em = f32::from(layout.font_size);
        let mono = cache
            .rows
            .get(&em.to_bits())
            .and_then(|rows| rows.get(text.trim()))
            .copied()
            .unwrap_or(PREFERRED);
        let mut line = flatten(std::mem::take(&mut layout.runs));
        layout.width = px(cache.set(ts, text, &mut line, f32::from(layout.width), em, mono));
        for placed in line {
            match layout.runs.last_mut() {
                Some(run) if run.font_id == placed.font => run.glyphs.push(placed.glyph),
                _ => layout.runs.push(ShapedRun {
                    font_id: placed.font,
                    glyphs: vec![placed.glyph],
                }),
            }
        }
    }

    fn wrap(
        &self,
        ts: &dyn PlatformTextSystem,
        font: FontId,
        font_size: Pixels,
        fragments: &[LineFragment],
        wrap_width: Pixels,
        indent: Pixels,
    ) -> Option<Vec<usize>> {
        let mut cache = self.0.lock().unwrap_or_else(|e| e.into_inner());
        cache.face(ts, font)?;
        let (em, wrap_width, indent) = (
            f32::from(font_size),
            f32::from(wrap_width),
            f32::from(indent),
        );
        let space = cache.widths(ts, " ", font, font_size)[0];

        // The paragraph's text, with each element's bytes as NULs, cut into pieces:
        // words between spaces, and each element on its own.
        let mut text = String::new();
        let mut elements = Vec::new();
        for fragment in fragments {
            match fragment {
                LineFragment::Text { text: t } => text.push_str(t),
                LineFragment::Element { width, len_utf8 } => {
                    elements.push((text.len(), *len_utf8, f32::from(*width)));
                    text.extend(std::iter::repeat_n('\0', *len_utf8));
                }
            }
        }
        let mut pieces: Vec<Piece> = Vec::new();
        let (mut lead, mut at) = (0., 0);
        while at < text.len() {
            let spaces = text[at..].len() - text[at..].trim_start_matches(' ').len();
            if spaces > 0 {
                match pieces.last_mut() {
                    Some(piece) => piece.spaces += space * spaces as f32,
                    None => lead += space * spaces as f32,
                }
                at += spaces;
                continue;
            }
            let breakable = pieces.last().is_some_and(|piece| piece.spaces > 0.);
            if let Some(&(_, len, width)) = elements.iter().find(|e| e.0 == at) {
                let widths = vec![width; MONOS.len()];
                pieces.push(Piece {
                    start: at,
                    widths,
                    spaces: 0.,
                    breakable,
                });
                at += len;
                continue;
            }
            let element = elements.iter().map(|e| e.0).find(|&e| e > at);
            let end = text[at..]
                .find(' ')
                .map_or(text.len(), |i| at + i)
                .min(element.unwrap_or(text.len()));
            let widths = cache.widths(ts, &text[at..end], font, font_size);
            pieces.push(Piece {
                start: at,
                widths,
                spaces: 0.,
                breakable,
            });
            at = end;
        }
        // Words too wide for a row break between letters, as greedily as they must.
        let mut i = 0;
        while i < pieces.len() {
            let room = wrap_width - indent;
            if pieces[i].widths[0] <= room || text.as_bytes()[pieces[i].start] == 0 {
                i += 1;
                continue;
            }
            let end = pieces.get(i + 1).map_or(text.len(), |p| p.start);
            let word = text[pieces[i].start..end].trim_end_matches(' ');
            let cut = word
                .char_indices()
                .skip(1)
                .map(|(ix, _)| ix)
                .take_while(|&ix| cache.widths(ts, &word[..ix], font, font_size)[0] <= room)
                .last()
                .unwrap_or_else(|| word.chars().next().map_or(word.len(), char::len_utf8));
            if cut >= word.len() {
                i += 1;
                continue;
            }
            let start = pieces[i].start;
            pieces[i].widths = cache.widths(ts, &word[..cut], font, font_size);
            let spaces = std::mem::take(&mut pieces[i].spaces);
            let widths = cache.widths(ts, &word[cut..], font, font_size);
            pieces.insert(
                i + 1,
                Piece {
                    start: start + cut,
                    widths,
                    spaces,
                    breakable: true,
                },
            );
            i += 1;
        }
        if pieces.is_empty() {
            return Some(Vec::new());
        }

        // Knuth–Plass: the cheapest rows ending at each piece, each row at its best
        // MONO.
        let preferred = MONOS
            .iter()
            .position(|m| *m == PREFERRED)
            .expect("MONOS has PREFERRED");
        let n = pieces.len();
        let mut best: Vec<(f32, usize, usize)> = vec![(f32::INFINITY, 0, preferred); n + 1];
        best[0].0 = 0.;
        for end in 1..=n {
            let mut width = vec![0.; MONOS.len()];
            for start in (0..end).rev() {
                for (k, w) in width.iter_mut().enumerate() {
                    *w += pieces[start].widths[k] + pieces[start].spaces;
                }
                if start > 0 && !pieces[start].breakable {
                    continue;
                }
                let room = wrap_width - if start == 0 { 0. } else { indent };
                let used: Vec<f32> = width
                    .iter()
                    .map(|w| w + if start == 0 { lead } else { 0. })
                    .collect();
                let fits = |k: usize| used[k] <= room;
                let stretch = |k: usize| STRETCH * ((MONOS[k] - PREFERRED) / 0.1).powi(2);
                let row = if end == n {
                    // The last row needs only to fit.
                    (0..MONOS.len())
                        .filter(|&k| fits(k))
                        .map(|k| (stretch(k), k))
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                } else {
                    (0..MONOS.len())
                        .filter(|&k| fits(k))
                        .map(|k| (((room - used[k]) / em).powi(2) + stretch(k), k))
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                };
                let (cost, k) = match row {
                    Some(row) => row,
                    // A row too wide at every MONO overflows only when nothing shorter can end
                    // here.
                    None if best[end].0.is_infinite() => (1e6, 0),
                    None => break,
                };
                if best[start].0 + cost < best[end].0 {
                    best[end] = (best[start].0 + cost, start, k);
                }
            }
        }
        let mut rows = Vec::new();
        let mut end = n;
        while end > 0 {
            let (_, start, k) = best[end];
            rows.push((start, end, MONOS[k]));
            end = start;
        }
        rows.reverse();

        let remembered = cache.rows.entry(em.to_bits()).or_default();
        if remembered.len() > ROWS {
            remembered.clear();
        }
        for &(start, end, mono) in &rows {
            let row = &text[pieces[start].start..pieces.get(end).map_or(text.len(), |p| p.start)];
            if !row.contains('\0') {
                remembered.insert(row.trim().to_owned(), mono);
            }
        }
        Some(
            rows[1..]
                .iter()
                .map(|&(start, _, _)| pieces[start].start)
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use gpui::FontRun;
    use gpui_wgpu::CosmicTextSystem;

    use super::*;

    const EM: f32 = 16.;
    const PARAGRAPH: &str = "Transcripts mix prose with paths like crates/rho-gui/src/typeset.rs::RhoTypesetter::wrap_every_paragraph_whole and identifiers. Full monospace spends width on every narrow letter, and plain sans makes code look like prose; minimum illicit swimming, wow.";

    fn rho() -> anyhow::Result<(CosmicTextSystem, FontId)> {
        let ts = CosmicTextSystem::new_without_system_fonts("sans-serif");
        ts.add_fonts(vec![
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/fonts/rho-font/RhoFont-Regular.ttf"
            ))?
            .into(),
        ])?;
        let font = ts.font_id(&gpui::font("Rho Font"))?;
        Ok((ts, font))
    }

    fn shape(ts: &CosmicTextSystem, text: &str, font: FontId) -> LineLayout {
        ts.layout_line(
            text,
            px(EM),
            &[FontRun {
                len: text.len(),
                font_id: font,
            }],
        )
    }

    fn glyphs(layout: &LineLayout) -> Vec<(FontId, gpui::ShapedGlyph)> {
        layout
            .runs
            .iter()
            .flat_map(|r| r.glyphs.iter().map(|g| (r.font_id, g.clone())))
            .collect()
    }

    #[test]
    fn letters_keep_one_optical_gap() -> anyhow::Result<()> {
        let (ts, font) = rho()?;
        let text = "minimum illicit, wow rhythm";
        let shaped = shape(&ts, text, font);
        let mut set = shape(&ts, text, font);
        let typesetter = RhoTypesetter::default();
        typesetter.typeset(&ts, text, &mut set);

        let mut cache = typesetter.0.lock().unwrap();
        let face = cache.face(&ts, font).expect("Rho Font");
        let (before, after) = (glyphs(&shaped), glyphs(&set));
        assert_eq!(
            before.iter().map(|g| g.1.index).collect::<Vec<_>>(),
            after.iter().map(|g| g.1.index).collect::<Vec<_>>()
        );
        let duo = ts
            .glyph_for_char(font, DUO_I)
            .expect("Rho Font carries Duo's i");
        let mut gaps = 0;
        for (i, (form, glyph)) in after.iter().enumerate() {
            let ch = &text[glyph.index..glyph.index + 1];
            match ch {
                "i" => assert_eq!(glyph.id, duo),
                "m" | "w" => assert_eq!(*form, face.at(0.), "m and w take their sans form"),
                " " => assert_eq!(*form, font),
                _ => assert_eq!(*form, face.at(PREFERRED)),
            }
            let Some((next_form, next)) = after.get(i + 1) else {
                continue;
            };
            let next_ch = &text[next.index..next.index + 1];
            if ch == " " {
                let was = f32::from(before[i + 1].1.position.x - before[i].1.position.x);
                assert!(
                    (f32::from(next.position.x - glyph.position.x) - was).abs() < 1e-3,
                    "spaces keep their width"
                );
            } else if next_ch != " " && next_ch != "," {
                let (a, b) = (
                    cache.glyph(&ts, *form, glyph.id).clone(),
                    cache.glyph(&ts, *next_form, next.id).clone(),
                );
                let distance = f32::from(next.position.x - glyph.position.x) / EM;
                let gap = distance + optical(&a, &b).expect("letters share the x-height");
                assert!(
                    (gap - face.gap).abs() < 1e-4,
                    "{ch}{next_ch}: gap {gap}, want {}",
                    face.gap
                );
                gaps += 1;
            }
        }
        assert_eq!(gaps, 19);
        assert!(
            face.gap > 0.05 && face.gap < 0.3,
            "an ordinary letter gap: {}",
            face.gap
        );
        Ok(())
    }

    /// Each row of `text` broken at `breaks`, set as it would be drawn.
    fn draw(
        typesetter: &RhoTypesetter,
        ts: &CosmicTextSystem,
        font: FontId,
        breaks: &[usize],
    ) -> Vec<(String, f32)> {
        let mut starts = vec![0];
        starts.extend(breaks);
        starts.push(PARAGRAPH.len());
        starts
            .windows(2)
            .map(|w| {
                let row = &PARAGRAPH[w[0]..w[1]];
                let mut layout = shape(ts, row, font);
                typesetter.typeset(ts, row, &mut layout);
                (row.to_owned(), f32::from(layout.width))
            })
            .collect()
    }

    #[test]
    fn rows_fit_and_beat_greedy_breaks() -> anyhow::Result<()> {
        let (ts, font) = rho()?;
        for width in [150., 260., 410.] {
            let typesetter = RhoTypesetter::default();
            let fragments = [LineFragment::text(PARAGRAPH)];
            let breaks = typesetter
                .wrap(&ts, font, px(EM), &fragments, px(width), px(0.))
                .expect("Rho Font wraps");
            let rows = draw(&typesetter, &ts, font, &breaks);
            for (row, drawn) in &rows {
                assert!(
                    *drawn <= width + 0.01,
                    "{row:?} is {drawn} wide, over {width}"
                );
                let monos = &typesetter.0.lock().unwrap().rows[&EM.to_bits()];
                assert!(monos.contains_key(row.trim()), "{row:?} remembers its MONO");
            }
            assert!(
                rows.iter()
                    .any(|(row, _)| row.starts_with("wrap_") || row.contains("::wrap"))
            );

            // Greedy breaks at the preferred MONO, measured as drawn.
            let plain = RhoTypesetter::default();
            let mut greedy = Vec::new();
            let (mut start, mut last_fit) = (0, None);
            let words: Vec<usize> = PARAGRAPH.match_indices(' ').map(|(i, _)| i + 1).collect();
            for &end in words.iter().chain([&PARAGRAPH.len()]) {
                let row = &PARAGRAPH[start..end];
                let mut layout = shape(&ts, row, font);
                plain.typeset(&ts, row, &mut layout);
                if f32::from(layout.width) > width
                    && let Some(fit) = last_fit
                {
                    greedy.push(fit);
                    start = fit;
                }
                last_fit = Some(end);
            }
            let ragged = |rows: &[(String, f32)], monos: &HashMap<String, f32>| -> f32 {
                rows[..rows.len() - 1]
                    .iter()
                    .map(|(row, drawn)| {
                        let mono = monos.get(row.trim()).copied().unwrap_or(PREFERRED);
                        ((width - drawn) / EM).powi(2)
                            + STRETCH * ((mono - PREFERRED) / 0.1).powi(2)
                    })
                    .sum()
            };
            let ours = ragged(&rows, &typesetter.0.lock().unwrap().rows[&EM.to_bits()]);
            let greedy_rows = draw(&plain, &ts, font, &greedy);
            if greedy_rows.iter().all(|(_, drawn)| *drawn <= width + 0.01) {
                let theirs = ragged(&greedy_rows, &HashMap::new());
                assert!(
                    ours <= theirs + 1e-3,
                    "at {width}: ours {ours}, greedy {theirs}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn highlight_chunks_do_not_break_words() -> anyhow::Result<()> {
        let (ts, font) = rho()?;
        let typesetter = RhoTypesetter::default();
        let whole = typesetter.wrap(
            &ts,
            font,
            px(EM),
            &[LineFragment::text(PARAGRAPH)],
            px(260.),
            px(0.),
        );
        let chunks: Vec<LineFragment> = PARAGRAPH
            .as_bytes()
            .chunks(7)
            .map(|c| LineFragment::text(std::str::from_utf8(c).unwrap()))
            .collect();
        let chunked = typesetter.wrap(&ts, font, px(EM), &chunks, px(260.), px(0.));
        assert_eq!(whole, chunked);
        Ok(())
    }

    #[test]
    fn capitals_keep_clear_of_their_neighbours() -> anyhow::Result<()> {
        let (ts, font) = rho()?;
        let text = "The FIs (L4) Tyr";
        let typesetter = RhoTypesetter::default();
        let mut set = shape(&ts, text, font);
        typesetter.typeset(&ts, text, &mut set);
        let mut cache = typesetter.0.lock().unwrap();
        let after = glyphs(&set);
        let mut pairs = 0;
        for pair in after.windows(2) {
            let [(fa, a), (fb, b)] = pair else { continue };
            if text[a.index..].starts_with(' ') || text[b.index..].starts_with(' ') {
                continue;
            }
            let (ga, gb) = (
                cache.glyph(&ts, *fa, a.id).clone(),
                cache.glyph(&ts, *fb, b.id).clone(),
            );
            let tightest = ga
                .whole
                .iter()
                .zip(&gb.whole)
                .filter_map(|(l, r)| {
                    Some(
                        f32::from(b.position.x - a.position.x) / EM + r.as_ref()?.0 - l.as_ref()?.1,
                    )
                })
                .fold(f32::MAX, f32::min);
            assert!(
                tightest >= CLEAR - 1e-4,
                "{:?}: ink {tightest} em apart",
                &text[a.index..=b.index]
            );
            pairs += 1;
        }
        assert_eq!(pairs, 9);
        Ok(())
    }

}
