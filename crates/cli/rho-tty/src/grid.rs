//! A Scene painted into cells, and the cells written to the terminal.
//!
//! Primitives are painted in draw order, as a GPU would: a quad sets the
//! background of every cell whose centre it covers (and, when opaque,
//! hides the text beneath it), a glyph sprite sets its cell's character and
//! colour, an underline marks its cells. A quad too thin to cover a cell
//! centre but tall as a line is a caret, and becomes the terminal cursor.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use crossterm::QueueableCommand;
use crossterm::cursor::{Hide, MoveTo, SetCursorStyle, Show};
use crossterm::style::{
    Attribute, Color as TermColor, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use gpui::{Bounds, Color, ContentMask, Rgba, ScaledPixels, Scene};

use crate::atlas::{CellAtlas, TileContent};
use crate::text::{char_width, font_style};
use crate::{CELL_H, CELL_W};

pub(crate) type Rgb = [u8; 3];

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// The right half of a wide character drawn in the cell before.
    pub tail: bool,
}

const BLANK: Cell = Cell {
    ch: ' ',
    fg: [255, 255, 255],
    bg: [0, 0, 0],
    bold: false,
    italic: false,
    underline: false,
    tail: false,
};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Caret {
    pub col: u16,
    pub row: u16,
}

#[derive(Clone)]
pub(crate) struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    pub caret: Option<Caret>,
    /// Bars one line tall drawn as `│`, with the cell from before: the
    /// editor's caret is one, and gives its cell back to the terminal's.
    bars: Vec<(Caret, Cell, Cell)>,
}

fn rgba(color: Color) -> (Rgb, f32) {
    let c = Rgba::from(color);
    let byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    ([byte(c.r), byte(c.g), byte(c.b)], c.a)
}

fn over(top: Rgb, alpha: f32, under: Rgb) -> Rgb {
    let a = alpha.clamp(0.0, 1.0);
    std::array::from_fn(|i| (top[i] as f32 * a + under[i] as f32 * (1.0 - a)).round() as u8)
}

/// The cell boundary nearest a pixel edge. gpui lays out on a pixel grid
/// finer than cells, and paddings put text off the cell grid; every edge
/// (a glyph's, a quad's, a clip's) snaps through this one function, so a
/// glyph, the selection behind it and the caret before it stay together.
fn edge(x: f32, cell: f32) -> f32 {
    (x / cell + 0.5).floor()
}

/// The cells between the boundaries nearest `lo` and `hi`.
fn span(lo: f32, hi: f32, cell: f32, count: u16) -> std::ops::Range<u16> {
    let clamp = |v: f32| v.clamp(0.0, count as f32) as u16;
    clamp(edge(lo, cell))..clamp(edge(hi, cell))
}

fn clip(bounds: &Bounds<ScaledPixels>, mask: &ContentMask<ScaledPixels>) -> Bounds<ScaledPixels> {
    bounds.intersect(&mask.bounds)
}

#[derive(Clone, Copy)]
enum Prim {
    Quad(usize),
    Mono(usize),
    Sub(usize),
    Poly(usize),
    Underline(usize),
    Path(usize),
}

