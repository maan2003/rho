//! Wire vocabulary and forgiving parser for the daemon-owned Desk document.

use std::ops::Range;
use std::sync::Arc;

use clock::{Global, Lamport, ReplicaId};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use text::{Buffer, BufferId, EditOperation, FullOffset, Operation, UndoOperation};

use crate::AgentId;

#[path = "desk_temporal.rs"]
pub mod temporal;
pub use temporal::{TemporalMark, TemporalMarkKind};

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
    /// Successfully parsed dated properties, in document order. Each kind
    /// appears at most once; the first property with that key owns it.
    pub temporal_marks: Vec<TemporalMark>,
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
    /// Org-style tags from the end of the heading line (`* Title :eng-x7y2:`).
    pub tags: Vec<String>,
    /// Byte range of the whole `:a:b:` tag token, colons included.
    pub tags_range: Option<Range<usize>>,
    /// The heading line plus everything nested beneath it: body and all
    /// deeper subheadings, up to the next heading at this depth or above.
    pub subtree_range: Range<usize>,
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
    if (start < text.len() || text.is_empty() || !text.ends_with('\n'))
        && lines.last().is_none_or(|line| line.full_end != text.len())
    {
        lines.push(Line {
            start,
            end: text.len(),
            full_end: text.len(),
            text: &text[start..],
        });
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
        let (parsed_tags, content) = parse_tags(content);
        let (parsed_state, title) = parse_state(content);
        let title_start = line.text.len() - line.text[content_start..].len()
            + (title.as_ptr() as usize - content.as_ptr() as usize);
        let state = parsed_state.as_ref().map(|(state, _)| *state);
        let base = line.start + content_start;
        let state_range = parsed_state.map(|(_, range)| base + range.start..base + range.end);
        let (tags, tags_range) = parsed_tags
            .map(|(tags, range)| (tags, Some(base + range.start..base + range.end)))
            .unwrap_or_default();
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
            temporal_marks: Vec::new(),
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
            tags,
            tags_range,
            subtree_range: line.start..text.len(),
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
        let mut seen_temporal = std::collections::BTreeSet::new();
        let mut property_state = None;
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
            if let Some(kind) = TemporalMarkKind::from_property_key(key)
                && seen_temporal.insert(kind)
                && let Some(mark) = TemporalMark::parse(kind, value)
            {
                if property_state.is_none() {
                    property_state = match kind {
                        TemporalMarkKind::Done => Some(DeskHeadingState::Done),
                        TemporalMarkKind::Discarded => Some(DeskHeadingState::Discarded),
                        TemporalMarkKind::Deadline
                        | TemporalMarkKind::Todo
                        | TemporalMarkKind::Defer
                        | TemporalMarkKind::Reminder => None,
                    };
                }
                headings[index].temporal_marks.push(mark);
            }
            if key.eq_ignore_ascii_case("agent") && headings[index].agent_value.is_none() {
                headings[index].agent_value = Some(value.to_owned());
                headings[index].duplicate_agent = !seen_agents.insert(value.to_owned());
            } else if key.eq_ignore_ascii_case("project") && headings[index].project.is_none() {
                headings[index].project = Some(value.to_owned());
            }
            headings[index].properties.push(property);
        }
        if property_state.is_some() {
            headings[index].state = property_state;
        }
        headings[index].body_range = lines[line_index].full_end..end;
        // Trailing blank lines separate this subtree from what follows.
        // They fold (and move) with the subtree only when the next heading
        // is a sibling at the same depth; before a shallower heading or the
        // end of the document, the gap belongs to the enclosing context, so
        // folding a subheading keeps the space above the outer heading.
        let terminator = (index + 1..headings.len())
            .find(|later| headings[*later].depth <= headings[index].depth);
        let mut end_line = terminator.map_or(lines.len(), |later| heading_lines[later]);
        let keeps_gap =
            terminator.is_none_or(|later| headings[later].depth < headings[index].depth);
        if keeps_gap {
            while end_line > line_index + 1 && lines[end_line - 1].text.trim().is_empty() {
                end_line -= 1;
            }
        }
        let subtree_end = lines.get(end_line).map_or(text.len(), |line| line.start);
        headings[index].subtree_range = headings[index].heading_range.start..subtree_end;
        headings[index].resolved_project = headings[index].project.clone().or_else(|| {
            headings[index]
                .parent
                .and_then(|parent| headings[parent].resolved_project.clone())
        });
    }
    headings
}

