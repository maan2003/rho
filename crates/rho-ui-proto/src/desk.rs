//! Wire vocabulary and forgiving parser for the daemon-owned Desk document.

use std::ops::Range;
use std::sync::Arc;

use clock::{Global, Lamport, ReplicaId};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use text::{Buffer, BufferId, EditOperation, FullOffset, Operation, UndoOperation};

use crate::AgentId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DeskHeadingState {
    Todo,
    Done,
    Discarded,
    Staffed,
}

impl DeskHeadingState {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Todo => "TODO",
            Self::Done => "DONE",
            Self::Discarded => "DISCARDED",
            Self::Staffed => "STAFFED",
        }
    }
}

/// A property line belonging to a heading. Ranges exclude the trailing newline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeskProperty {
    pub key: String,
    pub value: String,
    pub line_range: Range<usize>,
    pub value_range: Range<usize>,
}

/// A heading derived from Desk text. All ranges are UTF-8 byte ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeskHeading {
    pub depth: usize,
    pub state: Option<DeskHeadingState>,
    pub title: String,
    /// The first `:agent:` value under this heading, whether or not it parses.
    pub agent_value: Option<String>,
    pub duplicate_agent: bool,
    /// The first direct `:project:` value and its inherited resolution.
    pub project: Option<String>,
    pub resolved_project: Option<String>,
    pub properties: Vec<DeskProperty>,
    pub parent: Option<usize>,
    pub heading_range: Range<usize>,
    pub stars_range: Range<usize>,
    pub state_range: Option<Range<usize>>,
    pub title_range: Range<usize>,
    pub body_range: Range<usize>,
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
        let content_start = depth + 1;
        let content = line.text[content_start..].trim_end();
        let (state, title) = parse_state(content);
        let title_start = line.text.len() - line.text[content_start..].len()
            + (title.as_ptr() as usize - content.as_ptr() as usize);
        let state_range = state.map(|state| {
            let start = line.start + content_start;
            start..start + state.keyword().len()
        });
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
            agent_value: None,
            duplicate_agent: false,
            project: None,
            resolved_project: None,
            properties: Vec::new(),
            parent,
            heading_range: line.start..line.end,
            stars_range: line.start..line.start + depth,
            state_range,
            title_range: line.start + title_start..line.start + title_start + title.len(),
            body_range: line.full_end..text.len(),
        });
        heading_lines.push(line_index);
        stack.push((depth, index));
    }

    let mut seen_agents = std::collections::BTreeSet::new();
    for index in 0..headings.len() {
        let line_index = heading_lines[index];
        let end = heading_lines
            .get(index + 1)
            .map_or(text.len(), |next| lines[*next].start);
        for line in lines
            .iter()
            .skip(line_index + 1)
            .take_while(|line| line.start < end)
        {
            let Some((key, value, value_start)) = parse_property_line(line.text) else {
                continue;
            };
            let property = DeskProperty {
                key: key.to_owned(),
                value: value.to_owned(),
                line_range: line.start..line.end,
                value_range: line.start + value_start..line.start + value_start + value.len(),
            };
            if key.eq_ignore_ascii_case("agent") && headings[index].agent_value.is_none() {
                headings[index].agent_value = Some(value.to_owned());
                headings[index].duplicate_agent = !seen_agents.insert(value.to_owned());
            } else if key.eq_ignore_ascii_case("project") && headings[index].project.is_none() {
                headings[index].project = Some(value.to_owned());
            }
            headings[index].properties.push(property);
        }
        headings[index].body_range = lines[line_index].full_end..end;
        headings[index].resolved_project = headings[index].project.clone().or_else(|| {
            headings[index]
                .parent
                .and_then(|parent| headings[parent].resolved_project.clone())
        });
    }
    headings
}

fn parse_state(content: &str) -> (Option<DeskHeadingState>, &str) {
    for (keyword, state) in [
        ("TODO", DeskHeadingState::Todo),
        ("DONE", DeskHeadingState::Done),
        ("DISCARDED", DeskHeadingState::Discarded),
        ("STAFFED", DeskHeadingState::Staffed),
    ] {
        if let Some(rest) = content.strip_prefix(keyword)
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return (Some(state), rest.trim_start());
        }
    }
    (None, content.trim_start())
}