impl Grid {
    pub(crate) fn paint(scene: &Scene, atlas: &CellAtlas, cols: u16, rows: u16) -> Self {
        let mut grid = Grid {
            cols,
            rows,
            cells: vec![BLANK; cols as usize * rows as usize],
            caret: None,
            bars: Vec::new(),
        };
        let mut order: Vec<(u32, u8, Prim)> = Vec::new();
        order.extend(
            scene
                .quads
                .iter()
                .enumerate()
                .map(|(i, q)| (q.order, 0, Prim::Quad(i))),
        );
        order.extend(
            scene
                .paths
                .iter()
                .enumerate()
                .map(|(i, p)| (p.order, 0, Prim::Path(i))),
        );
        order.extend(
            scene
                .underlines
                .iter()
                .enumerate()
                .map(|(i, u)| (u.order, 1, Prim::Underline(i))),
        );
        order.extend(
            scene
                .monochrome_sprites
                .iter()
                .enumerate()
                .map(|(i, s)| (s.order, 2, Prim::Mono(i))),
        );
        order.extend(
            scene
                .subpixel_sprites
                .iter()
                .enumerate()
                .map(|(i, s)| (s.order, 2, Prim::Sub(i))),
        );
        order.extend(
            scene
                .polychrome_sprites
                .iter()
                .enumerate()
                .map(|(i, s)| (s.order, 2, Prim::Poly(i))),
        );
        order.sort_by_key(|(order, kind, _)| (*order, *kind));
        for (_, _, prim) in order {
            match prim {
                Prim::Quad(i) => {
                    let q = &scene.quads[i];
                    if let Some(color) = q.background.as_solid() {
                        grid.quad(clip(&q.bounds, &q.content_mask), color);
                    }
                    grid.border(q);
                }
                Prim::Path(i) => {
                    let path = &scene.paths[i];
                    if let Some(color) = path.color.as_solid() {
                        grid.path(path, color);
                    }
                }
                Prim::Underline(i) => {
                    let u = &scene.underlines[i];
                    grid.underline(clip(&u.bounds, &u.content_mask));
                }
                Prim::Mono(i) => {
                    let s = &scene.monochrome_sprites[i];
                    grid.glyph(
                        atlas.content(s.tile.tile_id),
                        &s.bounds,
                        &s.content_mask,
                        Some(s.color),
                    );
                }
                Prim::Sub(i) => {
                    let s = &scene.subpixel_sprites[i];
                    grid.glyph(
                        atlas.content(s.tile.tile_id),
                        &s.bounds,
                        &s.content_mask,
                        Some(s.color),
                    );
                }
                Prim::Poly(i) => {
                    let s = &scene.polychrome_sprites[i];
                    grid.glyph(
                        atlas.content(s.tile.tile_id),
                        &s.bounds,
                        &s.content_mask,
                        None,
                    );
                }
            }
        }
        grid
    }

    /// Where the terminal's cursor stands, from the focused input's caret;
    /// the bar the editor drew for that caret goes.
    pub(crate) fn set_caret(&mut self, caret: Option<Caret>) {
        self.caret = caret;
        let Some(caret) = caret else { return };
        if let Some(&(_, before, after)) = self.bars.iter().rev().find(|(at, ..)| *at == caret)
            && *self.cell(caret.col, caret.row) == after
        {
            *self.cell(caret.col, caret.row) = before;
        }
    }

    /// The cell a pixel point lands in, snapped as everything painted is.
    pub(crate) fn cell_at(&self, x: f32, y: f32) -> Option<Caret> {
        let (col, row) = (edge(x, CELL_W), edge(y, CELL_H));
        (col >= 0.0 && row >= 0.0 && col < self.cols as f32 && row < self.rows as f32).then_some(
            Caret {
                col: col as u16,
                row: row as u16,
            },
        )
    }

    pub(crate) fn row_text(&self, row: u16) -> String {
        let start = row as usize * self.cols as usize;
        self.cells[start..start + self.cols as usize]
            .iter()
            .filter(|cell| !cell.tail)
            .map(|cell| cell.ch)
            .collect()
    }

    fn cell(&mut self, col: u16, row: u16) -> &mut Cell {
        &mut self.cells[row as usize * self.cols as usize + col as usize]
    }

