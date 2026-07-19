//! The dashboard: the rail reborn as a real editor buffer — rho's
//! magit-status. One line per workstream in triage order, generated
//! read-only text in a normal editor, so the cursor, motions, and search
//! all come from the editor rather than bespoke list chrome. Acting keys
//! (`enter` to open, later reply and verdicts) address the row under the
//! cursor; across refreshes the cursor sticks to its row's identity, not
//! its line number.

use std::ops::Range;

use editor::{Editor, EditorMode, HighlightKey, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, FontWeight, HighlightStyle, Window};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_ui_proto::{AgentId, UiAttention, WorkstreamId};
use theme::ActiveTheme as _;

use crate::registry::{AgentRegistry, Workstream};
use crate::workspace::Workspace;

/// How many member tags a workstream row shows before collapsing into `+n`.
const VISIBLE_TAGS: usize = 4;

/// Highlight-key space for dashboard classes, clear of the transcript's
/// semantic and syntax key ranges.
const DASHBOARD_KEY_BASE: usize = usize::MAX - 200;

/// What the line under the cursor refers to; the object of every
/// dashboard command.
#[derive(Clone, Debug, PartialEq)]
pub enum RowTarget {
    /// Group headers and other inert lines.
    None,
    Stream {
        workstream_id: WorkstreamId,
        primary: Option<AgentId>,
    },
    FoldToggle,
    NewAgent,
}

impl RowTarget {
    /// Row identity for cursor restoration: the cursor follows the same
    /// workstream across refreshes even as its line number or primary
    /// agent changes.
    fn same_row(&self, other: &RowTarget) -> bool {
        match (self, other) {
            (
                RowTarget::Stream { workstream_id, .. },
                RowTarget::Stream {
                    workstream_id: other_id,
                    ..
                },
            ) => workstream_id == other_id,
            (RowTarget::None, _) | (_, RowTarget::None) => false,
            _ => self == other,
        }
    }
}

pub struct Dashboard {
    buffer: Entity<Buffer>,
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    /// One entry per buffer line, in order.
    rows: Vec<RowTarget>,
    /// The last generated listing, for cheap change detection.
    text: String,
}

impl Dashboard {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local("", cx);
            buffer.set_capability(Capability::Read, cx);
            buffer
        });
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::Read);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                buffer.clone(),
                [Point::zero()..buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        });
        let buffer_id = buffer.read(cx).remote_id();
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
                None,
                window,
                cx,
            );
            crate::editor_config::configure(&mut editor, window, cx);
            // Listing lines clip like the rail did; wrapping would break the
            // line-per-row shape.
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::None, cx);
            // Unlike the chat editors, clicking a row to put the cursor on
            // it is the whole point.
            editor.set_mouse_click_selection_enabled(true, cx);
            editor.set_read_only(true);
            editor.disable_header_for_buffer(buffer_id, cx);
            editor
        });
        Self {
            buffer,
            multi_buffer,
            editor,
            rows: Vec::new(),
            text: String::new(),
        }
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    /// Regenerates the listing from the registry: replaces the buffer text
    /// when it changed (keeping the cursor on its row by identity) and
    /// reapplies highlights (attention lamps shift without text edits).
    pub fn sync(&mut self, registry: &AgentRegistry, window: &mut Window, cx: &mut Context<Workspace>) {
        let layout = generate(registry);
        if layout.text != self.text {
            let cursor_row = self.cursor_target(cx);
            self.buffer.update(cx, |buffer, cx| {
                let len = buffer.len();
                buffer.edit([(0..len, layout.text.as_str())], None, cx);
            });
            let buffer = self.buffer.clone();
            self.multi_buffer.update(cx, |multi_buffer, cx| {
                multi_buffer.set_excerpts_for_path(
                    PathKey::sorted(0),
                    buffer.clone(),
                    [Point::zero()..buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            });
            self.text = layout.text;
            let restore = cursor_row.and_then(|target| {
                layout
                    .rows
                    .iter()
                    .position(|row| row.same_row(&target))
                    .map(|line| Point::new(line as u32, 0))
            });
            self.rows = layout.rows;
            if let Some(point) = restore {
                self.editor.update(cx, |editor, cx| {
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_ranges([point..point]);
                    });
                });
            }
        } else {
            self.rows = layout.rows;
        }
        self.apply_highlights(&layout.spans, cx);
    }

    /// The row under the cursor.
    pub fn cursor_target(&self, cx: &mut Context<Workspace>) -> Option<RowTarget> {
        let row = self.editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head()
                .row
        });
        self.rows.get(row as usize).cloned()
    }

    fn apply_highlights(&self, spans: &[(DashClass, Range<usize>)], cx: &mut Context<Workspace>) {
        let buffer_snapshot = self.buffer.read(cx).snapshot();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let anchors = |class: DashClass| {
            spans
                .iter()
                .filter(|(span_class, _)| *span_class == class)
                .filter_map(|(_, range)| {
                    let start = snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(
                        range.start.min(buffer_snapshot.len()),
                    ))?;
                    let end = snapshot.anchor_in_excerpt(
                        buffer_snapshot.anchor_before(range.end.min(buffer_snapshot.len())),
                    )?;
                    Some(start..end)
                })
                .collect::<Vec<_>>()
        };
        let updates = DashClass::ALL
            .into_iter()
            .map(|class| (class, anchors(class)))
            .collect::<Vec<_>>();
        self.editor.update(cx, |editor, cx| {
            for (class, ranges) in updates {
                editor.highlight_text(class.key(), ranges, class.style(cx), cx);
            }
        });
    }
}

