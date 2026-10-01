//! How Rho Font sets a word: each letter's MONO form and where it sits.
//!
//! The shaper sets Rho Font at its default, MONO 0.82, and that fixes every
//! word's width, so wrapping, cursors and clicks stay as shaped. Inside each
//! word, letters take equal cells (one and a half for `m` and `w`), the
//! monospace rhythm. Each letter then picks a MONO form, and moves off its
//! cell, so the white between neighbours is optically even: the mean gap across
//! the x-height between their ink profiles, capped where a letter opens like
//! `c` or `r`. Upright `i` is iA Writer Duo's, carried by the font at U+E000.

use std::collections::HashMap;
use std::sync::Mutex;

use gpui::{FontId, GlyphId, LineLayout, LineTypesetter, PlatformTextSystem, ShapedRun, px};

const MONO: [u8; 4] = *b"MONO";
/// Forms a letter may take, as MONO values, preferring the widest. They are
/// slimmer than the default, so the rest of each cell opens the gaps.
const FORMS: [f32; 4] = [0.3, 0.4, 0.5, 0.6];
/// Wide letters are cramped as slabs, so they take the sans form, MONO 0, in a
/// wider cell.
const WIDE_FORMS: [f32; 3] = [0.0, 0.2, 0.4];
const WIDE: &str = "mwMW";
const DUO_I: char = '\u{E000}';
/// Scanlines across the x-height for ink profiles.
const SCANLINES: usize = 12;
/// How far into an opening the optical gap looks, in ems.
const DEPTH: f32 = 0.1;
/// Cost of a letter's form being a whole MONO unit from its preferred one, in
/// gap errors of 0.05 em squared.
const PULL: f32 = 3.0;
/// Cost of a letter moving off its cell, against an equal error in a gap.
const STAY: f32 = 1.0;

#[derive(Default)]
pub struct RhoTypesetter(Mutex<Cache>);

#[derive(Default)]
struct Cache {
    faces: HashMap<FontId, Option<Face>>,
    glyphs: HashMap<(FontId, GlyphId), Glyph>,
}

/// A Rho Font face (some weight, upright or italic) at each MONO value a form
/// uses.
struct Face {
    at: Vec<(f32, FontId)>,
    duo_i: Option<GlyphId>,
}

impl Face {
    fn at(&self, mono: f32) -> FontId {
        self.at
            .iter()
            .find(|(m, _)| *m == mono)
            .map(|(_, id)| *id)
            .expect("a form's MONO value")
    }
}

/// A glyph in one font instance: its advance and ink edges at each scanline, in
/// ems.
#[derive(Clone)]
struct Glyph {
    advance: f32,
    left: [Option<f32>; SCANLINES],
    right: [Option<f32>; SCANLINES],
}

/// One way to draw a letter.
#[derive(Clone, Copy)]
struct Form {
    font: FontId,
    glyph: GlyphId,
    mono: f32,
}

impl Cache {
    fn face(&mut self, ts: &dyn PlatformTextSystem, font: FontId) -> Option<&Face> {
        self.faces
            .entry(font)
            .or_insert_with(|| {
                let mut at = Vec::new();
                for mono in FORMS.iter().chain(&WIDE_FORMS).chain(&[1.]) {
                    at.push((*mono, ts.font_with_axis(font, MONO, *mono)?));
                }
                Some(Face {
                    at,
                    duo_i: ts.glyph_for_char(font, DUO_I),
                })
            })
            .as_ref()
    }