    fn quad(&mut self, b: Bounds<ScaledPixels>, color: Color) {
        let (rgb, alpha) = rgba(color);
        if alpha <= 0.01 || b.size.width.0 <= 0.0 || b.size.height.0 <= 0.0 {
            return;
        }
        let cols = span(
            b.origin.x.0,
            b.origin.x.0 + b.size.width.0,
            CELL_W,
            self.cols,
        );
        let rows = span(
            b.origin.y.0,
            b.origin.y.0 + b.size.height.0,
            CELL_H,
            self.rows,
        );
        let (w, h) = (b.size.width.0, b.size.height.0);
        if w <= CELL_W / 2.0 && h >= CELL_H / 2.0 {
            // As tall as a line and too thin for a cell: a rule.
            let col = edge(b.origin.x.0, CELL_W);
            if col >= 0.0 && col < self.cols as f32 {
                let col = col as u16;
                let line = (rows.len() == 1).then(|| (rows.start, *self.cell(col, rows.start)));
                self.rule(col..col + 1, rows, '│', rgb, alpha);
                if let Some((row, before)) = line {
                    let at = Caret { col, row };
                    let after = *self.cell(col, row);
                    self.bars.push((at, before, after));
                }
            }
            return;
        }
        if h <= CELL_H / 2.0 && w > CELL_W / 2.0 {
            let row = (b.origin.y.0 / CELL_H).floor();
            if row >= 0.0 && row < self.rows as f32 {
                let row = row as u16;
                self.rule(cols, row..row + 1, '─', rgb, alpha);
            }
            return;
        }
        if cols.is_empty() || rows.is_empty() {
            return;
        }
        for row in rows {
            for col in cols.clone() {
                let cell = self.cell(col, row);
                cell.bg = over(rgb, alpha, cell.bg);
                if alpha > 0.99 {
                    cell.ch = ' ';
                    cell.tail = false;
                    cell.underline = false;
                }
            }
        }
    }

    /// A quad's border as box-drawing characters in its outermost cells,
    /// where nothing is written yet; text drawn later goes over it.
    fn border(&mut self, q: &gpui::Quad) {
        let w = &q.border_widths;
        let (rgb, alpha) = rgba(q.border_color);
        if alpha <= 0.01
            || [w.top, w.right, w.bottom, w.left]
                .iter()
                .all(|w| w.0 <= 0.0)
        {
            return;
        }
        let b = &q.bounds;
        let cols = span(
            b.origin.x.0,
            b.origin.x.0 + b.size.width.0,
            CELL_W,
            self.cols,
        );
        let rows = span(
            b.origin.y.0,
            b.origin.y.0 + b.size.height.0,
            CELL_H,
            self.rows,
        );
        if cols.is_empty() || rows.is_empty() {
            return;
        }
        let m = &q.content_mask.bounds;
        let visible_cols = span(
            m.origin.x.0,
            m.origin.x.0 + m.size.width.0,
            CELL_W,
            self.cols,
        );
        let visible_rows = span(
            m.origin.y.0,
            m.origin.y.0 + m.size.height.0,
            CELL_H,
            self.rows,
        );
        let (top, right, bottom, left) = (
            w.top.0 > 0.0,
            w.right.0 > 0.0,
            w.bottom.0 > 0.0,
            w.left.0 > 0.0,
        );
        let round = q.corner_radii.top_left.0 > 0.0;
        let (first_col, last_col) = (cols.start, cols.end - 1);
        let (first_row, last_row) = (rows.start, rows.end - 1);
        for row in rows.clone() {
            for col in cols.clone() {
                let (at_top, at_bottom) = (top && row == first_row, bottom && row == last_row);
                let (at_left, at_right) = (left && col == first_col, right && col == last_col);
                let ch = match (at_top, at_bottom, at_left, at_right) {
                    (true, _, true, _) => {
                        if round {
                            '╭'
                        } else {
                            '┌'
                        }
                    }
                    (true, _, _, true) => {
                        if round {
                            '╮'
                        } else {
                            '┐'
                        }
                    }
                    (_, true, true, _) => {
                        if round {
                            '╰'
                        } else {
                            '└'
                        }
                    }
                    (_, true, _, true) => {
                        if round {
                            '╯'
                        } else {
                            '┘'
                        }
                    }
                    (true, ..) | (_, true, ..) => '─',
                    (.., true, _) | (.., true) => '│',
                    _ => continue,
                };
                if visible_cols.contains(&col) && visible_rows.contains(&row) {
                    self.rule(col..col + 1, row..row + 1, ch, rgb, alpha);
                }
            }
        }
    }

