//! The left rail listing agents grouped by topic.
//!
//! Topics are ad-hoc tab groups; every topic — including the daemon-created
//! "default" one that agents are born into — renders uniformly with its
//! name as the header, which advertises that grouping exists. Clicking an
//! agent opens it (loading it on demand); the `+` row opens the draft
//! compose view and doubles as its selection indicator.

use std::collections::BTreeSet;

use gpui::prelude::*;
use gpui::{Context, Div, MouseButton, TextStyle, div, px};
use rho_ui_proto::{AgentId, UiTopic};
use theme::ActiveTheme as _;
use ui::{Color, Icon, IconName, IconSize};

use crate::registry::AgentRegistry;
use crate::workspace::Workspace;

pub fn render_topic_rail(
    registry: &AgentRegistry,
    text_style: &TextStyle,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let (selected_color, border_color) = {
        let colors = cx.theme().colors();
        (
            colors.terminal_ansi_magenta,
            colors.border_variant.opacity(0.6),
        )
    };
    let selected_agent = registry.selected().cloned();
    let live = registry.live_agents().cloned().collect::<BTreeSet<_>>();

    let rows = registry
        .topics()
        .iter()
        .map(|topic| {
            render_topic_rows(
                topic,
                selected_agent.as_ref(),
                &live,
                text_style,
                selected_color,
                cx,
            )
        })
        .collect::<Vec<_>>();

    div()
        .id("rho-gui2-topic-rail")
        .h_full()
        .w(px(224.))
        .flex_none()
        .border_r_1()
        .border_color(border_color)
        .pr(px(6.))
        .py(px(2.))
        .overflow_hidden()
        .flex()
        .flex_col()
        .font_family(text_style.font_family.clone())
        .text_size(text_style.font_size)
        .line_height(text_style.line_height)
        .text_color(text_style.color)
        .child(
            div()
                .id("rho-gui2-topic-list")
                .w_full()
                .flex_grow(1.0)
                .overflow_y_scroll()
                .children(rows),
        )
        // Pinned below the scrolling list, so it stays reachable no matter
        // how many agents accumulate.
        .child(new_agent_row(
            selected_agent.is_none(),
            text_style,
            selected_color,
            cx,
        ))
}

fn new_agent_row(
    draft_selected: bool,
    text_style: &TextStyle,
    selected_color: gpui::Hsla,
    cx: &mut Context<Workspace>,
) -> Div {
    let (text_color, icon_color) = if draft_selected {
        (selected_color, selected_color)
    } else {
        (text_style.color.opacity(0.8), text_style.color.opacity(0.5))
    };
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .pl(px(4.))
        .pt(px(4.))
        .pb(px(2.))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, window, cx| {
                this.enter_draft(None, window, cx);
            }),
        )
        .child(
            Icon::new(IconName::Plus)
                .size(IconSize::XSmall)
                .color(Color::Custom(icon_color)),
        )
        .child(div().text_color(text_color).child("new agent"))
}

fn render_topic_rows(
    topic: &UiTopic,
    selected_agent: Option<&AgentId>,
    live: &BTreeSet<AgentId>,
    text_style: &TextStyle,
    selected_color: gpui::Hsla,
    cx: &mut Context<Workspace>,
) -> Div {
    let name = topic.name.clone();
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(
            div()
                .w_full()
                .pt(px(5.))
                .pl(px(4.))
                .text_color(text_style.color.opacity(0.65))
                .child(name),
        )
        .children(topic.agents.iter().map(|summary| {
            let agent_id = &summary.agent_id;
            let selected = selected_agent == Some(agent_id);
            let is_live = live.contains(agent_id);
            let text_color = if selected {
                selected_color
            } else {
                text_style.color
            };
            let icon_color = if selected {
                selected_color
            } else if is_live {
                text_style.color.opacity(0.9)
            } else {
                text_style.color.opacity(0.5)
            };
            div()
                .relative()
                .w_full()
                .flex()
                .items_center()
                .gap_1()
                .pl(px(12.))
                .overflow_hidden()
                .whitespace_nowrap()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener({
                        let agent_id = *agent_id;
                        move |this, _, window, cx| {
                            this.open_agent(agent_id, window, cx);
                        }
                    }),
                )
                .child(
                    Icon::new(if selected {
                        IconName::PlayFilled
                    } else {
                        IconName::Circle
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Custom(icon_color)),
                )
                .child(
                    div()
                        .flex_grow(1.0)
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(text_color)
                        .child(agent_id.to_string()),
                )
        }))
}