    fn glyph(&mut self, ts: &dyn PlatformTextSystem, font: FontId, glyph: GlyphId) -> &Glyph {
        self.glyphs.entry((font, glyph)).or_insert_with(|| {
            let metrics = ts.font_metrics(font);
            let upm = metrics.units_per_em as f32;
            let advance = ts.advance(font, glyph).map_or(0., |a| a.width / upm);
            let (mut left, mut right) = ([None; SCANLINES], [None; SCANLINES]);
            for curve in ts
                .glyph_outline(font, glyph)
                .ok()
                .flatten()
                .unwrap_or_default()
            {
                for s in 0..SCANLINES {
                    let y = metrics.x_height / upm * (s as f32 + 0.5) / SCANLINES as f32;
                    for x in crossings(&curve, y) {
                        left[s] = Some(left[s].map_or(x, |l: f32| l.min(x)));
                        right[s] = Some(right[s].map_or(x, |r: f32| r.max(x)));
                    }
                }
            }
            Glyph {
                advance,
                left,
                right,
            }
        })
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
/// minus that distance.
fn optical(a: &Glyph, b: &Glyph) -> Option<f32> {
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

/// A glyph of the line, flattened out of its run.
struct Placed {
    font: FontId,
    glyph: gpui::ShapedGlyph,
}

impl LineTypesetter for RhoTypesetter {
    fn typeset(&self, ts: &dyn PlatformTextSystem, text: &str, layout: &mut LineLayout) {
        let mut cache = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut line: Vec<Placed> = layout
            .runs
            .drain(..)
            .flat_map(|run| {
                run.glyphs.into_iter().map(move |glyph| Placed {
                    font: run.font_id,
                    glyph,
                })
            })
            .collect();
        let em = f32::from(layout.font_size);
        let mut start = 0;
        while start < line.len() {
            let font = line[start].font;
            let letter = |p: &Placed| {
                p.font == font
                    && !p.glyph.is_emoji
                    && !text[p.glyph.index..].starts_with(char::is_whitespace)
            };
            if !letter(&line[start]) || cache.face(ts, font).is_none() {
                start += 1;
                continue;
            }
            let end = (start..line.len())
                .find(|&i| !letter(&line[i]))
                .unwrap_or(line.len());
            let right = line
                .get(end)
                .map_or(f32::from(layout.width), |p| f32::from(p.glyph.position.x));
            let left = f32::from(line[start].glyph.position.x);
            set_word(
                &mut cache,
                ts,
                text,
                font,
                &mut line[start..end],
                (right - left) / em,
                em,
            );
            start = end;
        }
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
}

/// Sets one word, `width` ems wide, in place.
fn set_word(
    cache: &mut Cache,
    ts: &dyn PlatformTextSystem,
    text: &str,
    font: FontId,
    word: &mut [Placed],
    width: f32,
    em: f32,
) {
    let face = cache.face(ts, font).expect("a Rho Font face");
    let (one, duo_i) = (face.at(1.), face.duo_i);
    let options: Vec<(Vec<Form>, f32)> = word
        .iter()
        .map(|p| {
            let ch = text[p.glyph.index..].chars().next().unwrap_or(' ');
            let glyph = p.glyph.id;
            match (ch, duo_i) {
                ('i', Some(duo)) => (
                    vec![Form {
                        font,
                        glyph: duo,
                        mono: 1.,
                    }],
                    1.,
                ),
                _ if WIDE.contains(ch) => (
                    WIDE_FORMS
                        .iter()
                        .map(|&m| Form {
                            font: face.at(m),
                            glyph,
                            mono: m,
                        })
                        .collect(),
                    0.,
                ),
                _ => (
                    FORMS
                        .iter()
                        .map(|&m| Form {
                            font: face.at(m),
                            glyph,
                            mono: m,
                        })
                        .collect(),
                    FORMS[FORMS.len() - 1],
                ),
            }
        })
        .collect();
    let cells: Vec<f32> = word
        .iter()
        .zip(&options)
        .map(|(p, (_, preferred))| {
            let cell = cache.glyph(ts, one, p.glyph.id).advance;
            if *preferred == 0. { 1.5 * cell } else { cell }
        })
        .collect();
    let scale = width / cells.iter().sum::<f32>().max(1e-3);
    let mut at = 0.;
    let slots: Vec<(f32, f32)> = cells
        .iter()
        .map(|c| {
            let s = (at, c * scale);
            at += c * scale;
            s
        })
        .collect();
    let mut glyph = |f: &Form| cache.glyph(ts, f.font, f.glyph).clone();
    let forms: Vec<Vec<(Form, Glyph)>> = options
        .iter()
        .map(|(fs, _)| fs.iter().map(|f| (*f, glyph(f))).collect())
        .collect();
    // Each form centred in its slot.
    let centred = |i: usize, g: &Glyph| slots[i].0 + (slots[i].1 - g.advance) / 2.;
    let gap = |i: usize, a: &Glyph, b: &Glyph| {
        optical(a, b).map(|c| centred(i + 1, b) - centred(i, a) + c)
    };
    // The word's mean gap with preferred forms, so evening the gaps never resizes
    // the word.
    let preferred = |i: usize| {
        forms[i]
            .iter()
            .min_by(|a, b| {
                (a.0.mono - options[i].1)
                    .abs()
                    .total_cmp(&(b.0.mono - options[i].1).abs())
            })
            .map(|f| &f.1)
            .expect("a form")
    };
    let gaps: Vec<f32> = (1..word.len())
        .filter_map(|i| gap(i - 1, preferred(i - 1), preferred(i)))
        .collect();
    let even = gaps.iter().sum::<f32>() / gaps.len().max(1) as f32;
    let unit = 0.05;
    let unary = |i: usize, f: &Form| PULL * (f.mono - options[i].1).powi(2);

    // Viterbi over forms, letters centred in their slots.
    let mut cost: Vec<f32> = forms[0].iter().map(|(f, _)| unary(0, f)).collect();
    let mut back: Vec<Vec<usize>> = vec![Vec::new()];
    for i in 1..word.len() {
        let (mut next, mut from) = (Vec::new(), Vec::new());
        for (f, g) in &forms[i] {
            let (j, c) = forms[i - 1]
                .iter()
                .enumerate()
                .map(|(j, (_, p))| {
                    (
                        j,
                        cost[j] + gap(i - 1, p, g).map_or(0., |x| ((x - even) / unit).powi(2)),
                    )
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .expect("a form");
            next.push(c + unary(i, f));
            from.push(j);
        }
        cost = next;
        back.push(from);
    }
    let mut pick = vec![0; word.len()];
    pick[word.len() - 1] = (0..cost.len())
        .min_by(|a, b| cost[*a].total_cmp(&cost[*b]))
        .unwrap_or(0);
    for i in (1..word.len()).rev() {
        pick[i - 1] = back[i][pick[i]];
    }
    let chosen: Vec<&(Form, Glyph)> = pick
        .iter()
        .enumerate()
        .map(|(i, k)| &forms[i][*k])
        .collect();

    // Offsets off the cells: minimise Σ STAY·o² + Σ (o[i+1] - o[i] + error[i])²
    // over pairs with an optical gap, a tridiagonal system solved by the Thomas
    // algorithm.
    let n = word.len();
    let (mut lower, mut diag, mut upper, mut rhs) =
        (vec![0f32; n], vec![STAY; n], vec![0f32; n], vec![0f32; n]);
    for i in 0..n.saturating_sub(1) {
        let Some(g) = gap(i, &chosen[i].1, &chosen[i + 1].1) else {
            continue;
        };
        let error = g - even;
        diag[i] += 1.;
        diag[i + 1] += 1.;
        upper[i] -= 1.;
        lower[i + 1] -= 1.;
        rhs[i] += error;
        rhs[i + 1] -= error;
    }
    for i in 1..n {
        let f = lower[i] / diag[i - 1];
        diag[i] -= f * upper[i - 1];
        rhs[i] -= f * rhs[i - 1];
    }
    let mut offset = vec![0f32; n];
    for i in (0..n).rev() {
        let next = if i + 1 < n {
            upper[i] * offset[i + 1]
        } else {
            0.
        };
        offset[i] = (rhs[i] - next) / diag[i];
    }

    let origin = f32::from(word[0].glyph.position.x);
    for (i, placed) in word.iter_mut().enumerate() {
        let (form, g) = chosen[i];
        placed.font = form.font;
        placed.glyph.id = form.glyph;
        placed.glyph.position.x = px(origin + (centred(i, g) + offset[i]) * em);
    }
}

#[cfg(test)]
mod tests {
    use gpui::FontRun;
    use gpui_wgpu::CosmicTextSystem;

    use super::*;

    /// How far optical gaps between neighbouring letters stray from their
    /// word's mean, as a root mean square in ems.
    fn unevenness(
        cache: &mut Cache,
        ts: &dyn PlatformTextSystem,
        text: &str,
        layout: &LineLayout,
    ) -> f32 {
        let em = f32::from(layout.font_size);
        let glyphs: Vec<(FontId, &gpui::ShapedGlyph)> = layout
            .runs
            .iter()
            .flat_map(|r| r.glyphs.iter().map(move |g| (r.font_id, g)))
            .collect();
        let (mut words, mut gaps) = (Vec::new(), Vec::new());
        for pair in glyphs.windows(2) {
            let [(fa, a), (fb, b)] = pair else { continue };
            if text[b.index..].starts_with(' ') {
                words.push(std::mem::take(&mut gaps));
            }
            if text[a.index..].starts_with(' ') || text[b.index..].starts_with(' ') {
                continue;
            }
            let (ga, gb) = (
                cache.glyph(ts, *fa, a.id).clone(),
                cache.glyph(ts, *fb, b.id).clone(),
            );
            if let Some(c) = optical(&ga, &gb) {
                gaps.push(f32::from(b.position.x - a.position.x) / em + c);
            }
        }
        words.push(gaps);
        let errors: Vec<f32> = words
            .iter()
            .flat_map(|gaps| {
                let mean = gaps.iter().sum::<f32>() / gaps.len().max(1) as f32;
                gaps.iter().map(move |g| (g - mean).powi(2))
            })
            .collect();
        (errors.iter().sum::<f32>() / errors.len() as f32).sqrt()
    }

    #[test]
    fn words_keep_their_width_and_even_out() -> anyhow::Result<()> {
        let ts = CosmicTextSystem::new_without_system_fonts("sans-serif");
        ts.add_fonts(vec![
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/fonts/rho-font/RhoFont-Regular.ttf"
            ))?
            .into(),
        ])?;
        let font = ts.font_id(&gpui::font("Rho Font"))?;
        let text = "fill the illicit minimum, wow: ruler rhythm";
        let shaped = ts.layout_line(
            text,
            px(16.),
            &[FontRun {
                len: text.len(),
                font_id: font,
            }],
        );
        let mut set = ts.layout_line(
            text,
            px(16.),
            &[FontRun {
                len: text.len(),
                font_id: font,
            }],
        );
        RhoTypesetter::default().typeset(&ts, text, &mut set);

        let flat = |l: &LineLayout| -> Vec<(FontId, gpui::ShapedGlyph)> {
            l.runs
                .iter()
                .flat_map(|r| r.glyphs.iter().map(|g| (r.font_id, g.clone())))
                .collect()
        };
        let (before, after) = (flat(&shaped), flat(&set));
        assert_eq!(set.width, shaped.width, "the line keeps its width");
        assert_eq!(
            before.iter().map(|g| g.1.index).collect::<Vec<_>>(),
            after.iter().map(|g| g.1.index).collect::<Vec<_>>()
        );

        let duo = ts
            .glyph_for_char(font, DUO_I)
            .expect("Rho Font carries Duo's i");
        let wide: Vec<FontId> = WIDE_FORMS
            .iter()
            .map(|m| ts.font_with_axis(font, MONO, *m).unwrap())
            .collect();
        for ((_, was), (now_font, now)) in before.iter().zip(&after) {
            match &text[now.index..now.index + 1] {
                " " => assert_eq!(
                    now.position, was.position,
                    "spaces stay where the shaper put them"
                ),
                "i" => assert_eq!(now.id, duo),
                "m" | "w" => assert!(wide.contains(now_font), "m and w take a sans-side form"),
                _ => assert_eq!(now.id, was.id),
            }
        }
        assert!(
            after
                .windows(2)
                .all(|p| p[0].1.position.x < p[1].1.position.x),
            "letters stay in order"
        );

        let mut cache = Cache::default();
        let (was, now) = (
            unevenness(&mut cache, &ts, text, &shaped),
            unevenness(&mut cache, &ts, text, &set),
        );
        assert!(
            now < was * 0.6,
            "optical gaps even out: {was} em spread before, {now} after"
        );
        Ok(())
    }
}