/// Dashboard text classes: lamps, selection, and muted chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashClass {
    Muted,
    Selected,
    Working,
    Pending,
    NeedsInput,
    /// Attention at pending or above: the title asks for the eye.
    Urgent,
}

impl DashClass {
    const ALL: [DashClass; 6] = [
        DashClass::Muted,
        DashClass::Selected,
        DashClass::Working,
        DashClass::Pending,
        DashClass::NeedsInput,
        DashClass::Urgent,
    ];

    fn key(self) -> HighlightKey {
        let slot = match self {
            DashClass::Muted => 0,
            DashClass::Selected => 1,
            DashClass::Working => 2,
            DashClass::Pending => 3,
            DashClass::NeedsInput => 4,
            DashClass::Urgent => 5,
        };
        HighlightKey::SyntaxTreeView(DASHBOARD_KEY_BASE + slot)
    }

    fn style(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let color = match self {
            DashClass::Muted => colors.text_muted,
            DashClass::Selected => colors.terminal_ansi_magenta,
            DashClass::Working => colors.terminal_ansi_cyan,
            DashClass::Pending => colors.terminal_ansi_yellow,
            DashClass::NeedsInput => colors.terminal_ansi_red,
            DashClass::Urgent => {
                return HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    ..HighlightStyle::default()
                };
            }
        };
        HighlightStyle {
            color: Some(color.into()),
            ..HighlightStyle::default()
        }
    }

    fn lamp(attention: UiAttention) -> Option<DashClass> {
        match attention {
            UiAttention::Quiet => None,
            UiAttention::Working => Some(DashClass::Working),
            UiAttention::Pending => Some(DashClass::Pending),
            UiAttention::NeedsInput => Some(DashClass::NeedsInput),
        }
    }
}

/// One row of the assembled dashboard, in display order.
#[derive(Debug, PartialEq)]
pub enum RailRow<'a> {
    /// A workstream-group section starts; its member tasks follow.
    GroupHeader(&'a str),
    Task {
        topic: &'a Workstream,
        grouped: bool,
    },
    /// The quiet tail's "n more" / "fold" toggle.
    FoldToggle { folded_count: usize, expanded: bool },
}

