//! Wire vocabulary and forgiving parser for the daemon-owned Desk document.

use std::ops::Range;
use std::sync::Arc;

use clock::{Global, Lamport, ReplicaId};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use text::{Buffer, BufferId, EditOperation, FullOffset, Operation, UndoOperation};

use crate::AgentId;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack)]
pub struct DeskIdToken(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DeskHeadingState {
    Todo,
    Staffed,
    Done,
    Discarded,
}

impl DeskHeadingState {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Todo => "TODO",
            Self::Staffed => "STAFFED",
            Self::Done => "DONE",
            Self::Discarded => "DISCARDED",
        }
    }
}

/// A heading derived from Desk text. All ranges are UTF-8 byte ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeskHeading {
    pub depth: usize,
    pub state: Option<DeskHeadingState>,
    pub title: String,
    pub token: Option<DeskIdToken>,
    pub duplicate_token: bool,
    pub parent: Option<usize>,
    pub heading_range: Range<usize>,
    pub body_range: Range<usize>,
    pub id_line_range: Option<Range<usize>>,
}

/// Parse an org-like Desk document. This deliberately has no error result:
/// incomplete edits and malformed markup remain ordinary body text.
pub fn parse(text: &str) -> Vec<DeskHeading> {
    #[derive(Debug)]
    struct Line<'a> {
        start: usize,
        end: usize,
        full_end: usize,
        text: &'a str,
    }

    let mut lines = Vec::new();
    let mut start = 0;
    for part in text.split_inclusive('\n') {
        let end = start + part.strip_suffix('\n').unwrap_or(part).len();
        lines.push(Line {
            start,
            end,
            full_end: start + part.len(),
            text: &text[start..end],
        });
        start += part.len();
    }
    if start < text.len() || (text.is_empty() || !text.ends_with('\n')) {
        if lines.last().is_none_or(|line| line.full_end != text.len()) {
            lines.push(Line {
                start,
                end: text.len(),
                full_end: text.len(),
                text: &text[start..],
            });
        }
    }

    let mut headings = Vec::<DeskHeading>::new();
    let mut heading_lines = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for (line_index, line) in lines.iter().enumerate() {
        let depth = line.text.bytes().take_while(|byte| *byte == b'*').count();
        if depth == 0 || line.text.as_bytes().get(depth) != Some(&b' ') {
            continue;
        }
        let content = line.text[depth + 1..].trim_end();
        let (state, title) = parse_state(content);
        while stack
            .last()
            .is_some_and(|(ancestor_depth, _)| *ancestor_depth >= depth)
        {
            stack.pop();
        }
        let parent = stack.last().map(|(_, index)| *index);
        let index = headings.len();
        headings.push(DeskHeading {
            depth,
            state,
            title: title.to_owned(),
            token: None,
            duplicate_token: false,
            parent,
            heading_range: line.start..line.end,
            body_range: line.full_end..text.len(),
            id_line_range: None,
        });
        heading_lines.push(line_index);
        stack.push((depth, index));
    }

    let mut seen = std::collections::BTreeSet::new();
    for index in 0..headings.len() {
        let line_index = heading_lines[index];
        let end = heading_lines
            .get(index + 1)
            .map_or(text.len(), |next| lines[*next].start);
        let mut body_start = lines[line_index].full_end;
        if let Some(line) = lines.get(line_index + 1)
            && line.start < end
            && let Some(token) = parse_id_line(line.text)
        {
            let token = DeskIdToken(token.to_owned());
            // Paste may duplicate identity text. The first occurrence in
            // document order owns the binding; later copies are decoration-
            // only and are deliberately ignored by the binding join.
            headings[index].duplicate_token = !seen.insert(token.clone());
            headings[index].token = Some(token);
            headings[index].id_line_range = Some(line.start..line.end);
            body_start = line.full_end;
        }
        headings[index].body_range = body_start..end;
    }
    headings
}

