//! Composition: host documents with foreign buffers spliced in at cut
//! points, as one editable surface.
//!
//! A composition is the generalization of a multibuffer: instead of a
//! flat list of excerpts ordered by external keys, each *section* is a
//! host buffer whose own text drives the layout, with *rows* (whole
//! foreign buffers, e.g. generated annotation lines or writable drafts)
//! spliced in at *cuts*. Between syncs everything self-maintains: the
//! engine stores excerpt ranges as anchors, so editing the host moves
//! the slices, the rows, and any cursors together without a single
//! engine call. `sync` is needed only when the *structure* changes —
//! a cut moves, a row appears or disappears — and it touches only the
//! affected elements, so unchanged excerpts keep their identity (and
//! with it cursor anchors, scroll position, and highlights).
//!
//! Requires `set_multiple_paths_per_buffer(true)` on the multibuffer:
//! one host appears under one path per slice.

use std::ops::Range;

use collections::HashMap;
use gpui::{App, Entity};
use language::Buffer;
use text::{BufferId, ToOffset as _};

use crate::{MultiBuffer, PathKey};

/// Caller-stable identity for cuts and rows. Reusing an id across syncs
/// is what preserves the underlying excerpt.
pub type ElementKey = u64;

/// Initial spacing between path sort keys; insertions between two
/// neighbors bisect the gap, so ~32 nested insertions fit before a
/// global renumber.
const KEY_GAP: u64 = 1 << 32;

#[derive(Clone, Default)]
pub struct CompositionSpec {
    pub sections: Vec<SectionSpec>,
    /// Rows appended after every section — group headers, listings, and
    /// action lines that belong to the surface rather than to any host
    /// (and the whole surface, when there is no host yet).
    pub tail: Vec<RowSpec>,
}

#[derive(Clone)]
pub struct SectionSpec {
    pub host: Entity<Buffer>,
    /// Host offset where this projected section begins. Ordinary composed
    /// documents use zero; narrowed projections can omit an arbitrary prefix
    /// without materializing a synthetic empty excerpt at offset zero.
    pub start: usize,
    /// Host offset where this projected section ends. Narrowed projections
    /// use this to omit a suffix without relying on a cut whose endpoint can
    /// still include the following line when converted to an excerpt point.
    pub end: Option<usize>,
    /// Rows shown before the section's first slice — e.g. a group
    /// header naming the host.
    pub lead: Vec<RowSpec>,
    /// Ascending, non-overlapping cut points.
    pub cuts: Vec<CutSpec>,
}

#[derive(Clone)]
pub struct CutSpec {
    pub id: ElementKey,
    /// Host offset where the preceding slice ends (typically the end of
    /// a line, excluding its newline).
    pub position: usize,
    /// Host offset where the following slice resumes (typically the
    /// start of the next line). Text in `position..resume` — usually
    /// just the newline, or a folded body — is not displayed; the
    /// synthetic newline between excerpts stands in for it.
    pub resume: usize,
    pub rows: Vec<RowSpec>,
}

#[derive(Clone)]
pub struct RowSpec {
    pub id: ElementKey,
    pub buffer: Entity<Buffer>,
}

/// What one path key currently shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ElementId {
    /// The slice of a host from the section start to its first cut.
    SectionStart(BufferId),
    /// The slice of a host resuming after the cut with this id.
    AfterCut(BufferId, ElementKey),
    Row(ElementKey),
}

struct ElementState {
    id: ElementId,
    sort_key: u64,
    content: ElementContent,
}

enum ElementContent {
    Slice {
        host: Entity<Buffer>,
        range: Range<text::Anchor>,
    },
    Row {
        buffer: Entity<Buffer>,
    },
}

/// One desired element, before diffing against the current state.
struct DesiredElement {
    id: ElementId,
    content: DesiredContent,
}

enum DesiredContent {
    Slice {
        host: Entity<Buffer>,
        range: Range<usize>,
    },
    Row {
        buffer: Entity<Buffer>,
    },
}

#[derive(Default)]
pub struct Composition {
    /// Current elements in display order.
    elements: Vec<ElementState>,
}