/// The state keyword and its byte range within `content`, plus the
/// title. The keyword reads from the end of the line (`* Ship it DONE`);
/// the leading position (`* DONE Ship it`) is still accepted so older
/// documents and org-style habits keep parsing.
fn parse_state(content: &str) -> (Option<(DeskHeadingState, Range<usize>)>, &str) {
    const STATES: [(&str, DeskHeadingState); 4] = [
        ("TODO", DeskHeadingState::Todo),
        ("DONE", DeskHeadingState::Done),
        ("DISCARDED", DeskHeadingState::Discarded),
        ("STAFFED", DeskHeadingState::Staffed),
    ];
    for (keyword, state) in STATES {
        if let Some(rest) = content.strip_prefix(keyword)
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return (Some((state, 0..keyword.len())), rest.trim_start());
        }
    }
    for (keyword, state) in STATES {
        if let Some(rest) = content.strip_suffix(keyword)
            && (rest.is_empty() || rest.ends_with(char::is_whitespace))
        {
            return (
                Some((state, content.len() - keyword.len()..content.len())),
                rest.trim_end(),
            );
        }
    }
    (None, content.trim_start())
}

/// An org-style tag token (`:a:b:`) at the end of the heading content, plus
/// the content with the token stripped. The token must be its own word so
/// mid-title colons (`Deploy at 12:30`) stay part of the title.
type ParsedTags = Option<(Vec<String>, Range<usize>)>;

