//! Temporary (`temp-migration`): moves a store to the log where a request's row
//! holds what it carried. Remove it, with `PREVIOUS_AGENT_DB_FORMAT`, once the
//! store has moved.
//!
//! - A request takes the `Input` rows of its agent since its last request.
//! - Its notebook output becomes its `output`.
//! - Claude's output rows go: its transcript said the same.
//! - Claude's echoes keep only their uuid, which a rewind forks at.
//! - A Claude result that repeats what its request answered with goes. The rest
//!   stay as `ClaudeResults`: Claude Code's own tools, and refused calls.
//! - The runtimes' rows flatten into the log's own, and nothing keeps `why`,
//!   `wake` or a `Restarted` notice, which no code read.
//! - An input no request came after goes: the chat hands a message again, since
//!   the agent's watermark falls back with it.
//!
//! Rows merge, so positions change and the `Rewound` rows follow. A rewind can
//! name an input inside a request's group. What it left standing goes to the
//! agent's next request, which is what the old log read as.

use std::collections::{HashMap, HashSet};

use redb::TableDefinition;
use rho_agent_types::{AgentId, Role, WorksetId, Wpos};
use rho_db::{Sen, SenValue, WriteTxn};

use super::*;
use crate::entry::{Notice, output_content, plain_text};
use crate::record::Input;

/// The log as the previous format wrote it.
mod old {
    use rho_agent_types::transcript::{ExecCall, ExecId, PendingInferenceResponse, ToolResult};
    use rho_agent_types::{
        AgentId, ContentPart, ExecMilestone, MessageId, UnixMs, WorksetKind, Wpos,
    };
    use senax_encoder::{Decode, Encode};
    use uuid::Uuid;