impl Composition {
    /// Reconciles the multibuffer to show `spec`, by identity: elements
    /// whose id, order, and content are unchanged are not touched, so
    /// their excerpts — and all anchors into them — survive. Returns
    /// true if anything changed.
    pub fn sync(
        &mut self,
        multibuffer: &Entity<MultiBuffer>,
        spec: &CompositionSpec,
        cx: &mut App,
    ) -> bool {
        let desired = Self::desired_elements(spec, cx);

        // Assign sort keys: keep an element's key when its relative
        // order still holds; otherwise treat it as an insertion between
        // its new neighbors. If a gap is exhausted, renumber everything
        // (rare; costs all excerpt identity at once).
        let existing_keys: HashMap<ElementId, u64> = self
            .elements
            .iter()
            .map(|element| (element.id.clone(), element.sort_key))
            .collect();
        let keys = Self::assign_keys(&desired, &existing_keys).unwrap_or_else(|| {
            (0..desired.len() as u64)
                .map(|i| (i + 1) * KEY_GAP)
                .collect()
        });

        let mut changed = false;

        // Remove elements that disappeared or whose key moved.
        let desired_ids: HashMap<&ElementId, u64> = desired
            .iter()
            .zip(keys.iter())
            .map(|(element, key)| (&element.id, *key))
            .collect();
        for element in &self.elements {
            if desired_ids.get(&element.id) != Some(&element.sort_key) {
                multibuffer.update(cx, |multibuffer, cx| {
                    multibuffer.remove_excerpts(PathKey::sorted(element.sort_key), cx)
                });
                changed = true;
            }
        }

        // Add or update elements whose content moved.
        let mut new_elements = Vec::with_capacity(desired.len());
        for (element, sort_key) in desired.into_iter().zip(keys) {
            let unchanged = existing_keys.get(&element.id) == Some(&sort_key)
                && self
                    .elements
                    .iter()
                    .find(|existing| existing.id == element.id)
                    .is_some_and(|existing| existing.matches(&element.content, cx));
            let content = match element.content {
                DesiredContent::Slice { host, range } => {
                    let snapshot = host.read(cx).snapshot();
                    let anchors =
                        snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end);
                    if !unchanged {
                        let points = snapshot.offset_to_point(range.start)
                            ..snapshot.offset_to_point(range.end);
                        multibuffer.update(cx, |multibuffer, cx| {
                            multibuffer.set_excerpts_for_path(
                                PathKey::sorted(sort_key),
                                host.clone(),
                                [points],
                                0,
                                cx,
                            )
                        });
                        changed = true;
                    }
                    ElementContent::Slice {
                        host,
                        range: anchors,
                    }
                }
                DesiredContent::Row { buffer } => {
                    if !unchanged {
                        let max_point = buffer.read(cx).max_point();
                        multibuffer.update(cx, |multibuffer, cx| {
                            multibuffer.set_excerpts_for_path(
                                PathKey::sorted(sort_key),
                                buffer.clone(),
                                [rope::Point::zero()..max_point],
                                0,
                                cx,
                            )
                        });
                        changed = true;
                    }
                    ElementContent::Row { buffer }
                }
            };
            new_elements.push(ElementState {
                id: element.id,
                sort_key,
                content,
            });
        }
        self.elements = new_elements;
        changed
    }

    /// The path key currently showing an element, for anchor lookups.
    pub fn path_for_row(&self, id: ElementKey) -> Option<PathKey> {
        self.elements
            .iter()
            .find(|element| element.id == ElementId::Row(id))
            .map(|element| PathKey::sorted(element.sort_key))
    }

    fn desired_elements(spec: &CompositionSpec, cx: &App) -> Vec<DesiredElement> {
        let mut desired = Vec::new();
        for section in &spec.sections {
            let host_id = section.host.read(cx).remote_id();
            let host_len = section.host.read(cx).len();
            let mut len = section.end.unwrap_or(host_len).min(host_len);
            // Excerpt point ranges include the row containing their endpoint.
            // A narrowed byte boundary at the start of the following line must
            // therefore end before the separating newline, or that following
            // line becomes visible in the section.
            if len < host_len
                && len > 0
                && section
                    .host
                    .read(cx)
                    .text_for_range(len - 1..len)
                    .collect::<String>()
                    == "\n"
            {
                len -= 1;
            }
            for row in &section.lead {
                desired.push(DesiredElement {
                    id: ElementId::Row(row.id),
                    content: DesiredContent::Row {
                        buffer: row.buffer.clone(),
                    },
                });
            }
            let mut slice_start = section.start.min(len);
            let mut slice_id = ElementId::SectionStart(host_id);
            for cut in &section.cuts {
                let position = cut.position.min(len);
                let resume = cut.resume.clamp(position, len);
                if position > slice_start || matches!(slice_id, ElementId::SectionStart(_)) {
                    // The leading slice is kept even when empty so an
                    // empty document still has an excerpt to type into.
                    desired.push(DesiredElement {
                        id: slice_id.clone(),
                        content: DesiredContent::Slice {
                            host: section.host.clone(),
                            range: slice_start..position,
                        },
                    });
                }
                for row in &cut.rows {
                    desired.push(DesiredElement {
                        id: ElementId::Row(row.id),
                        content: DesiredContent::Row {
                            buffer: row.buffer.clone(),
                        },
                    });
                }
                slice_start = resume;
                slice_id = ElementId::AfterCut(host_id, cut.id);
            }
            if slice_start < len || matches!(slice_id, ElementId::SectionStart(_)) {
                desired.push(DesiredElement {
                    id: slice_id,
                    content: DesiredContent::Slice {
                        host: section.host.clone(),
                        range: slice_start..len,
                    },
                });
            }
        }
        for row in &spec.tail {
            desired.push(DesiredElement {
                id: ElementId::Row(row.id),
                content: DesiredContent::Row {
                    buffer: row.buffer.clone(),
                },
            });
        }
        desired
    }

    /// Keys for `desired`, reusing existing keys where the old relative
    /// order still holds and bisecting gaps for insertions. None when a
    /// gap is exhausted.
    fn assign_keys(
        desired: &[DesiredElement],
        existing: &HashMap<ElementId, u64>,
    ) -> Option<Vec<u64>> {
        // An existing key is reusable only if it is ascending with
        // respect to the previous *reused* key: reordered elements fall
        // back to insertion.
        let mut reusable = vec![false; desired.len()];
        let mut last = 0u64;
        for (index, element) in desired.iter().enumerate() {
            if let Some(&key) = existing.get(&element.id)
                && key > last
            {
                reusable[index] = true;
                last = key;
            }
        }
        let mut keys = Vec::with_capacity(desired.len());
        let mut last = 0u64;
        let mut index = 0;
        while index < desired.len() {
            if reusable[index] {
                let key = existing[&desired[index].id];
                keys.push(key);
                last = key;
                index += 1;
                continue;
            }
            // A run of insertions between `last` and the next reused key.
            let mut run_end = index;
            while run_end < desired.len() && !reusable[run_end] {
                run_end += 1;
            }
            let upper = if run_end < desired.len() {
                existing[&desired[run_end].id]
            } else {
                last.saturating_add(KEY_GAP.saturating_mul((run_end - index) as u64 + 1))
                    .max(last + (run_end - index) as u64 + 1)
            };
            let count = (run_end - index) as u64;
            if upper - last <= count {
                return None;
            }
            let step = (upper - last) / (count + 1);
            for i in 0..count {
                let key = last + step * (i + 1);
                keys.push(key);
            }
            last = *keys.last().unwrap();
            index = run_end;
        }
        Some(keys)
    }
}

