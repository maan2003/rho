use editor::RowExt;
use editor::display_map::Crease;
use text::Bias;

use super::*;

#[gpui::test]
fn a_gap_anchor_follows_its_source_row_through_a_prepend(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text("first\nsecond\nthird", window, cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchor = snapshot.anchor_after(language::Point::new(1, 6));
        editor.set_row_spacing(
            vec![editor::display_map::RowSpacing {
                range: anchor..anchor,
                minimum_height: 0.,
                gap_after: 0.5,
            }],
            cx,
        );
        editor
    });

    editor
        .update(cx, |editor, _, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit(
                    [(
                        multi_buffer::MultiBufferOffset(0)..multi_buffer::MultiBufferOffset(0),
                        "prefix\n",
                    )],
                    None,
                    cx,
                );
            });
            let snapshot = editor.display_snapshot(cx);
            let second_end = snapshot
                .point_to_display_point(language::Point::new(2, 6), Bias::Left)
                .row();
            let third = snapshot
                .point_to_display_point(language::Point::new(3, 0), Bias::Left)
                .row();
            assert_eq!(
                snapshot.row_y(third.as_f64()) - snapshot.row_y(second_end.as_f64()),
                1.5
            );
        })
        .unwrap();
}

#[gpui::test]
fn a_gap_follows_the_last_visual_row_after_soft_wrap(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text(format!("{}\nnext", "wrapped ".repeat(8)), window, cx);
        editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchor = snapshot.anchor_after(language::Point::new(
            0,
            snapshot.line_len(multi_buffer::MultiBufferRow(0)),
        ));
        editor.set_row_spacing(
            vec![editor::display_map::RowSpacing {
                range: snapshot.anchor_before(language::Point::new(0, 0))..anchor,
                minimum_height: 1.25,
                gap_after: 0.5,
            }],
            cx,
        );
        editor
    });
    cx.simulate_window_resize(*editor, size(px(220.), px(400.)));
    cx.run_until_parked();
    cx.update_window(*editor, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();

    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.display_snapshot(cx);
            let buffer = snapshot.buffer_snapshot();
            let last_wrapped = snapshot
                .point_to_display_point(
                    language::Point::new(0, buffer.line_len(multi_buffer::MultiBufferRow(0))),
                    Bias::Left,
                )
                .row();
            let next = snapshot
                .point_to_display_point(language::Point::new(1, 0), Bias::Left)
                .row();
            assert_eq!(snapshot.row_padding_before(DisplayRow(0)), 0.);
            assert!(last_wrapped.0 > 0, "the source row must actually soft-wrap");
            assert!(next > last_wrapped);
            assert_eq!(
                snapshot.row_y(next.as_f64()) - snapshot.row_y(last_wrapped.as_f64()),
                1.5
            );
        })
        .unwrap();
    for (width, expected_padding) in [(1800., 0.125), (220., 0.)] {
        cx.simulate_window_resize(*editor, size(px(width), px(400.)));
        cx.run_until_parked();
        cx.draw_window(*editor);
        editor
            .update(cx, |editor, _, cx| {
                let snapshot = editor.display_snapshot(cx);
                assert_eq!(
                    snapshot.row_padding_before(DisplayRow(0)),
                    expected_padding,
                    "centering follows the visual row count at width {width}"
                );
            })
            .unwrap();
    }
}

enum RowSpacingFold {}

#[gpui::test]
fn a_fold_end_row_keeps_its_gap_while_hidden_rows_do_not_pile_onto_it(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text("author\nheader\nbody\nnext", window, cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let hidden = snapshot.anchor_after(language::Point::new(1, 6));
        let body = snapshot.anchor_after(language::Point::new(2, 4));
        editor.display_map.update(cx, |map, cx| {
            map.fold(
                vec![Crease::simple(
                    snapshot.anchor_before(language::Point::new(0, 0))
                        ..snapshot.anchor_after(language::Point::new(2, 0)),
                    editor::FoldPlaceholder::concealed(std::any::TypeId::of::<RowSpacingFold>()),
                )],
                cx,
            );
            map.set_row_spacing(
                vec![
                    editor::display_map::RowSpacing {
                        range: hidden..hidden,
                        minimum_height: 0.,
                        gap_after: 0.75,
                    },
                    editor::display_map::RowSpacing {
                        range: body..body,
                        minimum_height: 0.,
                        gap_after: 0.5,
                    },
                ],
                cx,
            );
        });
        editor
    });

    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.display_snapshot(cx);
            let body = snapshot
                .point_to_display_point(language::Point::new(2, 4), Bias::Left)
                .row();
            let next = snapshot
                .point_to_display_point(language::Point::new(3, 0), Bias::Left)
                .row();
            assert_eq!(
                snapshot.row_y(next.as_f64()) - snapshot.row_y(body.as_f64()),
                1.5
            );
        })
        .unwrap();
}