fn parse_property_line(line: &str) -> Option<(&str, &str, usize)> {
    let leading = line.len() - line.trim_start().len();
    let rest = &line[leading..];
    let rest = rest.strip_prefix(':')?;
    let colon = rest.find(':')?;
    let key = &rest[..colon];
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    let raw_value = &rest[colon + 1..];
    let value = raw_value.trim();
    let value_start = leading + 1 + colon + 1 + (raw_value.len() - raw_value.trim_start().len());
    Some((key, value, value_start))
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

/// A CRDT position in the Desk text. Buffer ids differ per replica, so the
/// wire form carries only the insertion timestamp; each side reattaches its
/// own buffer id on conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub struct DeskAnchor {
    pub timestamp: DeskClock,
    pub offset: u32,
    pub right_bias: bool,
}

impl DeskAnchor {
    pub fn from_text(anchor: text::Anchor) -> Self {
        Self {
            timestamp: anchor.timestamp().into(),
            offset: anchor.offset,
            right_bias: anchor.bias == text::Bias::Right,
        }
    }

    pub fn to_text(&self, buffer_id: BufferId) -> text::Anchor {
        text::Anchor::new(
            self.timestamp.into(),
            self.offset,
            if self.right_bias {
                text::Bias::Right
            } else {
                text::Bias::Left
            },
            buffer_id,
        )
    }
}

/// An agent attached to the Desk document at an anchored position. The
/// heading containing the anchor is the agent's topic; the text itself
/// carries no binding markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskBinding {
    pub agent_id: AgentId,
    pub anchor: DeskAnchor,
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
        let text = "orphan\n*bad\n* \n*** TODO deep\n:owner: anyone\nbody\n** DONE sibling\n  :broken property\n";
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
        assert_eq!(headings[2].parent, Some(0));
        assert_eq!(&headings[1].properties[0].key, "owner");
        assert_eq!(
            &text[headings[2].body_range.clone()],
            "  :broken property\n"
        );
    }

    #[test]
    fn parses_properties_bindings_duplicates_and_project_inheritance() {
        let agent = AgentId::from_counter(1, &rho_core::AgentIdDomain(1)).unwrap();
        let text = format!(
            "* TODO root\n:project: /src/root\n:unknown: kept\n** first\n:agent: {}\n*** inherited\n** copy\n:agent: eng-{}\n:agent: ignored\n:project: child\n",
            format!("eng-{}", agent.encoded()),
            agent.encoded()
        );
        let headings = parse(&text);
        assert_eq!(headings[0].project.as_deref(), Some("/src/root"));
        assert_eq!(headings[0].properties[1].key, "unknown");
        assert_eq!(
            headings[1].agent_value.as_deref(),
            Some(&format!("eng-{}", agent.encoded())[..])
        );
        assert!(!headings[1].duplicate_agent);
        assert_eq!(headings[2].resolved_project.as_deref(), Some("/src/root"));
        assert_eq!(
            headings[3].agent_value.as_deref(),
            Some(&format!("eng-{}", agent.encoded())[..])
        );
        assert!(headings[3].duplicate_agent);
        assert_eq!(headings[3].resolved_project.as_deref(), Some("child"));
        assert_eq!(
            &text[headings[3].properties[0].line_range.clone()],
            format!(":agent: eng-{}", agent.encoded())
        );
    }

    #[test]
    fn first_agent_property_under_a_heading_wins_even_when_unknown() {
        let agent = AgentId::from_counter(2, &rho_core::AgentIdDomain(1)).unwrap();
        let headings = parse(&format!(
            "* task\n:agent: not-an-agent\n:agent: {}\n",
            agent.encoded()
        ));
        assert_eq!(headings[0].agent_value.as_deref(), Some("not-an-agent"));
    }
}