impl ElementState {
    /// Whether this element already shows the desired content: same
    /// buffer, and (for slices) stored anchors resolving to the desired
    /// offsets — which they do across host edits, since anchors move
    /// with the text.
    fn matches(&self, desired: &DesiredContent, cx: &App) -> bool {
        match (&self.content, desired) {
            (
                ElementContent::Slice { host, range },
                DesiredContent::Slice {
                    host: desired_host,
                    range: desired_range,
                },
            ) => {
                if host.entity_id() != desired_host.entity_id() {
                    return false;
                }
                let snapshot = host.read(cx).snapshot();
                let resolved = range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot);
                resolved == *desired_range
            }
            (
                ElementContent::Row { buffer },
                DesiredContent::Row {
                    buffer: desired_buffer,
                },
            ) => buffer.entity_id() == desired_buffer.entity_id(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Capability, ToOffset as _};
    use gpui::{AppContext as _, TestAppContext};
    use language::Buffer;

    fn build(cx: &mut TestAppContext) -> Entity<MultiBuffer> {
        cx.new(|_| {
            let mut multibuffer = MultiBuffer::without_headers(Capability::ReadWrite);
            multibuffer.set_multiple_paths_per_buffer(true);
            multibuffer
        })
    }

    fn buffer(text: &str, cx: &mut TestAppContext) -> Entity<Buffer> {
        cx.new(|cx| Buffer::local(text, cx))
    }

    fn text(multibuffer: &Entity<MultiBuffer>, cx: &mut TestAppContext) -> String {
        multibuffer.read_with(cx, |multibuffer, cx| multibuffer.snapshot(cx).text())
    }

    /// A multibuffer anchor at the composed text `needle`, for asserting
    /// that untouched excerpts keep their identity across syncs.
    fn anchor_at(
        multibuffer: &Entity<MultiBuffer>,
        needle: &str,
        cx: &mut TestAppContext,
    ) -> crate::Anchor {
        multibuffer.read_with(cx, |multibuffer, cx| {
            let snapshot = multibuffer.snapshot(cx);
            let offset = snapshot.text().find(needle).unwrap();
            snapshot.anchor_before(crate::MultiBufferOffset(offset))
        })
    }

    fn resolve<'a>(
        multibuffer: &Entity<MultiBuffer>,
        anchor: &crate::Anchor,
        len: usize,
        cx: &mut TestAppContext,
    ) -> String {
        multibuffer.read_with(cx, |multibuffer, cx| {
            let snapshot = multibuffer.snapshot(cx);
            let offset = anchor.to_offset(&snapshot);
            snapshot
                .text()
                .get(offset.0..offset.0 + len)
                .unwrap_or_default()
                .to_string()
        })
    }