fn parse_state(content: &str) -> (Option<DeskHeadingState>, &str) {
    for (keyword, state) in [
        ("TODO", DeskHeadingState::Todo),
        ("STAFFED", DeskHeadingState::Staffed),
        ("DONE", DeskHeadingState::Done),
        ("DISCARDED", DeskHeadingState::Discarded),
    ] {
        if let Some(rest) = content.strip_prefix(keyword)
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return (Some(state), rest.trim_start());
        }
    }
    (None, content.trim_start())
}

fn parse_id_line(line: &str) -> Option<&str> {
    let token = line.trim().strip_prefix(":id:")?.trim();
    (!token.is_empty() && !token.contains(char::is_whitespace)).then_some(token)
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct DeskClock {
    pub replica_id: u16,
    pub value: u32,
}

impl From<Lamport> for DeskClock {
    fn from(value: Lamport) -> Self {
        Self {
            replica_id: value.replica_id.as_u16(),
            value: value.value,
        }
    }
}

impl From<DeskClock> for Lamport {
    fn from(value: DeskClock) -> Self {
        Self {
            replica_id: ReplicaId::new(value.replica_id),
            value: value.value,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DeskOperation {
    Edit {
        timestamp: DeskClock,
        version: Vec<DeskClock>,
        ranges: Vec<(u64, u64)>,
        new_text: Vec<String>,
    },
    Undo {
        timestamp: DeskClock,
        version: Vec<DeskClock>,
        counts: Vec<(DeskClock, u32)>,
    },
}

impl DeskOperation {
    pub fn timestamp(&self) -> DeskClock {
        match self {
            Self::Edit { timestamp, .. } | Self::Undo { timestamp, .. } => *timestamp,
        }
    }
    pub fn replica_id(&self) -> u16 {
        self.timestamp().replica_id
    }
    pub fn from_text(operation: &Operation) -> Self {
        match operation {
            Operation::Edit(edit) => Self::Edit {
                timestamp: edit.timestamp.into(),
                version: version_to_wire(&edit.version),
                ranges: edit
                    .ranges
                    .iter()
                    .map(|r| (r.start.0 as u64, r.end.0 as u64))
                    .collect(),
                new_text: edit.new_text.iter().map(ToString::to_string).collect(),
            },
            Operation::Undo(undo) => Self::Undo {
                timestamp: undo.timestamp.into(),
                version: version_to_wire(&undo.version),
                counts: undo.counts.iter().map(|(c, n)| ((*c).into(), *n)).collect(),
            },
        }
    }
    pub fn to_text(&self) -> Result<Operation, String> {
        Ok(match self {
            Self::Edit {
                timestamp,
                version,
                ranges,
                new_text,
            } => {
                if ranges.len() != new_text.len() {
                    return Err("Desk edit range/text count mismatch".into());
                }
                if ranges.len() > 65_536
                    || new_text.iter().map(String::len).sum::<usize>() > 4 * 1024 * 1024
                {
                    return Err("Desk edit is too large".into());
                }
                Operation::Edit(EditOperation {
                    timestamp: (*timestamp).into(),
                    version: version_from_wire(version),
                    ranges: ranges
                        .iter()
                        .map(|(s, e)| {
                            Ok(FullOffset(
                                usize::try_from(*s).map_err(|_| "Desk edit offset overflow")?,
                            )
                                ..FullOffset(
                                    usize::try_from(*e).map_err(|_| "Desk edit offset overflow")?,
                                ))
                        })
                        .collect::<Result<_, String>>()?,
                    new_text: new_text.iter().map(|s| Arc::from(s.as_str())).collect(),
                })
            }
            Self::Undo {
                timestamp,
                version,
                counts,
            } => {
                if counts.len() > 65_536 {
                    return Err("Desk undo is too large".into());
                }
                Operation::Undo(UndoOperation {
                    timestamp: (*timestamp).into(),
                    version: version_from_wire(version),
                    counts: counts.iter().map(|(c, n)| ((*c).into(), *n)).collect(),
                })
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskTransaction {
    pub id: DeskClock,
    pub edit_ids: Vec<DeskClock>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DeskReplicaAuthor {
    User,
    Agent(AgentId),
    Gatekeeper,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskReplica {
    pub replica_id: u16,
    pub author: DeskReplicaAuthor,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskBinding {
    pub token: DeskIdToken,
    pub agent_id: AgentId,
    pub orphaned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskTextOpRecord {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub operation: DeskOperation,
    pub transaction: Option<DeskTransaction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack, Default)]
pub struct DeskSnapshot {
    /// Materialized document at snapshot time. Operation history accompanies
    /// it so clients can continue the native CRDT clock without rebasing.
    pub text: String,
    pub operations: Vec<DeskOperation>,
    pub transactions: Vec<DeskTransaction>,
    pub replicas: Vec<DeskReplica>,
    pub bindings: Vec<DeskBinding>,
    pub next_id: u64,
}

impl DeskSnapshot {
    pub fn buffer(&self, replica_id: u16) -> Result<Buffer, String> {
        let mut buffer = Buffer::new(ReplicaId::new(replica_id), BufferId::new(1).unwrap(), "");
        buffer.apply_ops(
            self.operations
                .iter()
                .map(DeskOperation::to_text)
                .collect::<Result<Vec<_>, _>>()?,
        );
        if buffer.has_deferred_ops() {
            return Err("Desk text has causally incomplete operation history".into());
        }
        Ok(buffer)
    }
    pub fn document_text(&self) -> Result<String, String> {
        Ok(self.buffer(ReplicaId::REMOTE_SERVER.as_u16())?.text())
    }
    pub fn refresh_orphans(&mut self, text: &str) {
        let live: std::collections::BTreeSet<_> = parse(text)
            .into_iter()
            .filter(|h| !h.duplicate_token)
            .filter_map(|h| h.token)
            .collect();
        for binding in &mut self.bindings {
            binding.orphaned = !live.contains(&binding.token);
        }
    }
}

fn version_to_wire(version: &Global) -> Vec<DeskClock> {
    version
        .iter()
        .filter(|c| c.value != 0)
        .map(Into::into)
        .collect()
}
fn version_from_wire(version: &[DeskClock]) -> Global {
    version.iter().copied().map(Into::into).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_is_forgiving_and_derives_parents() {
        let text = "orphan\n*bad\n* \n*** TODO deep\n:id: h-deep\nbody\n** DONE sibling\n  :id: bad token\n";
        let headings = parse(text);
        assert_eq!(headings.len(), 3);
        assert_eq!(
            (
                headings[0].depth,
                headings[0].title.as_str(),
                headings[0].parent
            ),
            (1, "", None)
        );
        assert_eq!(
            (headings[1].depth, headings[1].state, headings[1].parent),
            (3, Some(DeskHeadingState::Todo), Some(0))
        );
        assert_eq!(headings[1].token.as_ref().unwrap().0, "h-deep");
        assert_eq!(headings[2].parent, Some(0));
        assert_eq!(&text[headings[2].body_range.clone()], "  :id: bad token\n");
    }

    #[test]
    fn first_duplicate_token_owns_binding_join() {
        let headings = parse("* STAFFED first\n:id: h-7f3k\n* STAFFED copy\n:id: h-7f3k\n");
        assert!(!headings[0].duplicate_token);
        assert!(headings[1].duplicate_token);
    }

    #[test]
    fn literal_removal_and_restore_orphans_and_revives() {
        let agent_id = AgentId::from_counter(1, &rho_core::AgentIdDomain(1)).unwrap();
        let mut snapshot = DeskSnapshot {
            bindings: vec![DeskBinding {
                token: DeskIdToken("h-a".into()),
                agent_id,
                orphaned: false,
            }],
            ..Default::default()
        };
        snapshot.refresh_orphans("* TODO task\n");
        assert!(snapshot.bindings[0].orphaned);
        snapshot.refresh_orphans("* STAFFED task\n:id: h-a\n");
        assert!(!snapshot.bindings[0].orphaned);
    }
}