    /// Fills the cells whose middles the path covers, by the non-zero
    /// rule over its triangles, as gpui's path rasterizer counts winding.
    /// Curve triangles only round corners off, and are skipped.
    fn path(&mut self, path: &gpui::Path<ScaledPixels>, color: Color) {
        let (rgb, alpha) = rgba(color);
        if alpha <= 0.01 {
            return;
        }
        let b = clip(&path.bounds, &path.content_mask);
        let cols = span(
            b.origin.x.0,
            b.origin.x.0 + b.size.width.0,
            CELL_W,
            self.cols,
        );
        let rows = span(
            b.origin.y.0,
            b.origin.y.0 + b.size.height.0,
            CELL_H,
            self.rows,
        );
        let triangles: Vec<[(f32, f32); 3]> = path
            .vertices
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|t| {
                t.iter()
                    .all(|v| v.st_position.x == 0.0 && v.st_position.y == 1.0)
            })
            .map(|t| std::array::from_fn(|i| (t[i].xy_position.x.0, t[i].xy_position.y.0)))
            .collect();
        let cross = |a: (f32, f32), b: (f32, f32), p: (f32, f32)| {
            (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
        };
        for row in rows {
            for col in cols.clone() {
                // Layout lands on cells (`layout_grid`), so a cell's middle
                // is inside whatever covers it, rounded corners and all.
                let p = ((col as f32 + 0.5) * CELL_W, (row as f32 + 0.5) * CELL_H);
                let winding: i32 = triangles
                    .iter()
                    .map(|&[a, b, c]| {
                        let (d1, d2, d3) = (cross(a, b, p), cross(b, c, p), cross(c, a, p));
                        if d1 >= 0.0 && d2 >= 0.0 && d3 >= 0.0 {
                            1
                        } else if d1 <= 0.0 && d2 <= 0.0 && d3 <= 0.0 {
                            -1
                        } else {
                            0
                        }
                    })
                    .sum();
                if winding != 0 {
                    let cell = self.cell(col, row);
                    cell.bg = over(rgb, alpha, cell.bg);
                }
            }
        }
    }

    /// A line too thin for a cell, drawn as a box-drawing character in
    /// cells that hold no text.
    fn rule(
        &mut self,
        cols: std::ops::Range<u16>,
        rows: std::ops::Range<u16>,
        ch: char,
        rgb: Rgb,
        alpha: f32,
    ) {
        for row in rows {
            for col in cols.clone() {
                let cell = self.cell(col, row);
                let fg = over(rgb, alpha, cell.bg);
                // A line in the colour of what is behind it is not seen.
                if cell.ch == ' ' && !cell.tail && fg != cell.bg {
                    cell.ch = ch;
                    cell.fg = fg;
                    cell.bold = false;
                    cell.italic = false;
                }
            }
        }
    }

    fn underline(&mut self, b: Bounds<ScaledPixels>) {
        let row = (b.origin.y.0 / CELL_H).floor();
        if row < 0.0 || row >= self.rows as f32 {
            return;
        }
        for col in span(
            b.origin.x.0,
            b.origin.x.0 + b.size.width.0,
            CELL_W,
            self.cols,
        ) {
            self.cell(col, row as u16).underline = true;
        }
    }