    /// host "## A\nalpha…\n<resume_line>…" with a row spliced in before
    /// `resume_line`.
    fn spec_with_row(
        host: &Entity<Buffer>,
        row: &Entity<Buffer>,
        resume_line: &str,
        cx: &mut TestAppContext,
    ) -> CompositionSpec {
        let text = host.read_with(cx, |host, _| host.text());
        let resume = text.find(resume_line).unwrap();
        let cut = resume - 1;
        assert_eq!(&text[cut..resume], "\n");
        CompositionSpec {
            sections: vec![SectionSpec {
                host: host.clone(),
                start: 0,
                end: None,
                lead: Vec::new(),
                cuts: vec![CutSpec {
                    id: 1,
                    position: cut,
                    resume,
                    rows: vec![RowSpec {
                        id: 100,
                        buffer: row.clone(),
                    }],
                }],
            }],
            tail: Vec::new(),
        }
    }

    #[gpui::test]
    fn test_compose_and_self_maintain(cx: &mut TestAppContext) {
        let multibuffer = build(cx);
        let host = buffer("## A\nalpha\n## B\nbeta\n", cx);
        let row = buffer("· agent-row", cx);
        let mut composition = Composition::default();

        let spec = spec_with_row(&host, &row, "## B", cx);
        let changed = cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert!(changed);
        assert_eq!(
            text(&multibuffer, cx),
            "## A\nalpha\n· agent-row\n## B\nbeta\n"
        );

        // Editing the host anywhere — including at the cut's edges —
        // needs no sync: anchors carry the slices.
        let beta = anchor_at(&multibuffer, "beta", cx);
        host.update(cx, |host, cx| {
            let offset = host.text().find("alpha").unwrap() + "alpha".len();
            host.edit([(offset..offset, " typed")], None, cx);
        });
        assert_eq!(
            text(&multibuffer, cx),
            "## A\nalpha typed\n· agent-row\n## B\nbeta\n"
        );
        // Typing at the start of the resume line lands in the tail slice.
        host.update(cx, |host, cx| {
            let offset = host.text().find("## B").unwrap();
            host.edit([(offset..offset, "x")], None, cx);
        });
        assert_eq!(
            text(&multibuffer, cx),
            "## A\nalpha typed\n· agent-row\nx## B\nbeta\n"
        );

        // Re-syncing with the recomputed structure is a no-op: same
        // identities, anchors already resolve to the new offsets.
        let spec = spec_with_row(&host, &row, "x## B", cx);
        let changed = cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert!(!changed);
        assert_eq!(resolve(&multibuffer, &beta, 4, cx), "beta");

        // Rewriting a row buffer updates in place, no sync needed.
        row.update(cx, |row, cx| {
            let len = row.len();
            row.edit([(0..len, "● agent-row loud")], None, cx);
        });
        assert!(text(&multibuffer, cx).contains("● agent-row loud"));
    }