#[gpui::test]
fn trailing_spacing_stays_after_attachment_blocks(cx: &mut TestAppContext) {
    use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle};
    use gpui::{IntoElement as _, Styled as _};
    cx.update(init_test_app);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text("attachment\nnext", window, cx);
        editor
    });
    editor
        .update(cx, |editor, _, cx| {
            let source = editor.buffer().read(cx).snapshot(cx);
            let anchor = source.anchor_after(language::Point::new(0, 10));
            let ids = editor.insert_blocks(
                vec![BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(2),
                    style: BlockStyle::Fixed,
                    render: Arc::new(|cx| gpui::div().h(cx.line_height * 2.).into_any_element()),
                    priority: 0,
                }],
                None,
                cx,
            );
            editor.set_row_spacing(
                vec![editor::display_map::RowSpacing {
                    range: anchor..anchor,
                    minimum_height: 0.,
                    gap_after: 0.5,
                }],
                cx,
            );
            let snapshot = editor.display_snapshot(cx);
            assert_eq!(snapshot.row_y(1.), 1., "no gap between caption and image");
            assert_eq!(snapshot.row_y(2.), 2., "no gap inside the image");
            assert_eq!(
                snapshot.row_y(3.),
                3.5,
                "gap follows the image, before next message"
            );
            editor.remove_blocks(ids.into_iter().collect(), None, cx);
            assert_eq!(editor.display_snapshot(cx).row_y(1.), 1.5);
            editor.set_row_spacing(vec![], cx);
            assert_eq!(editor.display_snapshot(cx).row_y(1.), 1.);
        })
        .unwrap();
}

#[gpui::test]
fn avatar_minimum_height_does_not_add_a_blank_row_to_multiline_messages(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text("short\nlong\nbody\nnext", window, cx);
        let source = editor.buffer().read(cx).snapshot(cx);
        let at = |row, col| source.anchor_after(language::Point::new(row, col));
        editor.set_row_spacing(
            vec![
                editor::display_map::RowSpacing {
                    range: at(0, 0)..at(0, 5),
                    minimum_height: 1.25,
                    gap_after: 0.5,
                },
                editor::display_map::RowSpacing {
                    range: at(1, 0)..at(2, 4),
                    minimum_height: 1.25,
                    gap_after: 0.5,
                },
            ],
            cx,
        );
        editor
    });
    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert_eq!(
                snapshot.row_y(1.),
                1.75,
                "one-line avatar slot + half-line gap, without multiline top padding"
            );
            assert_eq!(
                snapshot.row_y(2.),
                2.75,
                "internal body lines remain consecutive"
            );
            assert_eq!(
                snapshot.row_y(3.),
                4.25,
                "multiline body adds only the half-line gap"
            );
            assert_eq!(
                snapshot.row_y(0.),
                0.125,
                "short text has equal top and bottom padding"
            );
            assert_eq!(snapshot.row_padding_before(DisplayRow(0)), 0.125);
            assert_eq!(snapshot.row_padding_before(DisplayRow(1)), 0.);
            for row in [0., 0.5, 1., 1.75, 2., 3.] {
                assert!((snapshot.row_at_y(snapshot.row_y(row)) - row).abs() < 1e-9);
            }
            assert_eq!(snapshot.text(), "short\nlong\nbody\nnext");
        })
        .unwrap();
}