    fn glyph(
        &mut self,
        content: Option<TileContent>,
        bounds: &Bounds<ScaledPixels>,
        mask: &ContentMask<ScaledPixels>,
        color: Option<Color>,
    ) {
        let (x, y) = (bounds.origin.x.0, bounds.origin.y.0);
        let m = &mask.bounds;
        let cols = span(
            m.origin.x.0,
            m.origin.x.0 + m.size.width.0,
            CELL_W,
            self.cols,
        );
        let rows = span(
            m.origin.y.0,
            m.origin.y.0 + m.size.height.0,
            CELL_H,
            self.rows,
        );
        let (col, row) = (edge(x, CELL_W), edge(y, CELL_H));
        if col < 0.0 || row < 0.0 || !cols.contains(&(col as u16)) || !rows.contains(&(row as u16))
        {
            return;
        }
        let (col, row) = (col as u16, row as u16);
        let (ch, font_id) = match content {
            Some(TileContent::Glyph { ch, font_id }) => (ch, font_id),
            Some(TileContent::Picture) => ('▪', 0),
            None => return,
        };
        let (bold, italic) = font_style(gpui::FontId(font_id));
        let wide = char_width(ch) == 2 && col + 1 < self.cols;
        let cell = self.cell(col, row);
        if let Some(color) = color {
            let (rgb, alpha) = rgba(color);
            cell.fg = over(rgb, alpha, cell.bg);
        }
        cell.ch = ch;
        cell.bold = bold;
        cell.italic = italic;
        cell.tail = false;
        if wide {
            let tail = self.cell(col + 1, row);
            tail.ch = ' ';
            tail.tail = true;
        }
    }
}

/// What the terminal shows now, so a frame writes only what changed.
#[derive(Default)]
pub(crate) struct Screen {
    shown: Option<Grid>,
    /// Instead of the terminal, each frame lands here.
    memory: Option<Rc<RefCell<Option<Grid>>>>,
}

impl Screen {
    pub(crate) fn in_memory(slot: Rc<RefCell<Option<Grid>>>) -> Self {
        Self {
            shown: None,
            memory: Some(slot),
        }
    }

    /// Forget what is shown: the next frame repaints every cell.
    pub(crate) fn invalidate(&mut self) {
        self.shown = None;
    }