    #[gpui::test]
    fn test_structural_changes_touch_only_their_elements(cx: &mut TestAppContext) {
        let multibuffer = build(cx);
        let host = buffer("## A\nalpha\n## B\nbeta\n", cx);
        let row = buffer("row-one", cx);
        let mut composition = Composition::default();
        let mut spec = spec_with_row(&host, &row, "## B", cx);
        cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        let alpha = anchor_at(&multibuffer, "alpha", cx);
        let beta = anchor_at(&multibuffer, "beta", cx);
        let row_anchor = anchor_at(&multibuffer, "row-one", cx);

        // A second row under the same cut: every prior excerpt survives.
        let draft = buffer("", cx);
        spec.sections[0].cuts[0].rows.push(RowSpec {
            id: 101,
            buffer: draft.clone(),
        });
        let changed = cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert!(changed);
        assert_eq!(
            text(&multibuffer, cx),
            "## A\nalpha\nrow-one\n\n## B\nbeta\n"
        );
        // Untouched elements keep their excerpts: anchors still resolve.
        assert_eq!(resolve(&multibuffer, &alpha, 5, cx), "alpha");
        assert_eq!(resolve(&multibuffer, &beta, 4, cx), "beta");
        assert_eq!(resolve(&multibuffer, &row_anchor, 7, cx), "row-one");

        // Typing into the (writable) draft grows its excerpt in place.
        draft.update(cx, |draft, cx| {
            draft.edit([(0..0, "reply text")], None, cx);
        });
        assert_eq!(
            text(&multibuffer, cx),
            "## A\nalpha\nrow-one\nreply text\n## B\nbeta\n"
        );

        // Dropping the cut merges the document back into one slice.
        spec.sections[0].cuts.clear();
        let changed = cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert!(changed);
        assert_eq!(text(&multibuffer, cx), "## A\nalpha\n## B\nbeta\n");
    }

    #[gpui::test]
    fn test_one_row_buffer_can_be_projected_at_multiple_paths(cx: &mut TestAppContext) {
        let multibuffer = build(cx);
        let host = buffer("## A\n## B\n", cx);
        let row = buffer("runtime", cx);
        let mut composition = Composition::default();
        let spec = CompositionSpec {
            sections: vec![SectionSpec {
                host,
                start: 0,
                end: None,
                lead: Vec::new(),
                cuts: vec![CutSpec {
                    id: 1,
                    position: 4,
                    resume: 5,
                    rows: vec![RowSpec {
                        id: 100,
                        buffer: row.clone(),
                    }],
                }],
            }],
            tail: vec![RowSpec {
                id: 101,
                buffer: row,
            }],
        };
        cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert_eq!(text(&multibuffer, cx).matches("runtime").count(), 2);

        let first_path = composition.path_for_row(100).unwrap();
        let second_path = composition.path_for_row(101).unwrap();
        multibuffer.read_with(cx, |multibuffer, cx| {
            let snapshot = multibuffer.snapshot(cx);
            let first = multibuffer.location_for_path(&first_path, cx).unwrap();
            let second = multibuffer.location_for_path(&second_path, cx).unwrap();
            assert_ne!(first.to_offset(&snapshot), second.to_offset(&snapshot));
            let crate::Anchor::Excerpt(first) = first else {
                panic!("row location should be an excerpt anchor")
            };
            let crate::Anchor::Excerpt(second) = second else {
                panic!("row location should be an excerpt anchor")
            };
            assert_eq!(snapshot.path_for_anchor(first), &first_path);
            assert_eq!(snapshot.path_for_anchor(second), &second_path);
        });
    }

