use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use editor::{Editor, EditorMode, HighlightKey};
use gpui::prelude::*;
use gpui::{Context, Entity, FontWeight, HighlightStyle, StrikethroughStyle, Window, div, px};
use language::{Buffer, BufferEvent, Capability};
use multi_buffer::MultiBufferOffset;
use rho_ui_proto::ClientMessage;
use rho_ui_proto::desk::{
    DeskClock, DeskHeading, DeskHeadingState, DeskOperation, DeskSnapshot, DeskTextOpRecord,
    DeskTransaction, parse,
};
use text::{BufferId, ReplicaId};
use theme::ActiveTheme as _;

use crate::DeskCycle;
use crate::registry::HostId;
use crate::workspace::Workspace;

const DESK_KEY_BASE: usize = usize::MAX - 300;

struct HostDesk {
    snapshot: DeskSnapshot,
    buffer: Entity<Buffer>,
    _subscription: gpui::Subscription,
}

/// Workspace-owned source of truth for every attached host's Desk document.
pub struct DeskSync {
    hosts: BTreeMap<HostId, HostDesk>,
    known_ops: HashSet<(HostId, DeskClock)>,
    next_buffer_id: u64,
}

impl Default for DeskSync {
    fn default() -> Self {
        Self {
            hosts: BTreeMap::new(),
            known_ops: HashSet::new(),
            next_buffer_id: 1,
        }
    }
}