/// Assembles the dashboard from the split rows: the whole structure as
/// plain data, decided here and only serialized by the caller.
///
/// Expansion merges the folded tail back before grouping, so a group split
/// across the fold reunites instead of repeating its header. A group
/// section anchors at its best-sorted member's position and gathers the
/// rest of the group up to it; ungrouped rows stay put. A non-empty tail
/// trails as the fold toggle.
fn rail_rows<'a>(
    listed: Vec<&'a Workstream>,
    folded: Vec<&'a Workstream>,
    expanded: bool,
) -> Vec<RailRow<'a>> {
    let folded_count = folded.len();
    let display = if expanded {
        listed.into_iter().chain(folded).collect()
    } else {
        listed
    };
    let mut rows = Vec::new();
    let mut seen_groups = std::collections::BTreeSet::new();
    for (index, topic) in display.iter().enumerate() {
        match &topic.group {
            None => rows.push(RailRow::Task {
                topic,
                grouped: false,
            }),
            Some(group) => {
                if !seen_groups.insert(group.clone()) {
                    continue;
                }
                rows.push(RailRow::GroupHeader(group));
                rows.extend(
                    display[index..]
                        .iter()
                        .filter(|member| member.group.as_ref() == Some(group))
                        .map(|member| RailRow::Task {
                            topic: member,
                            grouped: true,
                        }),
                );
            }
        }
    }
    if folded_count > 0 {
        rows.push(RailRow::FoldToggle {
            folded_count,
            expanded,
        });
    }
    rows
}

struct Layout {
    text: String,
    rows: Vec<RowTarget>,
    spans: Vec<(DashClass, Range<usize>)>,
}

/// Serializes the registry into the dashboard listing: text, one row
/// target per line, and highlight spans as byte ranges.
fn generate(registry: &AgentRegistry) -> Layout {
    let mut layout = Layout {
        text: String::new(),
        rows: Vec::new(),
        spans: Vec::new(),
    };
    let (listed, folded) = registry.split_rows();
    for row in rail_rows(listed, folded, registry.rail_tail_expanded()) {
        match row {
            RailRow::GroupHeader(name) => {
                layout.rows.push(RowTarget::None);
                layout.line(DashClass::Muted, |text| text.push_str(name));
            }
            RailRow::Task { topic, grouped } => task_line(topic, grouped, registry, &mut layout),
            RailRow::FoldToggle {
                folded_count,
                expanded,
            } => {
                layout.rows.push(RowTarget::FoldToggle);
                layout.line(DashClass::Muted, |text| {
                    if expanded {
                        text.push_str("fold");
                    } else {
                        text.push_str(&format!("{folded_count} more"));
                    }
                });
            }
        }
    }
    let draft_selected = registry.selected_agent().is_none();
    layout.rows.push(RowTarget::NewAgent);
    layout.line(
        if draft_selected {
            DashClass::Selected
        } else {
            DashClass::Muted
        },
        |text| text.push_str("+ new agent"),
    );
    // Drop the final newline so lines and row targets stay one-to-one.
    if layout.text.ends_with('\n') {
        layout.text.pop();
    }
    layout
}

impl Layout {
    /// Appends one whole line in a single class.
    fn line(&mut self, class: DashClass, write: impl FnOnce(&mut String)) {
        let start = self.text.len();
        write(&mut self.text);
        self.spans.push((class, start..self.text.len()));
        self.text.push('\n');
    }

    fn span(&mut self, class: Option<DashClass>, write: impl FnOnce(&mut String)) {
        let start = self.text.len();
        write(&mut self.text);
        if let Some(class) = class {
            self.spans.push((class, start..self.text.len()));
        }
    }
}

