//! The left rail listing agents grouped by topic.
//!
//! Topics are ad-hoc tab groups; every topic — including the daemon-created
//! "default" one that agents are born into — renders uniformly with its
//! name as the header, which advertises that grouping exists. Pinned topics
//! and agents sort first; folded agents (filed via `:done hide` or outside
//! the active bucket) collapse into a per-topic tail row that expands in
//! place. Clicking an agent opens it (loading folded agents on demand); the
//! `+` row opens the draft compose view and doubles as its selection
//! indicator.

use std::cmp::Reverse;
use std::collections::HashSet;

use gpui::prelude::*;
use gpui::{Context, Div, FontWeight, MouseButton, TextStyle, div, px};
use rho_ui_proto::{AgentId, Status, TopicId, UiAgentSummary, UiAttention, UiTopic};
use theme::ActiveTheme as _;
use ui::{Color, Icon, IconName, IconSize};

use crate::registry::AgentRegistry;
use crate::workspace::Workspace;

pub fn render_topic_rail(
    registry: &AgentRegistry,
    expanded_folds: &HashSet<TopicId>,
    text_style: &TextStyle,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let (selected_color, border_color, lamps) = {
        let colors = cx.theme().colors();
        (
            colors.terminal_ansi_magenta.into(),
            colors.border_variant.opacity(0.6),
            LampColors {
                needs_input: colors.terminal_ansi_red.into(),
                pending: colors.terminal_ansi_yellow.into(),
                working: colors.terminal_ansi_cyan.into(),
            },
        )
    };
    let selected_agent = registry.selected_agent().cloned();

    let mut visible_topics = registry.topics().iter().collect::<Vec<_>>();
    visible_topics.sort_by_key(|topic| topic.status != Status::Pinned);
    let rows = visible_topics
        .into_iter()
        .map(|topic| {
            let (agents, folded) = split_agents(topic, registry);
            render_topic_rows(
                topic,
                agents,
                folded,
                expanded_folds.contains(&topic.topic_id),
                selected_agent.as_ref(),
                registry,
                text_style,
                selected_color,
                lamps,
                cx,
            )
        })
        .collect::<Vec<_>>();

    div()
        .id("rho-gui-topic-rail")
        .h_full()
        .w(px(275.))
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
                .id("rho-gui-topic-list")
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
}

