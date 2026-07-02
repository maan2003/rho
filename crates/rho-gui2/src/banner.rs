//! The rho startup banner block, shown at the top of chat editors.

use std::sync::Arc;

use editor::Editor;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use gpui::prelude::*;
use gpui::{App, Entity, FontWeight, div, px, svg};
use language::Point;
use multi_buffer::MultiBuffer;
use theme::ActiveTheme as _;

pub fn insert(editor: &Entity<Editor>, multi_buffer: &Entity<MultiBuffer>, cx: &mut App) {
    let anchor = multi_buffer
        .read(cx)
        .snapshot(cx)
        .anchor_before(Point::new(0, 0));
    let version = format!("rho {}", env!("CARGO_PKG_VERSION"));
    let pun = startup_pun().to_owned();
    editor.update(cx, |editor, cx| {
        editor.insert_blocks(
            [BlockProperties {
                placement: BlockPlacement::Above(anchor),
                height: Some(4),
                style: BlockStyle::Fixed,
                render: Arc::new(move |cx| {
                    render_banner_block(&version, &pun, cx).into_any_element()
                }),
                priority: 0,
            }],
            None,
            cx,
        );
    });
}

const STARTUP_PUNS: &[&str] = &[
    "Rho is ready.",
    "Rows, roles, and rho.",
    "Rho-native, Unix-shaped.",
    "A small symbol for a large context.",
    "rho marks the prompt.",
    "Good tools, tight loops.",
    "Protocol first, pixels last.",
    "Streaming at terminal speed.",
    "A fresh path through the graph.",
    "Keep the context flowing.",
];

fn startup_pun() -> &'static str {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as usize)
        .unwrap_or(0);
    STARTUP_PUNS[nanos % STARTUP_PUNS.len()]
}

fn render_banner_block(version: &str, pun: &str, cx: &mut BlockContext<'_, '_>) -> impl IntoElement {
    let colors = cx.theme().colors();
    let text_style = cx.editor_style.text.clone();
    div()
        .block_mouse_except_scroll()
        .pl(cx.anchor_x)
        .ml(px(6.))
        .h(px(64.))
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            svg()
                .path("icons/rho.svg")
                .w(px(31.))
                .h(px(48.))
                .text_color(colors.text_accent),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(0.))
                .font_family(text_style.font_family.clone())
                .text_size(text_style.font_size)
                .line_height(text_style.line_height)
                .text_color(text_style.color)
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(6.))
                        .child(div().font_weight(FontWeight::BOLD).child("rho"))
                        .child(
                            div()
                                .text_color(text_style.color.opacity(0.7))
                                .child(version.trim_start_matches("rho").to_owned()),
                        ),
                )
                .child(
                    div()
                        .text_color(text_style.color.opacity(0.7))
                        .child(pun.to_owned()),
                ),
        )
}