/// One workstream's line: lamp glyph, title, then member tags beyond the
/// primary. The primary agent answers `enter`; the lamp is the most urgent
/// member — acting on the row means acting on whoever inside wants the user.
fn task_line(topic: &Workstream, grouped: bool, registry: &AgentRegistry, layout: &mut Layout) {
    let (agents, _folded) = registry.split_workstream_agents(topic);
    let primary = agents.first().map(|summary| summary.agent_id);
    let selected_agent = registry.selected_agent().copied();
    let selected = selected_agent
        .is_some_and(|selected| topic.agent_ids().any(|agent_id| agent_id == selected));
    let attention = agents
        .iter()
        .map(|summary| registry.attention(summary.agent_id))
        .max()
        .unwrap_or(UiAttention::Quiet);
    let title = if topic.name.trim().is_empty() {
        primary
            .map(|agent_id| registry.agent_display_label(agent_id))
            .unwrap_or_else(|| "task".to_owned())
    } else {
        topic.name.clone()
    };

    layout.rows.push(RowTarget::Stream {
        workstream_id: topic.workstream_id,
        primary,
    });
    if grouped {
        layout.span(None, |text| text.push_str("  "));
    }
    let glyph_class = if selected {
        Some(DashClass::Selected)
    } else if let Some(lamp) = DashClass::lamp(attention) {
        Some(lamp)
    } else if topic.pinned {
        None
    } else {
        Some(DashClass::Muted)
    };
    layout.span(glyph_class, |text| {
        text.push(if topic.pinned { '◆' } else { '●' })
    });
    layout.span(None, |text| text.push(' '));
    let title_class = if selected {
        Some(DashClass::Selected)
    } else if attention >= UiAttention::Pending {
        Some(DashClass::Urgent)
    } else {
        None
    };
    layout.span(title_class, |text| text.push_str(&title));

    let members = agents.iter().skip(1).collect::<Vec<_>>();
    let overflow = members.len().saturating_sub(VISIBLE_TAGS);
    for member in members.into_iter().take(VISIBLE_TAGS) {
        let class = if selected_agent == Some(member.agent_id) {
            DashClass::Selected
        } else {
            DashClass::lamp(registry.attention(member.agent_id)).unwrap_or(DashClass::Muted)
        };
        layout.span(None, |text| text.push_str("  "));
        layout.span(Some(class), |text| {
            text.push_str(&registry.agent_id_label(member.agent_id))
        });
    }
    if overflow > 0 {
        layout.span(None, |text| text.push_str("  "));
        layout.span(Some(DashClass::Muted), |text| {
            text.push_str(&format!("+{overflow}"))
        });
    }
    layout.text.push('\n');
}

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;
    use rho_ui_proto::{
        AgentIdDomain, AgentRole, UiAgentSummary, UiWorkstream, WorkspaceInfo, WorkstreamId,
    };

    use super::*;

    /// Pin state fixture shorthand, in the shape the old tag `Status` had.
    #[derive(Clone, Copy, PartialEq)]
    enum Status {
        Normal,
        Pinned,
    }

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
            attention: UiAttention::Quiet,
            last_active: UnixMs(crate::workspace::now_ms() + id),
            hidden: false,
            last_user_message_text: String::new(),
            workstream: WorkstreamId(1),
            labels: match status {
                Status::Normal => Vec::new(),
                Status::Pinned => vec![crate::registry::PIN_LABEL.to_owned()],
            },
        }
    }

    fn topic(status: Status, agents: Vec<UiAgentSummary>) -> Workstream {
        Workstream {
            workstream_id: WorkstreamId(1),
            name: "topic".to_owned(),
            pinned: status == Status::Pinned,
            hidden: false,
            group: None,
            agents,
        }
    }

    fn install(registry: &mut AgentRegistry, topic: &Workstream) {
        let mut labels = Vec::new();
        if topic.pinned {
            labels.push(crate::registry::PIN_LABEL.to_owned());
        }
        registry.set_data(
            vec![UiWorkstream {
                workstream_id: topic.workstream_id,
                name: topic.name.clone(),
                labels,
            }],
            topic.agents.clone(),
        );
    }

    /// Bare workstream fixture for row-assembly tests: identity and group
    /// only, no members.
    fn stream(id: u64, group: Option<&str>) -> Workstream {
        Workstream {
            workstream_id: WorkstreamId(id),
            name: format!("ws-{id}"),
            pinned: false,
            hidden: false,
            group: group.map(str::to_owned),
            agents: Vec::new(),
        }
    }

    fn ids(rows: &[RailRow<'_>]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                RailRow::GroupHeader(group) => format!("[{group}]"),
                RailRow::Task { topic, grouped } => {
                    format!("{}{}", if *grouped { "  " } else { "" }, topic.name)
                }
                RailRow::FoldToggle {
                    folded_count,
                    expanded,
                } => format!("fold({folded_count},{expanded})"),
            })
            .collect()
    }

    fn split_agents<'a>(
        topic: &'a Workstream,
        registry: &AgentRegistry,
    ) -> (Vec<&'a UiAgentSummary>, Vec<&'a UiAgentSummary>) {
        registry.split_workstream_agents(topic)
    }

    #[test]
    fn groups_anchor_at_first_member_and_gather_the_rest() {
        let rows = [
            stream(1, None),
            stream(2, Some("infra")),
            stream(3, None),
            stream(4, Some("infra")),
        ];
        let assembled = rail_rows(rows.iter().collect(), Vec::new(), false);
        assert_eq!(
            ids(&assembled),
            ["ws-1", "[infra]", "  ws-2", "  ws-4", "ws-3"]
        );
    }

    #[test]
    fn expansion_reunites_a_group_split_across_the_fold() {
        let listed = [stream(1, Some("infra")), stream(2, None)];
        let folded = [stream(3, Some("infra"))];

        let collapsed = rail_rows(listed.iter().collect(), folded.iter().collect(), false);
        assert_eq!(
            ids(&collapsed),
            ["[infra]", "  ws-1", "ws-2", "fold(1,false)"]
        );

        let expanded = rail_rows(listed.iter().collect(), folded.iter().collect(), true);
        assert_eq!(
            ids(&expanded),
            ["[infra]", "  ws-1", "  ws-3", "ws-2", "fold(1,true)"]
        );
    }

    #[test]
    fn empty_tail_gets_no_fold_toggle() {
        let listed = [stream(1, None)];
        let assembled = rail_rows(listed.iter().collect(), Vec::new(), false);
        assert_eq!(ids(&assembled), ["ws-1"]);
    }

    #[test]
    fn listing_lines_match_row_targets_one_to_one() {
        let members = vec![agent(1, Status::Normal, 10), agent(2, Status::Normal, 10)];
        let topic = topic(Status::Normal, members);
        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);

        let layout = generate(&registry);
        assert_eq!(layout.text.lines().count(), layout.rows.len());
        assert!(layout.text.lines().next().unwrap().contains("topic"));
        // The primary is the best-listed member: engagement recency puts
        // the later fixture first.
        assert_eq!(
            layout.rows.first(),
            Some(&RowTarget::Stream {
                workstream_id: WorkstreamId(1),
                primary: Some(AgentId::from_counter(2, &AgentIdDomain(0)).unwrap()),
            })
        );
        assert_eq!(layout.rows.last(), Some(&RowTarget::NewAgent));
    }

    #[test]
    fn cursor_identity_follows_the_workstream_not_the_line() {
        let stream_row = RowTarget::Stream {
            workstream_id: WorkstreamId(7),
            primary: None,
        };
        let same_stream_new_primary = RowTarget::Stream {
            workstream_id: WorkstreamId(7),
            primary: Some(AgentId::from_counter(1, &AgentIdDomain(0)).unwrap()),
        };
        assert!(stream_row.same_row(&same_stream_new_primary));
        assert!(!stream_row.same_row(&RowTarget::Stream {
            workstream_id: WorkstreamId(8),
            primary: None,
        }));
        assert!(RowTarget::NewAgent.same_row(&RowTarget::NewAgent));
        assert!(!RowTarget::None.same_row(&RowTarget::None));
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