/// The collapsed tail of a topic: click to expand the folded agents in
/// place (and again to fold them back).
fn fold_row(
    topic_id: TopicId,
    folded_count: usize,
    expanded: bool,
    text_style: &TextStyle,
    cx: &mut Context<Workspace>,
) -> Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .pl(px(12.))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _window, cx| {
                this.toggle_topic_fold(topic_id, cx);
            }),
        )
        .child(
            Icon::new(IconName::Archive)
                .size(IconSize::XSmall)
                .color(Color::Custom(text_style.color.opacity(0.5).into())),
        )
        .child(
            div()
                .text_color(text_style.color.opacity(0.65))
                .child(if expanded {
                    "fold".to_owned()
                } else {
                    format!("{folded_count} more")
                }),
        )
}

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;
    use rho_ui_proto::{AgentConfig, AgentIdDomain, TopicId, TopicIdDomain, WorkspaceInfo};

    use super::*;

    /// Freshly-engaged fixture (`last_active` at now + `id`) for deterministic
    /// active-bucket ordering.
    fn agent(id: u64, status: Status, updated_at: u64) -> UiAgentSummary {
        UiAgentSummary {
            agent_id: AgentId::from_counter(id, &AgentIdDomain(0)).unwrap(),
            parent_agent: None,
            display_name: None,
            created_at: UnixMs(id),
            updated_at: UnixMs(updated_at),
            mode: AgentConfig::default(),
            workspace: WorkspaceInfo::UserCheckout {
                repo: "/tmp".into(),
            },
            status,
            attention: UiAttention::Quiet,
            last_active: UnixMs(crate::workspace::now_ms() + id),
            hidden: false,
        }
    }

    fn topic(status: Status, agents: Vec<UiAgentSummary>) -> UiTopic {
        UiTopic {
            topic_id: TopicId::from_counter(1, &TopicIdDomain(0)).unwrap(),
            name: "topic".to_owned(),
            status,
            agents,
        }
    }

    #[test]
    fn hidden_and_inactive_bucket_agents_move_to_the_folded_tail() {
        let inactive = agent(1, Status::Normal, 10);
        let fresh = agent(2, Status::Normal, 10);
        let mut filed = agent(3, Status::Normal, 10);
        filed.hidden = true;
        let mut summaries = vec![inactive, fresh, filed];
        summaries.extend((4..=13).map(|id| agent(id, Status::Normal, 10)));
        let topic = topic(Status::Normal, summaries);
        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![topic.clone()]);

        let (active, folded) = split_agents(&topic, &registry);
        let active = active
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();
        let folded = folded
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();

        assert_eq!(
            active,
            [13, 12, 11, 10, 9, 8, 7, 6, 5, 4].map(|id| AgentId::from_counter(
                id,
                &AgentIdDomain(0)
            )
            .unwrap())
        );
        assert_eq!(
            folded,
            [
                AgentId::from_counter(1, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(2, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(3, &AgentIdDomain(0)).unwrap(),
            ]
        );
    }

    #[test]
    fn folded_agents_sort_by_updated_at_newest_first() {
        let mut summaries = vec![
            agent(1, Status::Normal, 10),
            agent(2, Status::Normal, 30),
            agent(3, Status::Normal, 20),
        ];
        for summary in &mut summaries {
            summary.hidden = true;
        }
        let topic = topic(Status::Normal, summaries);

        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![topic.clone()]);
        let (_, folded) = split_agents(&topic, &registry);
        let folded = folded
            .into_iter()
            .map(|summary| summary.updated_at)
            .collect::<Vec<_>>();

        assert_eq!(folded, [UnixMs(30), UnixMs(20), UnixMs(10)]);
    }

    #[test]
    fn pinned_agents_stay_above_attention_bucket() {
        let quiet_pinned = agent(1, Status::Pinned, 10);
        let urgent = agent(2, Status::Normal, 10);
        let topic = topic(Status::Normal, vec![quiet_pinned, urgent.clone()]);

        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![topic.clone()]);
        registry.set_attention(urgent.agent_id, UiAttention::NeedsInput);

        let visible = split_agents(&topic, &registry)
            .0
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();

        assert_eq!(
            visible,
            [
                AgentId::from_counter(1, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(2, &AgentIdDomain(0)).unwrap(),
            ]
        );
    }

    #[test]
    fn active_agents_sort_by_engagement_after_pins() {
        let idle = agent(1, Status::Normal, 10);
        let pinned = agent(2, Status::Pinned, 10);
        let mut recent = agent(3, Status::Normal, 10);
        recent.last_active = UnixMs(crate::workspace::now_ms() + 100);
        let topic = topic(Status::Normal, vec![idle, pinned, recent]);

        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![topic.clone()]);
        let visible = split_agents(&topic, &registry)
            .0
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();

        // Pins first, then by seeded engagement recency (last user message).
        assert_eq!(
            visible,
            [
                AgentId::from_counter(2, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(3, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(1, &AgentIdDomain(0)).unwrap(),
            ]
        );
    }

    #[test]
    fn same_topic_children_follow_their_parent() {
        let parent = agent(1, Status::Pinned, 10);
        let mut child = agent(2, Status::Normal, 10);
        child.parent_agent = Some(parent.agent_id);
        let mut grandchild = agent(3, Status::Normal, 10);
        grandchild.parent_agent = Some(child.agent_id);
        let root = agent(4, Status::Normal, 10);
        let topic = topic(Status::Normal, vec![parent, root, grandchild, child]);

        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![topic.clone()]);
        let collapsed = split_agents(&topic, &registry)
            .0
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();
        assert_eq!(
            collapsed,
            [1, 4].map(|id| AgentId::from_counter(id, &AgentIdDomain(0)).unwrap())
        );

        registry.select_agent(AgentId::from_counter(1, &AgentIdDomain(0)).unwrap());
        let visible = split_agents(&topic, &registry)
            .0
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();

        assert_eq!(
            visible,
            [1, 2, 3, 4].map(|id| AgentId::from_counter(id, &AgentIdDomain(0)).unwrap())
        );
        assert_eq!(registry.topic_agent_depth(&topic, visible[2]), 2);
    }
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
                .color(Color::Custom(icon_color.into())),
        )
        .child(div().text_color(text_color).child("new agent"))
}

