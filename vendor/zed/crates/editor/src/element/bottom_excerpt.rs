use super::*;

pub(crate) struct BottomExcerptText {
    pub(crate) rows: Range<DisplayRow>,
    pub(crate) lines: Vec<LineWithInvisibles>,
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) chrome_top: Pixels,
    pub(crate) top_y: f64,
    pub(crate) max_offset: f64,
}

pub(super) struct BottomExcerptLayout {
    pub(super) text: Rc<BottomExcerptText>,
    origin: gpui::Point<Pixels>,
    selections: Vec<(PlayerColor, Vec<SelectionLayout>)>,
    cursors: Vec<CursorLayout>,
    highlights: Vec<(Range<DisplayPoint>, Hsla)>,
    hints: Vec<AnyElement>,
    notice: Option<ShapedLine>,
}

impl EditorElement {
    pub(super) fn layout_bottom_excerpt(
        &self,
        snapshot: &EditorSnapshot,
        text_hitbox: &Hitbox,
        origin: gpui::Point<Pixels>,
        width: Pixels,
        scroll: gpui::Point<ScrollOffset>,
        pixels: gpui::Point<ScrollPixelOffset>,
        line_height: Pixels,
        em_width: Pixels,
        em_advance: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<BottomExcerptLayout> {
        let height = f64::from(text_hitbox.size.height / line_height);
        let top = snapshot.row_y(scroll.y);
        let (panel, notice, label_rows, start, end, redacted, highlights) =
            self.editor.update(cx, |editor, cx| {
                let rows = editor.bottom_excerpt_range(&snapshot.display_snapshot)?;
                let state = editor.bottom_excerpt.as_ref()?;
                let mut label_rows = 0.0;
                let mut notice = None;
                if snapshot.row_y(rows.start.as_f64()) >= top + height {
                    let text = state.label.clone();
                    let label = window.text_system().shape_line(
                        text.clone(),
                        self.style.text.font_size.to_pixels(window.rem_size()),
                        &[TextRun {
                            len: text.len(),
                            font: self.style.text.font(),
                            color: cx.theme().colors().text_muted.into(),
                            ..Default::default()
                        }],
                        None,
                    );
                    // Reserve this space whether or not mail is pending, so a
                    // notice never obscures a draft or changes what history is visible.
                    let first_row_end =
                        DisplayPoint::new(rows.start, snapshot.line_len(rows.start));
                    let text_width = snapshot.x_for_display_point(
                        first_row_end,
                        &editor.text_layout_details(window, cx),
                    );
                    let needs_own_row = rows.len() > 1
                        || text_width + label.width + 2.0 * em_advance > text_hitbox.size.width;
                    if needs_own_row && height >= 4.0 {
                        label_rows = 1.0;
                    }
                    if state.pending && (!needs_own_row || label_rows > 0.0) {
                        notice = Some(label);
                    }
                }
                let panel = editor.layout_bottom_excerpt_rows(
                    &snapshot.display_snapshot,
                    top,
                    height,
                    label_rows,
                )?;
                let start = snapshot
                    .display_point_to_anchor(DisplayPoint::new(panel.rows.start, 0), Bias::Left);
                let end = if panel.rows.end > snapshot.max_point().row() {
                    Anchor::Max
                } else {
                    snapshot
                        .display_point_to_anchor(DisplayPoint::new(panel.rows.end, 0), Bias::Right)
                };
                let highlights = editor.background_highlights_in_range(
                    start..end,
                    &snapshot.display_snapshot,
                    cx.theme(),
                );
                let redacted = editor.redacted_ranges(start..end, &snapshot.display_snapshot, cx);
                Some((panel, notice, label_rows, start, end, redacted, highlights))
            })?;
        let rows = panel.rows;
        if rows.is_empty() {
            return None;
        }
        let infos = snapshot
            .row_infos(rows.start)
            .take(rows.len())
            .collect::<Vec<_>>();
        let lines = Self::layout_lines(
            rows.clone(),
            snapshot,
            &self.style,
            width,
            |row| infos.get(row).is_none_or(|info| info.buffer_row.is_none()),
            &[],
            window,
            cx,
        );
        let mut hitbox = text_hitbox.clone();
        hitbox.bounds.size.height = line_height * panel.height as f32;
        hitbox.bounds.origin.y = text_hitbox.bottom() - hitbox.bounds.size.height;
        let origin = point(origin.x, hitbox.top());
        let scroll = point(scroll.x, snapshot.row_at_y(panel.top_y));
        let pixels = point(pixels.x, panel.top_y * f64::from(line_height));
        // The ordinary viewport does not collect selections in these
        // rows. Collect them just as we do for the history viewport.
        let local_selections = self
            .editor_with_selections(cx)
            .map_or_else(Vec::new, |editor| {
                editor.update(cx, |editor, _| {
                    let mut selections = editor
                        .selections
                        .disjoint_in_row_range(start..end, &snapshot.display_snapshot);
                    selections.extend(editor.selections.pending(&snapshot.display_snapshot));
                    selections
                })
            });
        let (selections, _, _) = self.layout_selections(
            start,
            end,
            &local_selections,
            snapshot,
            rows.start,
            rows.end,
            window,
            cx,
        );
        let cursors = self.layout_visible_cursors(
            snapshot,
            &selections,
            &HashMap::default(),
            rows.clone(),
            &lines,
            &hitbox,
            origin,
            scroll,
            pixels,
            line_height,
            em_width,
            em_advance,
            false,
            &redacted,
            false,
            window,
            cx,
        );
        let hints = self.layout_eol_hints(
            snapshot,
            &lines,
            rows.start,
            rows.end,
            em_width,
            origin,
            scroll,
            pixels,
            line_height,
            window,
            cx,
        );
        Some(BottomExcerptLayout {
            text: Rc::new(BottomExcerptText {
                rows,
                lines,
                bounds: hitbox.bounds,
                chrome_top: hitbox.top() - line_height * label_rows as f32,
                top_y: panel.top_y,
                max_offset: panel.max_offset,
            }),
            origin,
            selections,
            cursors,
            highlights,
            hints,
            notice,
        })
    }

    pub(super) fn paint_bottom_excerpt(
        &self,
        layout: &mut EditorLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(mut panel) = layout.bottom_excerpt.take() else {
            return;
        };
        let line_height = layout.position_map.line_height;
        let snapshot = &layout.position_map.snapshot;
        let bounds = Bounds::from_corners(
            point(layout.hitbox.left(), panel.text.chrome_top),
            point(layout.hitbox.right(), panel.text.bounds.bottom()),
        );
        let whitespaces = self
            .editor
            .read(cx)
            .buffer
            .read(cx)
            .language_settings(cx)
            .show_whitespaces;
        let text_origin = panel.origin
            + point(
                px(0.),
                line_height
                    * (snapshot.row_y(layout.position_map.scroll_position.y) - panel.text.top_y)
                        as f32,
            );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_layer(|window| {
                window.paint_quad(fill(bounds, self.style.background));
                for (row, line) in panel.text.rows.iter_rows().zip(&panel.text.lines) {
                    line.draw_background(layout, row, text_origin, window, cx);
                }
                let rows = || PaintedTextRows {
                    range: &panel.text.rows,
                    lines: &panel.text.lines,
                    snapshot,
                    origin: panel.origin,
                    bounds: panel.text.bounds,
                    line_height,
                    scroll_x: layout.position_map.scroll_pixel_position.x,
                    top_y: panel.text.top_y,
                    align: layout.text_align,
                };
                let overshoot = layout.line_end_overshoot();
                for (range, color) in &panel.highlights {
                    self.paint_highlighted_range_in(
                        range.clone(),
                        true,
                        *color,
                        px(0.),
                        overshoot,
                        rows(),
                        window,
                    );
                }
                let mut ranges = SmallVec::<[Range<DisplayPoint>; 32]>::new();
                for (player, selections) in &panel.selections {
                    for selection in selections {
                        self.paint_highlighted_range_in(
                            selection.range.clone(),
                            true,
                            player.selection.into(),
                            px(0.),
                            overshoot,
                            rows(),
                            window,
                        );
                        if selection.is_local && !selection.range.is_empty() {
                            ranges.push(selection.range.clone());
                        }
                    }
                }
                for (row, line) in panel.text.rows.iter_rows().zip(&panel.text.lines) {
                    line.draw(layout, row, text_origin, whitespaces, &ranges, window, cx);
                }
                for cursor in &mut panel.cursors {
                    cursor.paint(panel.origin, window, cx);
                }
                for hint in &mut panel.hints {
                    hint.paint(window, cx);
                }
                if let Some(notice) = panel.notice {
                    let right = panel.text.bounds.right() - layout.position_map.em_advance;
                    let at = point(right - notice.width, panel.text.chrome_top);
                    window.paint_quad(fill(
                        Bounds::new(
                            at,
                            size(notice.width + layout.position_map.em_advance, line_height),
                        ),
                        self.style.background,
                    ));
                    notice
                        .paint(at, line_height, TextAlign::Left, None, window, cx)
                        .log_err();
                }
            });
        });
    }
}