    pub(crate) fn present(&mut self, grid: Grid) -> io::Result<()> {
        if let Some(slot) = &self.memory {
            *slot.borrow_mut() = Some(grid);
            return Ok(());
        }
        let mut out = io::stdout().lock();
        out.queue(BeginSynchronizedUpdate)?;
        out.queue(Hide)?;
        let previous = self
            .shown
            .as_ref()
            .filter(|shown| shown.cols == grid.cols && shown.rows == grid.rows);
        let mut pen: Option<Cell> = None;
        let mut at: Option<(u16, u16)> = None;
        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let ix = row as usize * grid.cols as usize + col as usize;
                let cell = grid.cells[ix];
                if cell.tail || previous.is_some_and(|p| p.cells[ix] == cell) {
                    continue;
                }
                if at != Some((col, row)) {
                    out.queue(MoveTo(col, row))?;
                }
                if pen.is_none_or(|p| {
                    (p.bold, p.italic, p.underline) != (cell.bold, cell.italic, cell.underline)
                }) {
                    out.queue(SetAttribute(Attribute::Reset))?;
                    if cell.bold {
                        out.queue(SetAttribute(Attribute::Bold))?;
                    }
                    if cell.italic {
                        out.queue(SetAttribute(Attribute::Italic))?;
                    }
                    if cell.underline {
                        out.queue(SetAttribute(Attribute::Underlined))?;
                    }
                    pen = None;
                }
                if pen.is_none_or(|p| p.fg != cell.fg) {
                    let [r, g, b] = cell.fg;
                    out.queue(SetForegroundColor(TermColor::Rgb { r, g, b }))?;
                }
                if pen.is_none_or(|p| p.bg != cell.bg) {
                    let [r, g, b] = cell.bg;
                    out.queue(SetBackgroundColor(TermColor::Rgb { r, g, b }))?;
                }
                pen = Some(cell);
                out.queue(Print(cell.ch))?;
                let width = char_width(cell.ch).max(1) as u16;
                at = Some((col + width, row));
            }
        }
        out.queue(SetAttribute(Attribute::Reset))?;
        if let Some(caret) = grid.caret {
            out.queue(MoveTo(caret.col, caret.row))?;
            out.queue(SetCursorStyle::SteadyBar)?;
            out.queue(Show)?;
        }
        out.queue(EndSynchronizedUpdate)?;
        out.flush()?;
        self.shown = Some(grid);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use gpui::{
        AtlasKey, Bounds, Color, ContentMask, DevicePixels, FontId, GlyphId, MonochromeSprite,
        PlatformAtlas as _, Point, Quad, RenderGlyphParams, ScaledPixels, Scene, Size,
        TransformationMatrix, solid_background,
    };

    use super::*;

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<ScaledPixels> {
        Bounds {
            origin: Point {
                x: ScaledPixels(x),
                y: ScaledPixels(y),
            },
            size: Size {
                width: ScaledPixels(w),
                height: ScaledPixels(h),
            },
        }
    }

    fn everywhere() -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: bounds(0.0, 0.0, 1000.0, 1000.0),
        }
    }

    fn white() -> Color {
        Color::linear_srgb(1.0, 1.0, 1.0, 1.0)
    }

    fn red() -> Color {
        Color::linear_srgb(1.0, 0.0, 0.0, 1.0)
    }

    struct Painter {
        scene: Scene,
        atlas: CellAtlas,
        order: u32,
    }

    impl Painter {
        fn new() -> Self {
            Self {
                scene: Scene::default(),
                atlas: CellAtlas::default(),
                order: 0,
            }
        }

        fn next(&mut self) -> u32 {
            self.order += 1;
            self.order
        }

        /// A glyph the way `Window::paint_glyph` hands it over: its sprite
        /// sits three quarters of a line above the baseline at `x`.
        fn glyph(&mut self, ch: char, x: f32, line_top: f32, mask: ContentMask<ScaledPixels>) {
            let params = RenderGlyphParams {
                font_id: FontId(0),
                glyph_id: GlyphId(ch as u32),
                font_size: gpui::px(16.0),
                subpixel_variant: Default::default(),
                scale_factor: 1.0,
                is_emoji: false,
                subpixel_rendering: false,
                dilation: 0,
            };
            let size = Size {
                width: DevicePixels(8),
                height: DevicePixels(16),
            };
            let tile = self
                .atlas
                .get_or_insert_with(AtlasKey::Glyph(params), &mut || {
                    Ok(Some((size, Cow::Borrowed(&[][..]))))
                })
                .unwrap()
                .unwrap();
            let order = self.next();
            self.scene.monochrome_sprites.push(MonochromeSprite {
                order,
                pad: 0,
                bounds: bounds(x, line_top, 8.0, 16.0),
                content_mask: mask,
                color: white(),
                tile,
                transformation: TransformationMatrix::unit(),
            });
        }

        fn quad(&mut self, b: Bounds<ScaledPixels>, color: Color) {
            let order = self.next();
            self.scene.quads.push(Quad {
                order,
                bounds: b,
                content_mask: everywhere(),
                background: solid_background(color),
                ..Default::default()
            });
        }

        fn paint(&self) -> Grid {
            Grid::paint(&self.scene, &self.atlas, 10, 3)
        }
    }

    fn row(grid: &Grid, row: u16) -> String {
        (0..grid.cols)
            .map(|col| grid.cells[(row * grid.cols + col) as usize].ch)
            .collect()
    }

    /// Text inset 6 px, off the cell grid as gpui's paddings leave it: the
    /// block cursor drawn over a glyph covers that glyph's cell, not the
    /// one beside it, and the glyph drawn over the cursor is the glyph.
    #[test]
    fn a_cursor_and_the_glyph_under_it_share_a_cell() {
        let mut painter = Painter::new();
        for (i, ch) in "abc".chars().enumerate() {
            painter.glyph(ch, 6.0 + 8.0 * i as f32, 16.0, everywhere());
        }
        painter.quad(bounds(14.0, 16.0, 8.0, 16.0), red());
        painter.glyph('b', 14.0, 16.0, everywhere());
        let grid = painter.paint();
        assert_eq!(row(&grid, 1), " abc      ");
        let cursor: Vec<u16> = (0..10)
            .filter(|&col| grid.cells[(10 + col) as usize].bg == [255, 0, 0])
            .collect();
        assert_eq!(cursor, vec![2], "the cursor is behind the `b`");
    }

    /// A clip that starts where the text starts keeps the first glyph,
    /// though the cell that glyph lands in starts left of the clip.
    #[test]
    fn a_clip_at_the_text_edge_keeps_the_first_glyph() {
        let mut painter = Painter::new();
        let mask = ContentMask {
            bounds: bounds(6.0, 0.0, 60.0, 48.0),
        };
        painter.glyph('C', 6.0, 0.0, mask);
        painter.glyph('o', 14.0, 0.0, mask);
        // Outside the clip on the right: dropped.
        painter.glyph('x', 70.0, 0.0, mask);
        assert_eq!(row(&painter.paint(), 0), " Co       ");
    }

    /// A 2 px bar one line tall after the text is drawn as a rule, and
    /// is the editor's caret once the input says its caret is there; an
    /// opaque quad over a glyph hides it.
    #[test]
    fn the_caret_takes_back_its_bar_and_an_opaque_quad_hides_text() {
        let mut painter = Painter::new();
        painter.glyph('a', 6.0, 32.0, everywhere());
        painter.glyph('b', 14.0, 32.0, everywhere());
        painter.quad(bounds(22.0, 32.0, 2.0, 16.0), white());
        painter.quad(bounds(6.0, 32.0, 8.0, 16.0), red());
        let mut grid = painter.paint();
        assert_eq!(row(&grid, 2), "  b│      ", "the opaque quad hid the `a`");
        grid.set_caret(Some(Caret { col: 4, row: 2 }));
        assert_eq!(
            row(&grid, 2),
            "  b│      ",
            "a caret elsewhere leaves the bar"
        );
        grid.set_caret(Some(Caret { col: 3, row: 2 }));
        assert_eq!(row(&grid, 2), "  b       ");
        assert_eq!(grid.caret, Some(Caret { col: 3, row: 2 }));
    }

    /// A bordered box draws its border in its outer cells, and the text
    /// drawn inside it afterwards stays.
    #[test]
    fn a_border_is_box_drawing_in_the_outer_cells() {
        let mut painter = Painter::new();
        let order = painter.next();
        painter.scene.quads.push(Quad {
            order,
            bounds: bounds(8.0, 0.0, 40.0, 48.0),
            content_mask: everywhere(),
            border_color: white(),
            border_widths: gpui::Edges::all(ScaledPixels(1.0)),
            ..Default::default()
        });
        painter.glyph('x', 16.0, 16.0, everywhere());
        painter.glyph('y', 40.0, 16.0, everywhere());
        let grid = painter.paint();
        assert_eq!(
            [row(&grid, 0), row(&grid, 1), row(&grid, 2)],
            [" ┌───┐    ", " │x  y    ", " └───┘    "],
            "text over the border wins"
        );
    }

    /// A rule thinner than a cell is drawn only where there is no text,
    /// and not at all in the colour behind it.
    #[test]
    fn a_rule_draws_in_empty_cells_only() {
        let mut painter = Painter::new();
        painter.glyph('a', 0.0, 16.0, everywhere());
        painter.quad(bounds(2.0, 0.0, 4.0, 48.0), red());
        let grid = painter.paint();
        assert_eq!(
            [row(&grid, 0), row(&grid, 1), row(&grid, 2)].map(|r| r.chars().next().unwrap()),
            ['│', 'a', '│']
        );

        let mut painter = Painter::new();
        painter.quad(bounds(0.0, 0.0, 80.0, 48.0), red());
        painter.quad(bounds(2.0, 0.0, 4.0, 48.0), red());
        assert_eq!(
            row(&painter.paint(), 0),
            " ".repeat(10),
            "a red rule on red is not drawn"
        );
    }
}