    #[gpui::test]
    fn test_folded_cut_hides_a_body(cx: &mut TestAppContext) {
        let multibuffer = build(cx);
        let host = buffer("## A\nalpha\n## B\nbeta\n", cx);
        let fold = buffer("… 1 more", cx);
        let mut composition = Composition::default();
        // Cut at the end of "## A", resuming past alpha's line: the body
        // is folded behind the row.
        let spec = CompositionSpec {
            sections: vec![SectionSpec {
                host: host.clone(),
                start: 0,
                end: None,
                lead: Vec::new(),
                cuts: vec![CutSpec {
                    id: 1,
                    position: 4,
                    resume: 11,
                    rows: vec![RowSpec {
                        id: 100,
                        buffer: fold.clone(),
                    }],
                }],
            }],
            tail: Vec::new(),
        };
        cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert_eq!(text(&multibuffer, cx), "## A\n… 1 more\n## B\nbeta\n");
    }

    #[gpui::test]
    fn test_empty_host_keeps_an_editable_slice(cx: &mut TestAppContext) {
        let multibuffer = build(cx);
        let host = buffer("", cx);
        let tail = buffer("+ new agent", cx);
        let mut composition = Composition::default();
        let spec = CompositionSpec {
            sections: vec![SectionSpec {
                host: host.clone(),
                start: 0,
                end: None,
                lead: Vec::new(),
                cuts: vec![],
            }],
            tail: vec![RowSpec {
                id: 100,
                buffer: tail.clone(),
            }],
        };
        cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert_eq!(text(&multibuffer, cx), "\n+ new agent");
        // The empty slice accepts typing.
        host.update(cx, |host, cx| {
            host.edit([(0..0, "* First topic")], None, cx);
        });
        assert_eq!(text(&multibuffer, cx), "* First topic\n+ new agent");
    }

    #[gpui::test]
    fn test_two_sections_compose_in_order(cx: &mut TestAppContext) {
        let multibuffer = build(cx);
        let host_a = buffer("host a", cx);
        let host_b = buffer("host b", cx);
        let mut composition = Composition::default();
        let spec = CompositionSpec {
            sections: vec![
                SectionSpec {
                    host: host_a.clone(),
                    start: 0,
                    end: None,
                    lead: Vec::new(),
                    cuts: vec![],
                },
                SectionSpec {
                    host: host_b.clone(),
                    start: 0,
                    end: None,
                    lead: Vec::new(),
                    cuts: vec![],
                },
            ],
            tail: Vec::new(),
        };
        cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert_eq!(text(&multibuffer, cx), "host a\nhost b");
    }

    #[gpui::test]
    fn test_section_can_start_below_host_prefix(cx: &mut TestAppContext) {
        let multibuffer = build(cx);
        let host = buffer("hidden\ncard\n", cx);
        let mut composition = Composition::default();
        let spec = CompositionSpec {
            sections: vec![SectionSpec {
                host,
                start: "hidden\n".len(),
                end: None,
                lead: Vec::new(),
                cuts: Vec::new(),
            }],
            tail: Vec::new(),
        };
        cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert_eq!(text(&multibuffer, cx), "card\n");
    }

    #[gpui::test]
    fn test_section_can_end_before_following_line(cx: &mut TestAppContext) {
        let multibuffer = build(cx);
        let host = buffer("card\nnext heading\n", cx);
        let mut composition = Composition::default();
        let spec = CompositionSpec {
            sections: vec![SectionSpec {
                host,
                start: 0,
                end: Some("card\n".len()),
                lead: Vec::new(),
                cuts: Vec::new(),
            }],
            tail: Vec::new(),
        };
        cx.update(|cx| composition.sync(&multibuffer, &spec, cx));
        assert_eq!(text(&multibuffer, cx), "card\n");
    }
}
