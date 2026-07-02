//! The left rail listing agents grouped by topic.
//!
//! Topics are ad-hoc tab groups; every topic — including the daemon-created
//! "default" one that agents are born into — renders uniformly with its
//! name as the header, which advertises that grouping exists. Pinned topics
//! and agents sort first; archived ones are hidden until the "archived"
//! view mode flips the filter, showing exactly what the normal view hides.
//! Clicking an agent opens it (loading it on demand); the `+` row opens the
//! draft compose view and doubles as its selection indicator.

use std::collections::BTreeSet;

use gpui::prelude::*;
use gpui::{Context, Div, MouseButton, TextStyle, div, px};
use rho_ui_proto::{AgentId, Status, UiAgentSummary, UiTopic};
use theme::ActiveTheme as _;
use ui::{Color, Icon, IconName, IconSize};

use crate::registry::AgentRegistry;
use crate::workspace::Workspace;

pub fn render_topic_rail(
    registry: &AgentRegistry,
    show_archived: bool,
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
    let selected_agent = registry.selected_agent().cloned();
    let live = registry.live_agents().cloned().collect::<BTreeSet<_>>();

    let mut visible_topics = registry
        .topics()
        .iter()
        .filter_map(|topic| {
            let agents = visible_agents(topic, show_archived);
            (!agents.is_empty() || (topic.status == Status::Archived) == show_archived)
                .then_some((topic, agents))
        })
        .collect::<Vec<_>>();
    visible_topics.sort_by_key(|(topic, _)| topic.status != Status::Pinned);
    let rows = visible_topics
        .into_iter()
        .map(|(topic, agents)| {
            render_topic_rows(
                topic,
                agents,
                selected_agent.as_ref(),
                &live,
                registry,
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
        // Pinned below the scrolling list, so they stay reachable no matter
        // how many agents accumulate.
        .child(new_agent_row(
            selected_agent.is_none(),
            text_style,
            selected_color,
            cx,
        ))
        .child(show_archived_row(
            show_archived,
            text_style,
            selected_color,
            cx,
        ))
}

/// Flips the rail between active and archived views; archived items are
/// opened (and restored via `:agent archive` / `:topic archive`) from here.
fn show_archived_row(
    show_archived: bool,
    text_style: &TextStyle,
    selected_color: gpui::Hsla,
    cx: &mut Context<Workspace>,
) -> Div {
    let (text_color, icon_color) = if show_archived {
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
        .pt(px(2.))
        .pb(px(2.))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _window, cx| {
                this.toggle_show_archived(cx);
            }),
        )
        .child(
            Icon::new(IconName::Archive)
                .size(IconSize::XSmall)
                .color(Color::Custom(icon_color)),
        )
        .child(div().text_color(text_color).child(if show_archived {
            "back to active"
        } else {
            "view archived"
        }))
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

/// Which of a topic's agents the current view mode shows. The archived view
/// is the exact complement of the normal one: an agent is archived-visible
/// when it is archived itself or its whole topic is.
fn visible_agents(topic: &UiTopic, show_archived: bool) -> Vec<&UiAgentSummary> {
    let mut agents = topic
        .agents
        .iter()
        .filter(|summary| {
            let hidden = summary.status == Status::Archived || topic.status == Status::Archived;
            hidden == show_archived
        })
        .collect::<Vec<_>>();
    agents.sort_by_key(|summary| summary.status != Status::Pinned);
    agents
}

fn render_topic_rows(
    topic: &UiTopic,
    agents: Vec<&UiAgentSummary>,
    selected_agent: Option<&AgentId>,
    live: &BTreeSet<AgentId>,
    registry: &AgentRegistry,
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
                .flex()
                .items_center()
                .gap_1()
                .pt(px(5.))
                .pl(px(4.))
                .text_color(text_style.color.opacity(0.65))
                .child(name)
                .when(topic.status == Status::Pinned, |this| {
                    this.child(
                        Icon::new(IconName::Pin)
                            .size(IconSize::XSmall)
                            .color(Color::Custom(text_style.color.opacity(0.65))),
                    )
                }),
        )
        .children(agents.into_iter().map(|summary| {
            let agent_id = &summary.agent_id;
            let label = summary
                .display_name
                .clone()
                .unwrap_or_else(|| registry.agent_id_label(summary.agent_id));
            let selected = selected_agent == Some(agent_id);
            let is_live = live.contains(agent_id);
            let pinned = summary.status == Status::Pinned;
            let text_color = if selected {
                selected_color
            } else {
                text_style.color
            };
            let icon_color = if selected {
                selected_color
            } else if is_live || pinned {
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
                    Icon::new(if pinned {
                        IconName::Pin
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
                        .child(label),
                )
        }))
}