/// Splits a topic's agents into the listed ones (rail sort: pins, active
/// bucket, retained order) and the folded tail (filed away or inactive; most
/// recently touched first).
fn split_agents<'a>(
    topic: &'a UiTopic,
    registry: &AgentRegistry,
) -> (Vec<&'a UiAgentSummary>, Vec<&'a UiAgentSummary>) {
    let (mut agents, mut folded): (Vec<_>, Vec<_>) = topic
        .agents
        .iter()
        .filter(|summary| !registry.agent_auto_collapsed(summary.agent_id))
        .partition(|summary| !registry.agent_folded(summary.agent_id));
    agents = registry.order_topic_agents(topic, agents);
    folded.sort_by_key(|summary| Reverse(summary.updated_at));
    (agents, folded)
}

/// Lamp palette for attention levels; Quiet has no lamp.
#[derive(Clone, Copy)]
struct LampColors {
    needs_input: gpui::Hsla,
    pending: gpui::Hsla,
    working: gpui::Hsla,
}

impl LampColors {
    fn color(&self, attention: UiAttention) -> Option<gpui::Hsla> {
        match attention {
            UiAttention::Quiet => None,
            UiAttention::Working => Some(self.working),
            UiAttention::Pending => Some(self.pending),
            UiAttention::NeedsInput => Some(self.needs_input),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_topic_rows<'a>(
    topic: &UiTopic,
    mut agents: Vec<&'a UiAgentSummary>,
    folded: Vec<&'a UiAgentSummary>,
    expanded: bool,
    selected_agent: Option<&AgentId>,
    registry: &AgentRegistry,
    text_style: &TextStyle,
    selected_color: gpui::Hsla,
    lamps: LampColors,
    cx: &mut Context<Workspace>,
) -> Div {
    let name = topic.name.clone();
    let folded_count = folded.len();
    if expanded {
        agents.extend(folded);
    }
    agents = registry.order_topic_agents(topic, agents);
    let fold = (folded_count > 0)
        .then(|| fold_row(topic.topic_id, folded_count, expanded, text_style, cx));
    // Roll the topic's most urgent agent up into the header, so a collapsed
    // or scrolled-away topic still shows that something inside wants the
    // user. Working alone doesn't qualify: the header lamp means "act here".
    let rollup = agents
        .iter()
        .map(|summary| registry.attention(summary.agent_id))
        .max()
        .filter(|attention| *attention >= UiAttention::Pending)
        .and_then(|attention| lamps.color(attention));
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
                            .color(Color::Custom(text_style.color.opacity(0.65).into())),
                    )
                })
                .when_some(rollup, |this, lamp| {
                    this.child(
                        Icon::new(IconName::Circle)
                            .size(IconSize::XSmall)
                            .color(Color::Custom(lamp.into())),
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
            let depth = registry.topic_agent_depth(topic, summary.agent_id);
            let pinned = summary.status == Status::Pinned;
            let attention = registry.attention(summary.agent_id);
            let lamp = lamps.color(attention);
            let text_color = if selected {
                selected_color
            } else {
                text_style.color
            };
            let icon_color = if selected {
                selected_color
            } else if let Some(lamp) = lamp {
                lamp
            } else if pinned {
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
                .pl(px(12. + depth as f32 * 14.))
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
                    .color(Color::Custom(icon_color.into())),
                )
                .child(
                    div()
                        .flex_grow(1.0)
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(text_color)
                        .when(attention >= UiAttention::Pending, |this| {
                            this.font_weight(FontWeight::BOLD)
                        })
                        .child(label),
                )
        }))
        .children(fold)
}