    use crate::entry::{RequestNotice, ResponseUsage};
    use crate::inference::Carry;
    use crate::log::{AgentUsageBucket, ClaudeRewind, TranscriptCall};
    use crate::record::AgentSpec;

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct Record {
        pub at: UnixMs,
        pub event: Event,
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub enum Event {
        WorksetCreated {
            kind: WorksetKind,
            code: String,
        },
        Evaluated {
            code: String,
        },
        AgentCreated {
            agent: AgentId,
            spec: AgentSpec,
        },
        AgentChanged {
            agent: AgentId,
            spec: AgentSpec,
        },
        Input {
            agent: AgentId,
            message: Option<MessageId>,
            content: Vec<ContentPart>,
            wake: InputWake,
        },
        Rewound {
            agent: AgentId,
            to: Wpos,
        },
        Failed {
            agent: AgentId,
            partial: PendingInferenceResponse,
            error: String,
            retrying: bool,
        },
        ExecObserved {
            agent: AgentId,
            exec: ExecId,
            milestone: ExecMilestone,
        },
        Native {
            agent: AgentId,
            entry: Entry,
        },
        Claude {
            agent: AgentId,
            row: ClaudeRow,
        },
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
    pub enum InputWake {
        Person,
        Agent,
        Quiet,
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub enum Entry {
        Step {
            exec: Option<String>,
            prose: String,
            carry: Carry,
            usage: Option<ResponseUsage>,
        },
        RequestSent {
            report: Report,
            compact: bool,
        },
        Notice {
            notice: Notice,
        },
        CompactionTrigger {
            manual: bool,
        },
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub enum Notice {
        Error(String),
        Stopped(String),
        Restarted,
        Archived,
        FreshNotebook,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
    pub struct Report {
        pub notices: Vec<RequestNotice>,
        pub notebook: rho_notebook::Report,
    }

    /// A decode skips the fields this hop drops: `wake` and `why` here and on
    /// the native request, all of `Output` and `OutputHandedOff`, and an
    /// echo's text.
    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub enum TranscriptLine {
        User {},
        Assistant {
            text: String,
            calls: Vec<TranscriptCall>,
            usage: Option<AgentUsageBucket>,
            context_used: Option<u64>,
        },
        ToolResults {
            results: Vec<ToolResult>,
        },
        Compacted {
            context_used: Option<u64>,
        },
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub enum ClaudeRow {
        Transcript { uuid: Uuid, line: TranscriptLine },
        ExecAdmitted { call: ExecCall },
        Output {},
        OutputHandedOff {},
        RewindPending(Option<ClaudeRewind>),
        RequestSent { report: Report },
        Notice(Notice),
    }

    impl Event {
        pub fn agent(&self) -> Option<AgentId> {
            match self {
                Self::WorksetCreated { .. } | Self::Evaluated { .. } => None,
                Self::AgentCreated { agent, .. }
                | Self::AgentChanged { agent, .. }
                | Self::Input { agent, .. }
                | Self::Rewound { agent, .. }
                | Self::Failed { agent, .. }
                | Self::ExecObserved { agent, .. }
                | Self::Native { agent, .. }
                | Self::Claude { agent, .. } => Some(*agent),
            }
        }
    }
}

/// The same table as `WORKSET_LOG`, as bytes the old types decode.
const OLD_LOG: TableDefinition<(WorksetKey, u64), Raw> = TableDefinition::new(WORKSET_LOG_NAME);

/// A row's bytes, under the type name of the log's values.
#[derive(Debug)]
struct Raw;

impl redb::Value for Raw {
    type SelfType<'a> = &'a [u8];
    type AsBytes<'a> = &'a [u8];

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> &'a [u8]
    where
        Self: 'a,
    {
        data
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a &'b [u8]) -> &'a [u8]
    where
        Self: 'b,
    {
        value
    }

    fn type_name() -> redb::TypeName {
        <Sen<Record<'static>> as redb::Value>::type_name()
    }
}

pub(crate) fn migrate(write: &mut WriteTxn) {
    let worksets: Vec<(WorksetId, WorksetHead)> = write
        .open_table(WORKSET_HEADS)
        .iter()
        .map(|(key, head)| (WorksetId(key.value()), head.value().into_owned()))
        .collect();
    for (workset, head) in worksets {
        let rows: Vec<old::Record> = write
            .open_table(OLD_LOG)
            .range((workset.0, 0)..=(workset.0, u64::MAX))
            .map(|(_, record)| {
                let mut bytes = record.value();
                senax_encoder::decode(&mut bytes).expect("a row of the previous format")
            })
            .collect();
        let specs: Vec<(AgentId, AgentSpec)> = head
            .agents
            .iter()
            .filter_map(|agent| Some((*agent, write.agent_head(*agent)?.spec)))
            .collect();
        let rows = rewrite(&rows);
        write.delete_workset(workset);
        write.open_table(WORKSET_HEADS).insert(
            workset.0,
            SenValue::borrowed(&WorksetHead {
                kind: head.kind,
                created_at: head.created_at,
                next: Wpos::ZERO,
                agents: Vec::new(),
            }),
        );
        for record in &rows {
            write
                .append(workset, record)
                .expect("a rewritten log appends as the old one did");
        }
        for (agent, spec) in specs {
            assert!(
                write
                    .agent_head(agent)
                    .is_some_and(|head| head.spec == spec),
                "agent {} folds to its old spec",
                agent.encoded()
            );
        }
    }
}

/// An agent's place in the rewrite.
#[derive(Default)]
struct Agent {
    /// Old indexes of the inputs its next request takes.
    waiting: Vec<usize>,
    /// Its requests: where each went, and the old indexes of its inputs.
    requests: Vec<(Wpos, Vec<usize>)>,
    /// Its newest new row.
    last: Option<Wpos>,
    /// Calls Rho ran for it.
    admitted: HashSet<String>,
    /// What its newest request's output said.
    output: String,
}

fn new_notice(notice: old::Notice) -> Option<Notice> {
    match notice {
        old::Notice::Error(error) => Some(Notice::Error(error)),
        old::Notice::Stopped(error) => Some(Notice::Stopped(error)),
        old::Notice::Restarted => None,
        old::Notice::Archived => Some(Notice::Archived),
        old::Notice::FreshNotebook => Some(Notice::FreshNotebook),
    }
}

/// A request as the new log holds it: it takes what waits for its agent.
fn request(
    agents: &mut HashMap<AgentId, Agent>,
    agent: AgentId,
    report: old::Report,
    compact: bool,
    at: usize,
    input: &impl Fn(usize) -> Input,
) -> Event<'static> {
    let place = agents.entry(agent).or_default();
    let taken = std::mem::take(&mut place.waiting);
    let inputs = taken.iter().map(|index| input(*index)).collect();
    place.requests.push((Wpos(at as u64), taken));
    let output = output_content(&report.notebook);
    place.output = plain_text(&output);
    Event::Request {
        agent,
        output,
        inputs,
        notices: report.notices,
        compact,
    }
}

/// The new rows of one workset's log, in order.
fn rewrite(rows: &[old::Record]) -> Vec<Record<'static>> {
    // Each old position's next new row.
    let mut moved: Vec<Wpos> = Vec::with_capacity(rows.len());
    let mut agents: HashMap<AgentId, Agent> = HashMap::new();
    let mut new: Vec<Record<'static>> = Vec::with_capacity(rows.len());
    let input = |index: usize| -> Input {
        let old::Event::Input {
            message,
            content,
            wake,
            ..
        } = &rows[index].event
        else {
            unreachable!("only inputs wait");
        };
        Input {
            role: match wake {
                old::InputWake::Person => Role::User,
                old::InputWake::Agent => Role::Tool,
                old::InputWake::Quiet => Role::System,
            },
            message: *message,
            content: content.clone(),
        }
    };
    for (index, record) in rows.iter().enumerate() {
        moved.push(Wpos(new.len() as u64));
        let at = record.at;
        let agent = record.event.agent();
        let event = match record.event.clone() {
            old::Event::Input { agent, .. } => {
                agents.entry(agent).or_default().waiting.push(index);
                continue;
            }
            old::Event::Claude {
                row: old::ClaudeRow::Output {} | old::ClaudeRow::OutputHandedOff {},
                ..
            } => continue,
            old::Event::Rewound { agent, to } => {
                let place = agents.entry(agent).or_default();
                place.waiting.retain(|waiting| *waiting < to.0 as usize);
                let to_new = moved[to.0 as usize];
                // Inputs before `to` stand, though their request went back.
                let mut standing = Vec::new();
                while place.requests.last().is_some_and(|(at, _)| *at >= to_new) {
                    let (_, inputs) = place.requests.pop().expect("just looked");
                    let mut kept: Vec<usize> = inputs
                        .into_iter()
                        .filter(|input| *input < to.0 as usize)
                        .collect();
                    kept.append(&mut standing);
                    standing = kept;
                }
                standing.append(&mut place.waiting);
                place.waiting = standing;
                // What it took back may all be gone: then it has no work left.
                if place.last.is_none_or(|last| to_new > last) {
                    continue;
                }
                Event::Rewound { agent, to: to_new }
            }
            old::Event::Native {
                agent,
                entry: old::Entry::RequestSent { report, compact },
            } => request(&mut agents, agent, report, compact, new.len(), &input),
            old::Event::Claude {
                agent,
                row: old::ClaudeRow::RequestSent { report },
            } => request(&mut agents, agent, report, false, new.len(), &input),
            old::Event::WorksetCreated { kind, code } => Event::WorksetCreated { kind, code },
            old::Event::Evaluated { code } => Event::Evaluated { code },
            old::Event::AgentCreated { agent, spec } => Event::AgentCreated { agent, spec },
            old::Event::AgentChanged { agent, spec } => Event::AgentChanged { agent, spec },
            old::Event::Failed {
                agent,
                partial,
                error,
                retrying,
            } => Event::Failed {
                agent,
                partial,
                error: error.into(),
                retrying,
            },
            old::Event::ExecObserved {
                agent,
                exec,
                milestone,
            } => Event::ExecObserved {
                agent,
                exec,
                milestone,
            },
            old::Event::Native { agent, entry } => match entry {
                old::Entry::Step {
                    exec,
                    prose,
                    carry,
                    usage,
                } => Event::Step {
                    agent,
                    exec,
                    prose,
                    carry,
                    usage,
                },
                old::Entry::Notice { notice } => {
                    let Some(notice) = new_notice(notice) else {
                        continue;
                    };
                    Event::Notice { agent, notice }
                }
                old::Entry::CompactionTrigger { manual } => {
                    Event::CompactionTrigger { agent, manual }
                }
                old::Entry::RequestSent { .. } => unreachable!("matched above"),
            },
            old::Event::Claude { agent, row } => match row {
                old::ClaudeRow::Transcript { uuid, line } => match line {
                    old::TranscriptLine::User {} => Event::ClaudeEcho { agent, uuid },
                    old::TranscriptLine::Assistant {
                        text,
                        calls,
                        usage,
                        context_used,
                    } => Event::ClaudeReply {
                        agent,
                        text,
                        calls,
                        usage,
                        context_used,
                    },
                    old::TranscriptLine::ToolResults { mut results } => {
                        let place = agents.entry(agent).or_default();
                        results.retain(|result| {
                            !place.admitted.contains(result.call_id.as_str())
                                || result.body.recorded_output().trim() != place.output.trim()
                        });
                        if results.is_empty() {
                            continue;
                        }
                        Event::ClaudeResults { agent, results }
                    }
                    old::TranscriptLine::Compacted { context_used } => Event::ClaudeCompacted {
                        agent,
                        context_used,
                    },
                },
                old::ClaudeRow::ExecAdmitted { call } => {
                    agents
                        .entry(agent)
                        .or_default()
                        .admitted
                        .insert(call.id.as_str().to_owned());
                    Event::ExecAdmitted { agent, call }
                }
                old::ClaudeRow::RewindPending(rewind) => Event::RewindPending { agent, rewind },
                old::ClaudeRow::Notice(notice) => {
                    let Some(notice) = new_notice(notice) else {
                        continue;
                    };
                    Event::Notice { agent, notice }
                }
                old::ClaudeRow::Output {}
                | old::ClaudeRow::OutputHandedOff {}
                | old::ClaudeRow::RequestSent { .. } => unreachable!("matched above"),
            },
        };
        if let Some(agent) = agent {
            agents.entry(agent).or_default().last = Some(Wpos(new.len() as u64));
        }
        new.push(Record { at, event });
    }
    new
}

#[cfg(test)]
pub(crate) mod tests {
    use rho_agent_types::{AgentIdDomain, ChatSeq, ContentPart, MessageId, UnixMs, WorksetKind};

    use super::*;
    use crate::entry::RequestNotice;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &AgentIdDomain(7)).unwrap()
    }
    fn at(event: old::Event) -> old::Record {
        old::Record {
            at: UnixMs(9),
            event,
        }
    }
    fn text(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: text.into() }]
    }
    pub(crate) fn old_input(agent: AgentId, said: &str, message: Option<u64>) -> old::Record {
        at(old::Event::Input {
            agent,
            message: message.map(|seq| MessageId(ChatSeq(seq))),
            content: text(said),
            wake: old::InputWake::Person,
        })
    }
    pub(crate) fn old_request(agent: AgentId, output: &str) -> old::Record {
        at(old::Event::Native {
            agent,
            entry: old::Entry::RequestSent {
                compact: false,
                report: old::Report {
                    notices: vec![RequestNotice::Restarted],
                    notebook: rho_notebook::Report::from_text(output.into(), vec![]),
                },
            },
        })
    }
    fn old_claude_request(agent: AgentId, output: &str) -> old::Record {
        at(old::Event::Claude {
            agent,
            row: old::ClaudeRow::RequestSent {
                report: old::Report {
                    notices: Vec::new(),
                    notebook: rho_notebook::Report::from_text(output.into(), vec![]),
                },
            },
        })
    }
    fn old_notice(agent: AgentId) -> old::Record {
        at(old::Event::Native {
            agent,
            entry: old::Entry::Notice {
                notice: old::Notice::Archived,
            },
        })
    }
    fn old_rewind(agent: AgentId, to: u64) -> old::Record {
        at(old::Event::Rewound {
            agent,
            to: Wpos(to),
        })
    }

    /// Rows of the old shape, written where the next rows go.
    pub(crate) fn write_old(write: &mut WriteTxn, workset: WorksetId, rows: &[old::Record]) {
        let mut head = write
            .open_table(WORKSET_HEADS)
            .get(workset.0)
            .unwrap()
            .value()
            .into_owned();
        for record in rows {
            let bytes = senax_encoder::encode(record).unwrap();
            write
                .open_table(OLD_LOG)
                .insert((workset.0, head.next.0), &bytes[..]);
            head.next = head.next.next();
        }
        write
            .open_table(WORKSET_HEADS)
            .insert(workset.0, SenValue::borrowed(&head));
    }

    fn said(inputs: &[Input]) -> Vec<String> {
        inputs
            .iter()
            .map(|input| rho_agent_types::transcript::text_content(&input.content))
            .collect()
    }

    /// Each request's inputs, as `rewrite` left them, with rewound ones gone.
    fn visible_requests(rows: &[Record<'_>], of: AgentId) -> Vec<Vec<String>> {
        let mut seen: Vec<(u64, Vec<String>)> = Vec::new();
        for (pos, record) in rows.iter().enumerate() {
            match &record.event {
                Event::Rewound { agent, to } if *agent == of => {
                    seen.retain(|(at, _)| *at < to.0);
                }
                Event::Request { agent, inputs, .. } if *agent == of => {
                    seen.push((pos as u64, said(inputs)));
                }
                _ => {}
            }
        }
        seen.into_iter().map(|(_, said)| said).collect()
    }

    #[test]
    fn a_request_takes_its_inputs_and_its_output() {
        let (a, b) = (agent(1), agent(2));
        let new = rewrite(&[
            old_input(a, "first", Some(3)),
            old_input(b, "for b", Some(4)),
            old_input(a, "second", None),
            old_request(a, "printed"),
            old_request(a, ""),
        ]);
        // Only `a`'s two requests stay: nothing came after `b`'s input.
        assert_eq!(new.len(), 2, "{new:?}");
        let Event::Request {
            agent: carrier,
            output,
            inputs,
            notices,
            ..
        } = &new[0].event
        else {
            panic!("{:?}", new[0]);
        };
        assert_eq!(*carrier, a);
        assert_eq!(output, &text("printed"));
        assert_eq!(said(inputs), ["first", "second"]);
        assert_eq!(inputs[0].message, Some(MessageId(ChatSeq(3))));
        assert_eq!(inputs[0].role, Role::User);
        assert_eq!(notices, &[RequestNotice::Restarted]);
        assert!(
            matches!(&new[1].event, Event::Request { output, inputs, .. }
            if output.is_empty() && inputs.is_empty())
        );
    }

    fn old_line(agent: AgentId, line: old::TranscriptLine) -> old::Record {
        at(old::Event::Claude {
            agent,
            row: old::ClaudeRow::Transcript {
                uuid: uuid::Uuid::from_u128(11),
                line,
            },
        })
    }
    fn old_admitted(agent: AgentId, id: &str) -> old::Record {
        at(old::Event::Claude {
            agent,
            row: old::ClaudeRow::ExecAdmitted {
                call: rho_agent_types::transcript::ExecCall {
                    id: id.try_into().unwrap(),
                    source: "pass".into(),
                },
            },
        })
    }
    fn old_result(id: &str, output: &str) -> rho_agent_types::transcript::ToolResult {
        rho_agent_types::transcript::ToolResult {
            call_id: id.try_into().unwrap(),
            tool_type: rho_agent_types::transcript::ToolType::Function,
            body: rho_agent_types::transcript::ToolOutput {
                full_output: None,
                images: Default::default(),
                output: std::sync::Arc::new(output.to_owned()),
                status: rho_agent_types::ToolOutputStatus::Success,
            },
            started_at: UnixMs(1),
            finished_at: UnixMs(2),
        }
    }
    fn result_ids(event: &Event<'_>) -> Vec<String> {
        let Event::ClaudeResults { results, .. } = event else {
            panic!("{event:?}");
        };
        results
            .iter()
            .map(|result| result.call_id.as_str().to_owned())
            .collect()
    }

    /// Claude's output rows go, and its request holds the output.
    #[test]
    fn a_claude_request_holds_its_output() {
        let a = agent(1);
        let new = rewrite(&[
            at(old::Event::Claude {
                agent: a,
                row: old::ClaudeRow::Output {},
            }),
            at(old::Event::Claude {
                agent: a,
                row: old::ClaudeRow::OutputHandedOff {},
            }),
            old_claude_request(a, "printed"),
        ]);
        assert_eq!(new.len(), 1);
        assert!(
            matches!(&new[0].event, Event::Request { output, compact: false, .. }
            if output == &text("printed"))
        );
    }

    /// A result goes only when it says what its request answered with, and
    /// Rho ran its call. Otherwise it is the only copy, and it stays.
    #[test]
    fn a_claude_result_goes_only_when_its_request_holds_it() {
        let a = agent(1);
        let other = agent(2);
        let results = |results| old::TranscriptLine::ToolResults { results };
        let new = rewrite(&[
            old_admitted(a, "ran"),
            old_claude_request(a, "printed"),
            // Its copy is in the request. Whitespace is Claude Code's.
            old_line(a, results(vec![old_result("ran", "printed\n")])),
            // Same text, but Claude Code's own tool: the text is a coincidence.
            old_line(a, results(vec![old_result("bash", "printed")])),
            // Rho ran it, and the request said something else.
            old_admitted(a, "differs"),
            old_line(a, results(vec![old_result("differs", "other")])),
            // Another agent's request says nothing of this one's result.
            old_admitted(other, "theirs"),
            old_claude_request(other, "not this"),
            old_line(a, results(vec![old_result("ran", "printed")])),
            // One line holds both kinds: only the copy goes.
            old_line(
                a,
                results(vec![
                    old_result("ran", "printed"),
                    old_result("refused", "only the first exec"),
                ]),
            ),
        ]);
        let ids: Vec<Vec<String>> = new
            .iter()
            .filter(|record| matches!(record.event, Event::ClaudeResults { .. }))
            .map(|record| result_ids(&record.event))
            .collect();
        assert_eq!(ids, [["bash"], ["differs"], ["refused"]]);
    }

    /// An echo keeps its uuid, which a rewind forks at. A reply keeps its
    /// words. A `Restarted` notice goes, and a rewind that named it follows.
    #[test]
    fn claude_rows_flatten() {
        let a = agent(1);
        let new = rewrite(&[
            old_line(a, old::TranscriptLine::User {}),
            at(old::Event::Claude {
                agent: a,
                row: old::ClaudeRow::Notice(old::Notice::Restarted),
            }),
            old_line(
                a,
                old::TranscriptLine::Assistant {
                    text: "said".into(),
                    calls: Vec::new(),
                    usage: None,
                    context_used: Some(4),
                },
            ),
            old_rewind(a, 1),
        ]);
        assert_eq!(new.len(), 3, "{new:?}");
        assert!(
            matches!(new[0].event, Event::ClaudeEcho { uuid, .. } if uuid == uuid::Uuid::from_u128(11))
        );
        assert!(
            matches!(&new[1].event, Event::ClaudeReply { text, context_used: Some(4), .. } if text == "said")
        );
        assert!(matches!(new[2].event, Event::Rewound { to: Wpos(1), .. }));
    }

    /// The old rows carried fields this module does not name: a decode skips
    /// them, whatever they hold.
    #[test]
    fn output_rows_decode_without_their_fields() {
        use senax_encoder::{Encode, decode, encode};
        #[derive(Encode)]
        enum ClaudeRow {
            Output {
                batch: Vec<String>,
            },
            OutputHandedOff {
                id: uuid::Uuid,
            },
            RequestSent {
                why: crate::entry::RequestNotice,
                report: old::Report,
            },
        }
        let bytes = encode(&ClaudeRow::RequestSent {
            why: RequestNotice::Checkin,
            report: old::Report::default(),
        })
        .unwrap();
        let row: old::ClaudeRow = decode(&mut bytes.as_ref()).unwrap();
        assert_eq!(
            row,
            old::ClaudeRow::RequestSent {
                report: old::Report::default()
            }
        );
        let bytes = encode(&ClaudeRow::Output {
            batch: vec!["x".into()],
        })
        .unwrap();
        let row: old::ClaudeRow = decode(&mut bytes.as_ref()).unwrap();
        assert_eq!(row, old::ClaudeRow::Output {});
        let bytes = encode(&ClaudeRow::OutputHandedOff {
            id: uuid::Uuid::from_u128(1),
        })
        .unwrap();
        let row: old::ClaudeRow = decode(&mut bytes.as_ref()).unwrap();
        assert_eq!(row, old::ClaudeRow::OutputHandedOff {});
    }

    /// A rewind that names a person's input takes back that input, and its
    /// request. An input before it in the group stands, as it did in the old
    /// log, and goes with the agent's next request.
    #[test]
    fn a_rewind_inside_a_group_leaves_what_came_before_it() {
        let a = agent(1);
        let new = rewrite(&[
            old_input(a, "peer", None),   // 0
            old_input(a, "person", None), // 1
            old_request(a, ""),           // 2
            old_rewind(a, 1),             // 3
            old_input(a, "after", None),  // 4
            old_request(a, ""),           // 5
        ]);
        assert_eq!(new.len(), 3, "{new:?}");
        assert!(matches!(new[1].event, Event::Rewound { to: Wpos(0), .. }));
        assert_eq!(visible_requests(&new, a), [["peer", "after"]]);
    }

    /// A rewind keeps naming the row it named, past rows that merged and went.
    /// One that names nothing left goes too, and an unanswered input it took
    /// back does not come back.
    #[test]
    fn rewinds_follow_their_rows() {
        let a = agent(1);
        let b = agent(2);
        let new = rewrite(&[
            old_input(a, "one", None),  // 0 -> into 0
            old_request(a, ""),         // 1 -> 0
            old_notice(b),              // 2 -> 1
            old_notice(a),              // 3 -> 2
            old_input(a, "two", None),  // 4 -> into 3
            old_request(a, ""),         // 5 -> 3
            old_notice(a),              // 6 -> 4
            old_rewind(a, 4),           // 7 -> 5, to 3
            old_input(a, "gone", None), // 8
            old_rewind(a, 8),           // 9 -> nothing left to take back
            old_request(a, ""),         // 10 -> 6
        ]);
        assert_eq!(new.len(), 7, "{new:?}");
        assert!(
            matches!(new[5].event, Event::Rewound { to: Wpos(3), .. }),
            "{:?}",
            new[5]
        );
        assert_eq!(
            visible_requests(&new, a),
            [vec!["one".to_owned()], Vec::new()]
        );
    }

    /// Against the old log's own reading, over many interleaved histories: an
    /// agent's standing rows are the same. The reading here shares no code
    /// with `rewrite`: it hides by old position, then groups what stands.
    #[test]
    fn every_agent_reads_as_it_did() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let agents = [agent(1), agent(2)];
        for _ in 0..200 {
            let mut rows = Vec::new();
            let mut theirs: [Vec<u64>; 2] = [Vec::new(), Vec::new()];
            let length = 1 + next(40);
            for n in 0..length {
                let which = next(2) as usize;
                let a = agents[which];
                let pos = rows.len() as u64;
                let record = match next(7) {
                    0..=2 => old_input(a, &n.to_string(), None),
                    3 | 4 => old_request(a, ""),
                    5 => old_notice(a),
                    _ if !theirs[which].is_empty() => {
                        let to = theirs[which][next(theirs[which].len() as u64) as usize];
                        old_rewind(a, to)
                    }
                    _ => old_notice(a),
                };
                theirs[which].push(pos);
                rows.push(record);
            }
            let new = rewrite(&rows);
            for a in agents {
                assert_eq!(
                    standing(&new, a),
                    old_standing(&rows, a),
                    "{a:?} in {rows:#?} became {new:#?}"
                );
            }
            // A rewritten log appends: every rewind names a row of its agent.
            for (pos, record) in new.iter().enumerate() {
                if let Event::Rewound { agent, to } = &record.event {
                    assert!(to.0 <= pos as u64);
                    assert!(
                        new[..pos]
                            .iter()
                            .enumerate()
                            .any(|(at, row)| at as u64 >= to.0
                                && row.event.agent() == Some(*agent)),
                        "{record:?} in {new:#?}"
                    );
                }
            }
        }
    }

    /// An agent's standing rows in the old log: requests with what they
    /// delivered, and notices.
    fn old_standing(rows: &[old::Record], of: AgentId) -> Vec<String> {
        let mut seen: Vec<(u64, &old::Record)> = Vec::new();
        for (pos, record) in rows.iter().enumerate() {
            if record.event.agent() != Some(of) {
                continue;
            }
            if let old::Event::Rewound { to, .. } = &record.event {
                seen.retain(|(at, _)| *at < to.0);
            } else {
                seen.push((pos as u64, record));
            }
        }
        let mut read = Vec::new();
        let mut group = Vec::new();
        for (_, record) in seen {
            match &record.event {
                old::Event::Input { content, .. } => {
                    group.push(rho_agent_types::transcript::text_content(content));
                }
                old::Event::Native {
                    entry: old::Entry::RequestSent { .. },
                    ..
                } => read.push(format!("request {:?}", std::mem::take(&mut group))),
                _ => read.push("notice".to_owned()),
            }
        }
        read
    }

    fn standing(rows: &[Record<'_>], of: AgentId) -> Vec<String> {
        let mut seen: Vec<(u64, String)> = Vec::new();
        for (pos, record) in rows.iter().enumerate() {
            match &record.event {
                Event::Rewound { agent, to } if *agent == of => {
                    seen.retain(|(at, _)| *at < to.0);
                }
                Event::Request { agent, inputs, .. } if *agent == of => {
                    seen.push((pos as u64, format!("request {:?}", said(inputs))));
                }
                event if event.agent() == Some(of) => {
                    seen.push((pos as u64, "notice".to_owned()));
                }
                _ => {}
            }
        }
        seen.into_iter().map(|(_, read)| read).collect()
    }

    /// The whole hop against a store: heads fold again from the new rows, the
    /// watermark falls back to what a request carried, and the log is dense.
    #[tokio::test]
    async fn a_workset_moves_and_its_heads_fold_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("db"));
        let workset = WorksetId([7; 6]);
        let a = agent(1);
        let mut write = db.write().await;
        write.init_store_tables();
        write.create_workset(workset, UnixMs(1), WorksetKind::Isolated, String::new());
        let spec = AgentSpec {
            runtime: crate::record::Runtime::Claude {
                model: rho_claude::Model::Opus,
                effort: rho_claude::Effort::Medium,
                session_id: uuid::Uuid::from_u128(5),
            },
            props: [("name".to_owned(), "kept".to_owned())].into(),
        };
        write
            .append(
                workset,
                &Record {
                    at: UnixMs(2),
                    event: Event::AgentCreated {
                        agent: a,
                        spec: spec.clone(),
                    },
                },
            )
            .unwrap();
        write_old(
            &mut write,
            workset,
            &[
                old_input(a, "delivered", Some(4)),
                old_request(a, "printed"),
                old_input(a, "tail", Some(6)),
            ],
        );
        migrate(&mut write);
        write.commit();

        let read = db.read();
        let head = read.agent(a).unwrap();
        assert_eq!(head.spec, spec);
        assert_eq!(head.delivered, ChatSeq(4), "the chat hands the tail again");
        let rows = read.workset_rows(workset, Wpos::ZERO, usize::MAX);
        assert_eq!(
            rows.iter().map(|(pos, _)| pos.0).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(head.last, Wpos(2));
        assert_eq!(read.workset(workset).unwrap().next, Wpos(3));
        assert_eq!(read.workset(workset).unwrap().agents, [a]);
    }
}
