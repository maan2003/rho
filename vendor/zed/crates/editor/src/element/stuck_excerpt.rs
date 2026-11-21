use super::*;

const SEAM_WIDTH: Pixels = px(1.);

// The band is part of the projection, not a display row.
fn top_band_height(line_height: Pixels) -> Pixels {
    line_height / 2. + px(2.) + SEAM_WIDTH
}

pub(crate) struct StuckExcerptText {
    pub(crate) rows: Range<DisplayRow>,
    pub(crate) lines: Vec<LineWithInvisibles>,
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) top_y: f64,
    pub(crate) max_offset: f64,
}

pub(super) struct StuckExcerptLayout {
    pub(super) text: Rc<StuckExcerptText>,
    pub(super) edge: StuckEdge,
    pub(super) chrome: Bounds<Pixels>,
    background: Hsla,
    gutter: Vec<(Range<DisplayPoint>, Hsla)>,
    origin: gpui::Point<Pixels>,
    selections: Vec<(PlayerColor, Vec<SelectionLayout>)>,
    cursors: Vec<CursorLayout>,
    highlights: Vec<(Range<DisplayPoint>, Hsla)>,
    hints: Vec<AnyElement>,
    notice: Option<ShapedLine>,
}

impl EditorElement {
    pub(super) fn layout_stuck_excerpt(
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
    ) -> Option<StuckExcerptLayout> {
        let height = f64::from(text_hitbox.size.height / line_height);
        let top = snapshot.row_y(scroll.y);
        let (
            panel,
            edge,
            background,
            notice,
            chrome_height,
            start,
            end,
            redacted,
            highlights,
            gutter,
        ) = self.editor.update(cx, |editor, cx| {
            let rows = editor.stuck_excerpt_range(&snapshot.display_snapshot)?;
            let state = editor.stuck_excerpt.as_ref()?;
            let edge = state.edge;
            let background = (state.background)(cx);
            let mut chrome_height = match edge {
                StuckEdge::Top => top_band_height(line_height),
                StuckEdge::Bottom => px(0.),
            };
            let mut notice = None;
            if (state.pending || edge == StuckEdge::Bottom)
                && edge.is_outside(
                    snapshot.row_y(rows.start.as_f64()),
                    snapshot.row_y(rows.end.as_f64()),
                    top,
                    height,
                )
            {
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
                if edge == StuckEdge::Top {
                    notice = Some(label);
                } else {
                    // A bottom projection reserves a row if the label
                    // wouldn't fit beside the draft, even before mail lands.
                    let first_row_end =
                        DisplayPoint::new(rows.start, snapshot.line_len(rows.start));
                    let text_width = snapshot.x_for_display_point(
                        first_row_end,
                        &editor.text_layout_details(window, cx),
                    );
                    let needs_own_row = rows.len() > 1
                        || text_width + label.width + 2.0 * em_advance > text_hitbox.size.width;
                    if needs_own_row && height >= 4.0 {
                        chrome_height = line_height;
                    }
                    if state.pending && (!needs_own_row || chrome_height > px(0.)) {
                        notice = Some(label);
                    }
                }
            }
            let panel = editor.layout_stuck_excerpt_rows(
                &snapshot.display_snapshot,
                top,
                height,
                f64::from(chrome_height / line_height),
            )?;
            let start = snapshot
                .display_point_to_anchor(DisplayPoint::new(panel.rows.start, 0), Bias::Left);
            let end = if panel.rows.end > snapshot.max_point().row() {
                Anchor::Max
            } else {
                snapshot.display_point_to_anchor(DisplayPoint::new(panel.rows.end, 0), Bias::Right)
            };
            let highlights = editor.background_highlights_in_range(
                start..end,
                &snapshot.display_snapshot,
                cx.theme(),
            );
            let redacted = editor.redacted_ranges(start..end, &snapshot.display_snapshot, cx);
            let gutter =
                editor.gutter_highlights_in_range(start..end, &snapshot.display_snapshot, cx);
            // A highlight touching the end of the search range may
            // belong to the following mail, not to this projection.
            let gutter = if edge == StuckEdge::Top {
                gutter
                    .into_iter()
                    .filter(|(range, _)| rows.contains(&range.start.row()))
                    .collect()
            } else {
                gutter
            };
            Some((
                panel,
                edge,
                background,
                notice,
                chrome_height,
                start,
                end,
                redacted,
                highlights,
                gutter,
            ))
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
        hitbox.bounds.origin.y = match edge {
            StuckEdge::Top => text_hitbox.top(),
            StuckEdge::Bottom => text_hitbox.bottom() - hitbox.bounds.size.height,
        };
        let chrome = match edge {
            StuckEdge::Top => Bounds::from_corners(
                hitbox.bounds.origin,
                hitbox.bounds.bottom_right() + point(px(0.), chrome_height),
            ),
            StuckEdge::Bottom => Bounds::from_corners(
                hitbox.bounds.origin - point(px(0.), chrome_height),
                hitbox.bounds.bottom_right(),
            ),
        };
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
        Some(StuckExcerptLayout {
            text: Rc::new(StuckExcerptText {
                rows,
                lines,
                bounds: hitbox.bounds,
                top_y: panel.top_y,
                max_offset: panel.max_offset,
            }),
            edge,
            chrome,
            background,
            gutter,
            origin,
            selections,
            cursors,
            highlights,
            hints,
            notice,
        })
    }

    pub(super) fn paint_stuck_excerpt(
        &self,
        layout: &mut EditorLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(mut panel) = layout.stuck_excerpt.take() else {
            return;
        };
        let line_height = layout.position_map.line_height;
        let snapshot = &layout.position_map.snapshot;
        let bounds = Bounds::from_corners(
            point(layout.hitbox.left(), panel.chrome.top()),
            point(layout.hitbox.right(), panel.chrome.bottom()),
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
        if panel.edge == StuckEdge::Top {
            let below = Bounds::from_corners(
                point(bounds.left(), bounds.bottom()),
                layout.hitbox.bounds.bottom_right(),
            );
            window.with_content_mask(Some(ContentMask { bounds: below }), |window| {
                let shadow = gpui::BoxShadow::new(px(0.), px(4.), gpui::black().opacity(0.35))
                    .blur_radius(px(12.));
                window.paint_drop_shadows(bounds, Corners::default(), &[shadow]);
            });
        }
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_layer(|window| {
                window.paint_quad(fill(bounds, panel.background));
                match panel.edge {
                    StuckEdge::Top => {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.left(), bounds.bottom() - SEAM_WIDTH),
                                size(bounds.size.width, SEAM_WIDTH),
                            ),
                            cx.theme().colors().border_variant,
                        ));
                        if let Some((_, color)) = panel.gutter.first() {
                            let (left, _) = self.gutter_highlight_geometry(layout, cx);
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(left.round(), bounds.top()),
                                    size(px(2.), bounds.size.height),
                                ),
                                *color,
                            ));
                        }
                    }
                    StuckEdge::Bottom => self.paint_gutter_highlight_ranges(
                        &panel.gutter,
                        &panel.text.rows,
                        panel.text.bounds.top(),
                        panel.text.top_y * f64::from(line_height),
                        layout,
                        window,
                        cx,
                    ),
                }
                let text_bounds = Bounds::new(
                    point(bounds.left(), panel.text.bounds.top()),
                    size(bounds.size.width, panel.text.bounds.size.height),
                );
                let text_mask = (panel.edge == StuckEdge::Top).then_some(ContentMask {
                    bounds: text_bounds,
                });
                window.with_content_mask(text_mask, |window| {
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
                });
                if let Some(notice) = panel.notice {
                    let right = panel.text.bounds.right() - layout.position_map.em_advance;
                    let y = match panel.edge {
                        StuckEdge::Top => panel.text.bounds.bottom(),
                        StuckEdge::Bottom => panel.chrome.top(),
                    };
                    let at = point(right - notice.width, y);
                    let notice_height = match panel.edge {
                        StuckEdge::Top => line_height / 2.,
                        StuckEdge::Bottom => {
                            window.paint_quad(fill(
                                Bounds::new(
                                    at,
                                    size(
                                        notice.width + layout.position_map.em_advance,
                                        line_height,
                                    ),
                                ),
                                panel.background,
                            ));
                            line_height
                        }
                    };
                    notice
                        .paint(at, notice_height, TextAlign::Left, None, window, cx)
                        .log_err();
                }
            });
        });
    }
}
