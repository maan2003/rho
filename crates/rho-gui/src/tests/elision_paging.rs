//! What happens to the elisions already on screen when history arrives.
//!
//! A spec exists only while its anchors resolve, so a page composed at the
//! head makes several earlier plans resolve at once and the specs already
//! there all shift down the list. Paired by position, that reads as every
//! elision having changed shape: on the walk gate's elided drive one page
//! updated all sixty-one of them, and the block map was told most of the
//! document had moved when nothing below the page had.

use editor::Editor;
use gpui::{AppContext as _, TestAppContext};
use rho_agents::transcript::elisions::{ElisionSpec, ElisionState, ElisionSync};

use super::{history_elisions, init_test_app};

/// An elision belongs to its turn, not to its place in the list.
#[gpui::test]
fn history_arriving_leaves_the_elisions_below_it_alone(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let turn = |name: &str| {
        let text = (0..12)
            .map(|row| format!("line {row} of {name}"))
            .collect::<Vec<_>>()
            .join("\n");
        cx.update(|cx| cx.new(|cx| language::Buffer::local(text, cx)))
    };
    let (page, first, second) = (turn("the page"), turn("the first"), turn("the second"));
    let excerpt = |multi_buffer: &mut multi_buffer::MultiBuffer,
                   key: u64,
                   buffer: &gpui::Entity<language::Buffer>,
                   cx: &mut gpui::Context<multi_buffer::MultiBuffer>| {
        multi_buffer.set_excerpts_for_path(
            multi_buffer::PathKey::sorted(key),
            buffer.clone(),
            [language::Point::zero()..buffer.read(cx).max_point()],
            0,
            cx,
        );
    };
    // The page is not composed yet, so its spec's anchors resolve to
    // nothing and only the two turns below it become elisions.
    let multi_buffer = cx.update(|cx| {
        cx.new(|cx| {
            let mut multi_buffer =
                multi_buffer::MultiBuffer::without_headers(language::Capability::ReadWrite);
            excerpt(&mut multi_buffer, 1, &first, cx);
            excerpt(&mut multi_buffer, 2, &second, cx);
            multi_buffer
        })
    });
    let window = cx.add_window(|window, cx| {
        Editor::new(
            editor::EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: true,
                show_active_line_background: false,
                sizing_behavior: editor::SizingBehavior::ExcludeOverscrollMargin,
            },
            multi_buffer.clone(),
            None,
            window,
            cx,
        )
    });
    let editor = window.root(cx).expect("editor");
    let host = cx.update(|cx| cx.new(|_| ()));
    let mut sync = ElisionSync::default();
    let mut state = ElisionState::default();

    let specs = cx.update(|cx| {
        [(0, &page), (3, &first), (6, &second)]
            .into_iter()
            .map(|(start_block, buffer)| {
                let buffer = buffer.read(cx);
                ElisionSpec {
                    start_block,
                    range: buffer.anchor_before(0)..buffer.anchor_after(buffer.len()),
                    tool_count: 2,
                    tail_rows: 0,
                }
            })
            .collect::<Vec<_>>()
    });
    sync.set_specs(specs.clone());
    let mut reconcile = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            host.update(cx, |_, cx| {
                sync.apply(&mut state, &multi_buffer, &editor, cx)
            })
        })
    };
    reconcile(cx);
    let elided = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                let ids = snapshot
                    .blocks_in_range(
                        editor::display_map::DisplayRow(0)..snapshot.max_point().row() + 1,
                    )
                    .filter_map(|(_, block)| match block {
                        editor::display_map::Block::DisplayElision(elision) => Some(elision.id),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                (ids, snapshot.text())
            })
        })
    };
    let (settled, _) = elided(cx);
    assert_eq!(settled.len(), 2, "the two composed turns are elided");

    // The reader opens the first of them. Which elision is open is the
    // editor's own state, held against the elision's id.
    cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            editor.set_display_elisions_expanded(
                [settled[0]].into_iter().collect(),
                true,
                None,
                cx,
            );
        })
    });
    let (_, opened) = elided(cx);
    assert!(
        opened.contains("line 5 of the first"),
        "the turn the reader opened is not showing: {opened}"
    );

    // The page composes, which is an excerpt arriving above everything the
    // editor is already carrying.
    cx.update(|cx| multi_buffer.update(cx, |multi_buffer, cx| excerpt(multi_buffer, 0, &page, cx)));
    reconcile(cx);

    // An open elision is not a block, so the two blocks are the page and
    // the turn below the one the reader opened.
    let (paged, text) = elided(cx);
    assert_eq!(paged.len(), 2, "the page and the second turn are elided");
    assert!(
        text.contains("line 5 of the first"),
        "the turn the reader opened closed when history arrived: {text}"
    );
    assert!(
        !text.contains("line 5 of the page"),
        "the page opened itself with the elision the reader had opened: {text}"
    );
    assert!(
        !text.contains("line 5 of the second"),
        "a turn the reader never opened is showing: {text}"
    );
}