fn parse_tags(content: &str) -> (ParsedTags, &str) {
    let Some(start) = content
        .rfind(char::is_whitespace)
        .and_then(|index| Some(index + content[index..].chars().next()?.len_utf8()))
    else {
        return (None, content);
    };
    let Some(interior) = content[start..]
        .strip_prefix(':')
        .and_then(|rest| rest.strip_suffix(':'))
        .filter(|interior| {
            !interior.is_empty()
                && interior.split(':').all(|segment| {
                    !segment.is_empty()
                        && segment.chars().all(|c| {
                            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '@' | '#')
                        })
                })
        })
    else {
        return (None, content);
    };
    let tags = interior.split(':').map(str::to_owned).collect();
    (
        Some((tags, start..content.len())),
        content[..start].trim_end(),
    )
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
    #[test]
    fn state_keyword_reads_from_the_line_end() {
        let text = "* Ship it DONE\n* STAFFED Crewed\n* mark TODO list\n";
        let headings = super::parse(text);
        assert_eq!(headings[0].state, Some(super::DeskHeadingState::Done));
        assert_eq!(headings[0].title, "Ship it");
        assert_eq!(&text[headings[0].state_range.clone().unwrap()], "DONE");
        // Leading keywords keep parsing for older documents.
        assert_eq!(headings[1].state, Some(super::DeskHeadingState::Staffed));
        assert_eq!(headings[1].title, "Crewed");
        // A keyword in the middle of a title is just a word.
        assert_eq!(headings[2].state, None);
        assert_eq!(headings[2].title, "mark TODO list");
    }

    use super::*;

    #[test]
    fn tags_conceal_at_the_line_end_and_mid_title_colons_stay_words() {
        let text = "* Fix parser DONE :eng-x7y2:\n* Meet at 12:30\n* Crewed :eng-a1:eng-b2:\n* :lonely:\n* Odd :a::b:\n";
        let headings = parse(text);
        assert_eq!(headings[0].tags, vec!["eng-x7y2"]);
        assert_eq!(headings[0].state, Some(DeskHeadingState::Done));
        assert_eq!(headings[0].title, "Fix parser");
        assert_eq!(&text[headings[0].tags_range.clone().unwrap()], ":eng-x7y2:");
        assert_eq!(&text[headings[0].state_range.clone().unwrap()], "DONE");
        // A colon inside the title is not a tag.
        assert!(headings[1].tags.is_empty());
        assert_eq!(headings[1].title, "Meet at 12:30");
        // Several tags share one token.
        assert_eq!(headings[2].tags, vec!["eng-a1", "eng-b2"]);
        assert_eq!(headings[2].title, "Crewed");
        // A heading that is nothing but a token keeps it as its title.
        assert!(headings[3].tags.is_empty());
        assert_eq!(headings[3].title, ":lonely:");
        // Empty segments disqualify the whole token.
        assert!(headings[4].tags.is_empty());
        assert_eq!(headings[4].title, "Odd :a::b:");
    }

    #[test]
    fn subtree_gaps_belong_to_the_boundary_that_needs_them() {
        // Before a sibling, the blank run folds with the subtree; before a
        // shallower heading, it stays outside so the visual gap survives
        // folding. The same rule repeats at the parent's own boundary.
        let text = "* One\n** A\nbody\n\n** B\nstuff\n\n\n* Two\n";
        let headings = parse(text);
        let (one, a, b, _two) = (&headings[0], &headings[1], &headings[2], &headings[3]);
        assert_eq!(&text[a.subtree_range.clone()], "** A\nbody\n\n");
        assert_eq!(&text[b.subtree_range.clone()], "** B\nstuff\n");
        assert_eq!(
            &text[one.subtree_range.clone()],
            "* One\n** A\nbody\n\n** B\nstuff\n\n\n"
        );
        // At the end of the document the gap also stays out.
        let headings = parse("* Last\nbody\n\n\n");
        assert_eq!(headings[0].subtree_range, 0.."* Last\nbody\n".len());
    }

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
            "* TODO root\n:project: /src/root\n:unknown: kept\n** first\n:agent: eng-{}\n*** inherited\n** copy\n:agent: eng-{}\n:agent: ignored\n:project: child\n",
            agent.encoded(),
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

    #[test]
    fn temporal_properties_are_typed_case_insensitive_and_first_wins() {
        use chrono::{NaiveDate, NaiveTime};

        let text = "* legacy TODO\n:ToDo: 2026-08-23 12:30 9d\n:todo: 2026-09-01\n:DEADLINE: not-a-date\n:deadline: 2026-08-30\n:reminder: 2026-08-30\n";
        let headings = parse(text);
        let heading = &headings[0];
        assert_eq!(heading.state, Some(DeskHeadingState::Todo));
        assert_eq!(heading.temporal_marks.len(), 2);
        assert_eq!(heading.temporal_marks[0].kind, TemporalMarkKind::Todo);
        assert_eq!(heading.temporal_marks[0].pace_days, 9);
        assert_eq!(
            heading.temporal_marks[0].at,
            NaiveDate::from_ymd_opt(2026, 8, 23)
                .unwrap()
                .and_time(NaiveTime::from_hms_opt(12, 30, 0).unwrap())
        );
        assert_eq!(heading.temporal_marks[1].kind, TemporalMarkKind::Reminder);
        assert_eq!(heading.temporal_marks[1].pace_days, 1);
        assert_eq!(
            heading.properties.len(),
            5,
            "all source properties remain visible data"
        );
    }

    #[test]
    fn terminal_properties_override_legacy_keywords_in_document_order() {
        let headings = parse(
            "* old DONE\n:discarded: 2026-08-24\n:done: 2026-08-25\n* old DISCARDED\n:done: 2026-08-26\n",
        );
        assert_eq!(headings[0].state, Some(DeskHeadingState::Discarded));
        assert_eq!(headings[1].state, Some(DeskHeadingState::Done));
        assert_eq!(headings[0].title, "old");
        assert!(
            headings[0].state_range.is_some(),
            "legacy source range remains addressable"
        );
    }
}