impl DeskSync {
    pub fn apply_snapshot(
        &mut self,
        host: HostId,
        snapshot: DeskSnapshot,
        replica_id: u16,
        cx: &mut Context<Workspace>,
    ) -> Entity<Buffer> {
        self.known_ops.retain(|(owner, _)| *owner != host);
        self.known_ops
            .extend(snapshot.operations.iter().map(|op| (host, op.timestamp())));
        let operations = snapshot.operations.clone();
        let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
        self.next_buffer_id += 1;
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::remote(
                buffer_id,
                ReplicaId::new(replica_id),
                Capability::ReadWrite,
                "",
            );
            buffer.apply_ops(
                operations
                    .iter()
                    .filter_map(|operation| operation.to_text().ok())
                    .map(language::Operation::Buffer)
                    .collect::<Vec<_>>(),
                cx,
            );
            buffer
        });
        let subscription = cx.subscribe(&buffer, move |workspace, _, event, _| {
            let BufferEvent::Operation {
                operation: language::Operation::Buffer(operation),
                is_local: true,
            } = event
            else {
                return;
            };
            let operation = DeskOperation::from_text(operation);
            let timestamp = operation.timestamp();
            workspace.mark_desk_text_local(host, timestamp);
            workspace.send_to_host(
                host,
                ClientMessage::DeskTextApply {
                    operation,
                    transaction: Some(DeskTransaction {
                        id: timestamp,
                        edit_ids: vec![timestamp],
                    }),
                },
            );
        });
        self.hosts.insert(
            host,
            HostDesk {
                snapshot,
                buffer: buffer.clone(),
                _subscription: subscription,
            },
        );
        buffer
    }

    pub fn apply_text(
        &mut self,
        host: HostId,
        record: DeskTextOpRecord,
        cx: &mut Context<Workspace>,
    ) {
        if !self.known_ops.insert((host, record.operation.timestamp())) {
            return;
        }
        let Some(desk) = self.hosts.get_mut(&host) else {
            return;
        };
        desk.snapshot.operations.push(record.operation.clone());
        if let Some(transaction) = record.transaction {
            desk.snapshot.transactions.push(transaction);
        }
        if let Ok(operation) = record.operation.to_text() {
            desk.buffer.update(cx, |buffer, cx| {
                buffer.apply_ops([language::Operation::Buffer(operation)], cx)
            });
        }
    }

    pub fn mark_local(&mut self, host: HostId, clock: DeskClock) {
        self.known_ops.insert((host, clock));
    }

    pub fn buffer(&self, host: HostId) -> Option<Entity<Buffer>> {
        self.hosts.get(&host).map(|desk| desk.buffer.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CycleState {
    Folded,
    Children,
    Subtree,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HighlightRanges {
    stars: Vec<Range<usize>>,
    todo: Vec<Range<usize>>,
    done: Vec<Range<usize>>,
    discarded: Vec<Range<usize>>,
    titles: Vec<Range<usize>>,
    properties: Vec<Range<usize>>,
}

fn highlight_ranges(text: &str) -> HighlightRanges {
    let mut result = HighlightRanges::default();
    for heading in parse(text) {
        result.stars.push(heading.stars_range);
        result.titles.push(heading.title_range);
        result.properties.extend(
            heading
                .properties
                .into_iter()
                .map(|property| property.line_range),
        );
        if let (Some(state), Some(range)) = (heading.state, heading.state_range) {
            match state {
                DeskHeadingState::Todo => result.todo.push(range),
                DeskHeadingState::Done => result.done.push(range),
                DeskHeadingState::Discarded => result.discarded.push(range),
            }
        }
    }
    result
}

fn subtree_end(headings: &[DeskHeading], index: usize, text_len: usize) -> usize {
    headings[index + 1..]
        .iter()
        .find(|heading| heading.depth <= headings[index].depth)
        .map_or(text_len, |heading| heading.heading_range.start)
}

fn child_indices(headings: &[DeskHeading], index: usize, text_len: usize) -> Vec<usize> {
    let end = subtree_end(headings, index, text_len);
    headings[index + 1..]
        .iter()
        .enumerate()
        .take_while(|(_, heading)| heading.heading_range.start < end)
        .filter(|(_, heading)| heading.parent == Some(index))
        .map(|(offset, _)| index + 1 + offset)
        .collect()
}

fn cycle_next(state: CycleState, has_children: bool, has_content: bool) -> Option<CycleState> {
    if !has_content {
        return None;
    }
    Some(match (state, has_children) {
        (CycleState::Folded, true) => CycleState::Children,
        (CycleState::Folded, false) => CycleState::Subtree,
        (CycleState::Children, _) => CycleState::Subtree,
        (CycleState::Subtree, _) => CycleState::Folded,
    })
}

fn fold_ranges(
    text: &str,
    headings: &[DeskHeading],
    index: usize,
    state: CycleState,
) -> Vec<Range<usize>> {
    let end = subtree_end(headings, index, text.len());
    let line_end = heading_full_end(text, &headings[index]);
    match state {
        CycleState::Subtree => Vec::new(),
        CycleState::Folded => (line_end < end)
            .then_some(line_end..end)
            .into_iter()
            .collect(),
        CycleState::Children => {
            let children = child_indices(headings, index, text.len());
            let mut ranges = Vec::new();
            let first = children
                .first()
                .map_or(end, |child| headings[*child].heading_range.start);
            if line_end < first {
                ranges.push(line_end..first);
            }
            for (position, child) in children.iter().enumerate() {
                let start = heading_full_end(text, &headings[*child]);
                let child_end = children
                    .get(position + 1)
                    .map_or(end, |next| headings[*next].heading_range.start);
                if start < child_end {
                    ranges.push(start..child_end);
                }
            }
            ranges
        }
    }
}

fn heading_full_end(text: &str, heading: &DeskHeading) -> usize {
    heading.heading_range.end
        + usize::from(text.as_bytes().get(heading.heading_range.end) == Some(&b'\n'))
}

fn heading_for_offset(text: &str, offset: usize) -> Option<(Vec<DeskHeading>, usize)> {
    let headings = parse(text);
    let index = headings
        .iter()
        .position(|heading| heading.heading_range.start == offset)?;
    Some((headings, index))
}

pub struct DeskView {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    cycle_states: HashMap<usize, CycleState>,
    _subscription: gpui::Subscription,
}

impl DeskView {
    pub fn new(buffer: Entity<Buffer>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let multibuffer = cx.new(|cx| multi_buffer::MultiBuffer::singleton(buffer.clone(), cx));
            let mut editor = Editor::new(
                EditorMode::full(),
                multibuffer,
                #[cfg(feature = "native")]
                None,
                window,
                cx,
            );
            crate::editor_config::configure_file(&mut editor, window, cx);
            editor
        });
        let subscription = cx.subscribe(&buffer, |this, _, event, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                this.cycle_states.clear();
                this.apply_highlights(cx);
            }
        });
        let mut this = Self {
            editor,
            buffer,
            cycle_states: HashMap::new(),
            _subscription: subscription,
        };
        this.apply_highlights(cx);
        this
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn shared_buffer(&self) -> Entity<Buffer> {
        self.buffer.clone()
    }

    pub fn jump_to_heading(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.buffer.read(cx).text();
        let Some((headings, index)) = heading_for_offset(&text, offset) else {
            return;
        };
        let target = headings[index].title_range.start;
        let anchor = self.buffer.read(cx).anchor_after(target);
        let snapshot = self.editor.read(cx).buffer().read(cx).snapshot(cx);
        let Some(anchor) = snapshot.anchor_in_excerpt(anchor) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
            editor.request_autoscroll(editor::scroll::Autoscroll::fit(), cx);
        });
    }

    fn apply_highlights(&mut self, cx: &mut Context<Self>) {
        let text = self.buffer.read(cx).text();
        let ranges = highlight_ranges(&text);
        let muted = cx.theme().colors().text_muted;
        let warning = cx.theme().status().warning;
        let success = cx.theme().status().success;
        let to_anchors = |ranges: Vec<Range<usize>>| {
            let buffer = self.buffer.read(cx);
            let snapshot = self.editor.read(cx).buffer().read(cx).snapshot(cx);
            ranges
                .into_iter()
                .filter_map(|range| {
                    Some(
                        snapshot.anchor_in_excerpt(buffer.anchor_after(range.start))?
                            ..snapshot.anchor_in_excerpt(buffer.anchor_before(range.end))?,
                    )
                })
                .collect()
        };
        let styles = [
            (
                0,
                ranges.stars,
                HighlightStyle {
                    color: Some(muted.into()),
                    ..Default::default()
                },
            ),
            (
                1,
                ranges.todo,
                HighlightStyle {
                    color: Some(warning.into()),
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                },
            ),
            (
                2,
                ranges.done,
                HighlightStyle {
                    color: Some(success.into()),
                    ..Default::default()
                },
            ),
            (
                3,
                ranges.discarded,
                HighlightStyle {
                    color: Some(muted.into()),
                    strikethrough: Some(StrikethroughStyle {
                        thickness: px(1.),
                        color: Some(muted.into()),
                    }),
                    ..Default::default()
                },
            ),
            (
                4,
                ranges.titles,
                HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                },
            ),
            (
                5,
                ranges.properties,
                HighlightStyle {
                    color: Some(muted.into()),
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .map(|(slot, ranges, style)| (slot, to_anchors(ranges), style))
        .collect::<Vec<_>>();
        self.editor.update(cx, |editor, cx| {
            for (slot, ranges, style) in styles {
                editor.highlight_text(
                    HighlightKey::SyntaxTreeView(DESK_KEY_BASE + slot),
                    ranges,
                    style,
                    cx,
                );
            }
        });
    }

    fn cycle(&mut self, _: &DeskCycle, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.editor.update(cx, |editor, cx| {
            let point = editor
                .selections
                .newest::<language::Point>(&editor.display_snapshot(cx))
                .head();
            editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .point_to_buffer_offset(point)
                .map(|(_, offset)| offset.0)
        });
        let text = self.buffer.read(cx).text();
        let headings = parse(&text);
        let Some((index, heading)) = headings.iter().enumerate().find(|(_, heading)| {
            cursor.is_some_and(|offset| {
                heading.heading_range.contains(&offset) || offset == heading.heading_range.end
            })
        }) else {
            cx.propagate();
            return;
        };
        let end = subtree_end(&headings, index, text.len());
        let line_end = heading_full_end(&text, heading);
        let has_children = !child_indices(&headings, index, text.len()).is_empty();
        let has_content = line_end < end;
        let current = self
            .cycle_states
            .get(&heading.heading_range.start)
            .copied()
            .unwrap_or(CycleState::Subtree);
        let Some(next) = cycle_next(current, has_children, has_content) else {
            return;
        };
        self.cycle_states.insert(heading.heading_range.start, next);
        self.editor.update(cx, |editor, cx| {
            editor.unfold_ranges(
                &[MultiBufferOffset(heading.heading_range.start)..MultiBufferOffset(end)],
                true,
                false,
                cx,
            );
            editor.fold_ranges(
                fold_ranges(&text, &headings, index, next)
                    .into_iter()
                    .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end))
                    .collect(),
                false,
                window,
                cx,
            );
        });
    }
}