#[gpui::test]
fn anchored_hanging_indents_wrap_resize_and_hit_test_without_changing_source(
    cx: &mut TestAppContext,
) {
    cx.update(init_test_app);
    let text = concat!(
        "9. alpha bravo charlie delta echo foxtrot golf hotel india juliet\n",
        "10. kilo lima mike november oscar papa quebec romeo sierra tango\n",
        "   1. nested uniform victor whiskey xray yankee zulu alpha bravo\n",
        "plain charlie delta echo foxtrot golf hotel india juliet kilo lima"
    );
    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text(text, window, cx);
        editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
        let source = editor.buffer().read(cx).snapshot(cx);
        let range = |row| {
            source.anchor_before(language::Point::new(row, 0))
                ..source.anchor_after(language::Point::new(
                    row,
                    source.line_len(multi_buffer::MultiBufferRow(row)),
                ))
        };
        editor.set_hanging_indents(vec![(range(0), 3), (range(1), 4), (range(2), 6)], cx);
        editor
    });

    for width in [300., 210.] {
        cx.simulate_window_resize(*window, size(px(width), px(600.)));
        cx.run_until_parked();
        cx.draw_window(*window);
        window
            .update(cx, |editor, window, cx| {
                let snapshot = editor.snapshot(window, cx);
                assert_eq!(snapshot.buffer_snapshot().text(), text);

                for (source_row, indent) in [(0, 3), (1, 4), (2, 6), (3, 0)] {
                    let line_len = snapshot
                        .buffer_snapshot()
                        .line_len(multi_buffer::MultiBufferRow(source_row));
                    let points = (0..=line_len)
                        .map(|column| {
                            snapshot.display_snapshot.point_to_display_point(
                                language::Point::new(source_row, column),
                                Bias::Right,
                            )
                        })
                        .collect::<Vec<_>>();
                    let first_row = points[0].row();
                    let last_row = points.last().unwrap().row();
                    let starts = (first_row.0..=last_row.0)
                        .map(|row| {
                            points
                                .iter()
                                .position(|point| point.row().0 == row)
                                .unwrap()
                        })
                        .collect::<Vec<_>>();
                    let expected_starts: &[usize] = match (width as u32, source_row) {
                        (300, 0) => &[0, 29, 53],
                        (300, 1) => &[0, 28, 52],
                        (300, 2) => &[0, 28, 48],
                        (300, 3) => &[0, 25, 50],
                        (210, 0) => &[0, 15, 29, 42, 59],
                        (210, 1) => &[0, 19, 34, 46, 59],
                        (210, 2) => &[0, 13, 27, 41, 53],
                        (210, 3) => &[0, 20, 38, 57],
                        _ => unreachable!(),
                    };
                    assert_eq!(
                        starts, expected_starts,
                        "indent width must reduce continuation capacity"
                    );
                    assert!(
                        last_row > first_row,
                        "source row {source_row} must wrap at width {width}"
                    );
                    for visual_row in (first_row.0 + 1)..=last_row.0 {
                        let first = points
                            .iter()
                            .find(|point| point.row().0 == visual_row)
                            .unwrap();
                        assert_eq!(
                            first.column(),
                            indent,
                            "source row {source_row}, visual row {visual_row}, width {width}"
                        );
                    }
                }

                let source = snapshot.buffer_snapshot();
                let continuation = (0..=source.line_len(multi_buffer::MultiBufferRow(1)))
                    .map(|column| {
                        (
                            column,
                            snapshot.display_snapshot.point_to_display_point(
                                language::Point::new(1, column),
                                Bias::Right,
                            ),
                        )
                    })
                    .find(|(_, point)| {
                        point.row().0
                            > snapshot
                                .display_snapshot
                                .point_to_display_point(language::Point::new(1, 0), Bias::Right)
                                .row()
                                .0
                    })
                    .unwrap();
                assert_eq!(continuation.1.column(), 4);

                let position = editor
                    .window_position_for_display_point(continuation.1, &snapshot, window, cx)
                    .unwrap()
                    + point(px(1.), px(1.));
                let (_, _, hit) = editor
                    .buffer_location_for_window_position(position, Bias::Left)
                    .unwrap();
                let expected = text.lines().next().unwrap().len() + 1 + continuation.0 as usize;
                assert_eq!(hit, expected, "continuation click at width {width}");
            })
            .unwrap();
    }
}
