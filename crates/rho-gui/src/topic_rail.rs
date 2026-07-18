//! The left rail listing tasks: one row per workstream.
//!
//! A workstream is the unit of work — every top-level agent founds its own —
//! so the rail lists workstreams, not agents. The row carries the title,
//! an attention rollup lamp, and small tags for member agents beyond the
//! primary one (subagents, joiners). Clicking the row opens the workstream's
//! primary agent (switching to its own pane arrangement); clicking a tag
//! opens that member directly. A workstream's group shows as a dim
//! annotation on the row — rows keep the plain pinned-first creation order.
//! The `+` row opens the draft compose view and doubles as its selection
//! indicator.

use gpui::prelude::*;
use gpui::{Context, Div, FontWeight, MouseButton, TextStyle, div, px};
use rho_ui_proto::{AgentId, Status, UiAgentSummary, UiAttention};
use theme::ActiveTheme as _;
use ui::{Color, Icon, IconName, IconSize};

use crate::registry::{AgentRegistry, Workstream};
use crate::workspace::Workspace;

pub fn render_topic_rail(
    registry: &AgentRegistry,
    text_style: &TextStyle,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let (selected_color, border_color, tag_background, lamps) = {
        let colors = cx.theme().colors();
        (
            colors.terminal_ansi_magenta.into(),
            colors.border_variant.opacity(0.6),
            colors.element_background.into(),
            LampColors {
                needs_input: colors.terminal_ansi_red.into(),
                pending: colors.terminal_ansi_yellow.into(),
                working: colors.terminal_ansi_cyan.into(),
            },
        )
    };
    let selected_agent = registry.selected_agent().cloned();

    let mut visible_topics = registry
        .workstreams()
        .iter()
        .filter(|topic| !topic.hidden)
        .collect::<Vec<_>>();
    visible_topics.sort_by_key(|topic| topic.status != Status::Pinned);
    let rows = visible_topics
        .into_iter()
        .map(|topic| {
            task_row(
                topic,
                selected_agent.as_ref(),
                registry,
                text_style,
                selected_color,
                tag_background,
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

/// How many member tags a task row shows before collapsing into `+n`.
const VISIBLE_TAGS: usize = 4;

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;
    use rho_ui_proto::{AgentIdDomain, AgentRole, TagId, TagKind, UiTag, WorkspaceInfo};

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
            role: AgentRole::default(),
            workspace: WorkspaceInfo::UserCheckout {
                repo: "/tmp".into(),
            },
            status,
            attention: UiAttention::Quiet,
            last_active: UnixMs(crate::workspace::now_ms() + id),
            hidden: false,
            tags: vec![TagId(1)],
        }
    }

    fn topic(status: Status, agents: Vec<UiAgentSummary>) -> Workstream {
        Workstream {
            tag_id: TagId(1),
            name: "topic".to_owned(),
            status,
            hidden: false,
            group: None,
            agents,
        }
    }

    fn install(registry: &mut AgentRegistry, topic: &Workstream) {
        registry.set_data(
            vec![UiTag {
                tag_id: topic.tag_id,
                name: topic.name.clone(),
                kind: TagKind::Workstream,
                parent: topic.group,
                status: topic.status,
            }],
            topic.agents.clone(),
        );
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
        install(&mut registry, &topic);

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
        install(&mut registry, &topic);
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
        install(&mut registry, &topic);
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
        install(&mut registry, &topic);
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
        install(&mut registry, &topic);
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

/// Splits a workstream's agents into the listed ones (rail sort: pins,
/// active bucket, retained order) and the folded tail (filed away or
/// inactive; most recently touched first).
fn split_agents<'a>(
    topic: &'a Workstream,
    registry: &AgentRegistry,
) -> (Vec<&'a UiAgentSummary>, Vec<&'a UiAgentSummary>) {
    registry.split_workstream_agents(topic)
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

/// One task: title, attention rollup, and member tags. The primary agent
/// (rail sort: pins, attention, engagement) answers a row click; the
/// remaining members render as small clickable tags.
#[allow(clippy::too_many_arguments)]
fn task_row(
    topic: &Workstream,
    selected_agent: Option<&AgentId>,
    registry: &AgentRegistry,
    text_style: &TextStyle,
    selected_color: gpui::Hsla,
    tag_background: gpui::Hsla,
    lamps: LampColors,
    cx: &mut Context<Workspace>,
) -> Div {
    let (agents, _folded) = split_agents(topic, registry);
    let primary = agents.first().map(|summary| summary.agent_id);
    let selected = selected_agent
        .is_some_and(|selected| topic.agent_ids().any(|agent_id| agent_id == *selected));
    let pinned = topic.status == Status::Pinned;
    // The row lamp is the most urgent member: acting on the task means
    // acting on whoever inside it wants the user.
    let attention = agents
        .iter()
        .map(|summary| registry.attention(summary.agent_id))
        .max()
        .unwrap_or(UiAttention::Quiet);
    let lamp = lamps.color(attention);
    let title = if topic.name.trim().is_empty() {
        primary
            .map(|agent_id| registry.agent_display_label(agent_id))
            .unwrap_or_else(|| "task".to_owned())
    } else {
        topic.name.clone()
    };
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
    // The group reads as a dim annotation rather than a section, so rows
    // keep their plain pinned-first creation order.
    let group_name = topic
        .group
        .and_then(|group| registry.tag_name(group))
        .map(str::to_owned);
    let members = agents.iter().skip(1).copied().collect::<Vec<_>>();
    let overflow = members.len().saturating_sub(VISIBLE_TAGS);
    let tags = members
        .into_iter()
        .take(VISIBLE_TAGS)
        .map(|summary| {
            member_tag(
                summary,
                selected_agent,
                registry,
                text_style,
                selected_color,
                tag_background,
                lamps,
                cx,
            )
            .into_any_element()
        })
        .chain((overflow > 0).then(|| {
            div()
                .text_color(text_style.color.opacity(0.5))
                .child(format!("+{overflow}"))
                .into_any_element()
        }))
        .collect::<Vec<_>>();
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .pl(px(4.))
        .pt(px(2.))
        .overflow_hidden()
        .whitespace_nowrap()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                if let Some(agent_id) = primary {
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
                .child(title),
        )
        .children(group_name.map(|name| {
            div()
                .flex_none()
                .text_color(text_style.color.opacity(0.45))
                .child(name)
        }))
        .children(tags)
}

/// A member agent beyond the primary (subagent, joiner): a small chip on
/// the task row, lamp-colored when it wants the user. Clicking opens that
/// member instead of the primary.
#[allow(clippy::too_many_arguments)]
fn member_tag(
    summary: &UiAgentSummary,
    selected_agent: Option<&AgentId>,
    registry: &AgentRegistry,
    text_style: &TextStyle,
    selected_color: gpui::Hsla,
    tag_background: gpui::Hsla,
    lamps: LampColors,
    cx: &mut Context<Workspace>,
) -> Div {
    let agent_id = summary.agent_id;
    let attention = registry.attention(agent_id);
    let selected = selected_agent == Some(&agent_id);
    let color = if selected {
        selected_color
    } else if let Some(lamp) = lamps.color(attention) {
        lamp
    } else {
        text_style.color.opacity(0.65)
    };
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .bg(tag_background)
        .text_color(color)
        .child(registry.agent_id_label(agent_id))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.open_agent(agent_id, window, cx);
            }),
        )
}