impl Render for DeskView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoDeskView")
            .on_action(cx.listener(Self::cycle))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtree_ranges_cover_nested_siblings_and_eof() {
        let text = "* A\nbody\n** B\nb\n*** C\nc\n** D\nd\n* E\ne";
        let headings = parse(text);
        assert_eq!(
            subtree_end(&headings, 0, text.len()),
            headings[4].heading_range.start
        );
        assert_eq!(
            subtree_end(&headings, 1, text.len()),
            headings[3].heading_range.start
        );
        assert_eq!(subtree_end(&headings, 4, text.len()), text.len());
        assert_eq!(
            fold_ranges(text, &headings, 0, CycleState::Children),
            vec![4..9, 14..24, 29..31]
        );
    }

    #[test]
    fn cycle_skips_children_without_children_and_empty_does_nothing() {
        assert_eq!(
            cycle_next(CycleState::Folded, true, true),
            Some(CycleState::Children)
        );
        assert_eq!(
            cycle_next(CycleState::Children, true, true),
            Some(CycleState::Subtree)
        );
        assert_eq!(
            cycle_next(CycleState::Subtree, true, true),
            Some(CycleState::Folded)
        );
        assert_eq!(
            cycle_next(CycleState::Folded, false, true),
            Some(CycleState::Subtree)
        );
        assert_eq!(
            cycle_next(CycleState::Subtree, false, true),
            Some(CycleState::Folded)
        );
        assert_eq!(cycle_next(CycleState::Subtree, false, false), None);
    }

    #[test]
    fn highlights_only_parser_owned_spans() {
        let text =
            "* TODO Work\n:agent: eng-a\nprose TODO\n** DONE Child\n:other: value\n* DISCARDED Old";
        let ranges = highlight_ranges(text);
        assert_eq!(
            ranges
                .todo
                .iter()
                .map(|r| &text[r.clone()])
                .collect::<Vec<_>>(),
            ["TODO"]
        );
        assert_eq!(
            ranges
                .done
                .iter()
                .map(|r| &text[r.clone()])
                .collect::<Vec<_>>(),
            ["DONE"]
        );
        assert_eq!(
            ranges
                .discarded
                .iter()
                .map(|r| &text[r.clone()])
                .collect::<Vec<_>>(),
            ["DISCARDED"]
        );
        assert_eq!(
            ranges
                .properties
                .iter()
                .map(|r| &text[r.clone()])
                .collect::<Vec<_>>(),
            [":agent: eng-a", ":other: value"]
        );
        assert_eq!(
            ranges
                .titles
                .iter()
                .map(|r| &text[r.clone()])
                .collect::<Vec<_>>(),
            ["Work", "Child", "Old"]
        );
    }

    #[test]
    fn dashboard_offset_maps_to_title() {
        let text = "* TODO Work\nbody\n";
        let (headings, index) = heading_for_offset(text, 0).unwrap();
        assert_eq!(&text[headings[index].title_range.clone()], "Work");
        assert!(heading_for_offset(text, 1).is_none());
    }
}
