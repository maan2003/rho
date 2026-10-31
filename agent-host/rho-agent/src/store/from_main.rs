//! Temporary: converts a store written by `main` (format `6967c9bb`: one
//! log per agent) to worksets and mail
//! ([ARCH-rho2](../../../specs/ARCH-rho2.md)). Remove it once the user's stores
//! have moved forward (`temp-migration` skill).
//!
//! Agents keep their ids. The agents of one `main` workset share one
//! rho2 workset, their rows merged by time. Every message becomes mail:
//! one thread per pair of correspondents. A `Received` row is the `Input`
//! that hands its mail over, with the text the model was shown then.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use camino::Utf8PathBuf;
use redb::TableDefinition;
use rho_agent_types::transcript::{ExecCall, ExecId, PendingInferenceResponse};
use rho_agent_types::{
    Address, AgentId, AgentRole, ContentPart, ExecMilestone, InputWake, MailId, Props, SendKind,
    ThreadId, UnixMs, WorksetId, WorksetKind, Wpos,
};
use rho_db::WriteTxn;
use senax_encoder::{Decode, Encode};
use uuid::Uuid;

use super::{NewMail, StoreWrite as _};
use crate::entry::{Block, Notice, RequestNotice, ResponseUsage, Wake};
use crate::inference::config::{InferenceModel, InferenceProfile};
use crate::inference::{Carry, PromptCacheKey};
use crate::log::{ClaudeOutputBatch, ClaudeRewind, TranscriptLine, WakeFacts};
use crate::record::{AgentSpec, ClaudeRow, Event, Record, Runtime};

pub(crate) const MAIN_FORMAT: &str = "6967c9bb";

/// `main`'s log, read as bytes so a row no decoder knows is set aside, not
/// fatal.
const AGENT_LOG: TableDefinition<(AgentId, u64), Raw> = TableDefinition::new("agent_log");
/// The rows of agents a row of which did not decode, as `main` wrote them.
const SET_ASIDE: TableDefinition<(AgentId, u64), &[u8]> =
    TableDefinition::new("main_agent_log_set_aside");

/// A row's bytes, under the type name `main` gave its log's values.
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
        redb::TypeName::new("rho-db::Sen<AgentEvent<'_>>")
    }
}

fn decode(bytes: &[u8]) -> senax_encoder::Result<AgentEvent<'static>> {
    let mut data = bytes;
    senax_encoder::Decoder::decode(&mut data)
}
/// Tables `main` kept beside the log, all derived from it or dead.
const DROPPED: [&str; 4] = [
    "agent_log",
    "agent_heads",
    "journal",
    "agent_response_subscriptions",
];

// -- `main`'s rows, as it wrote them ------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
struct AgentEventPos {
    pos: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum AgentRuntime {
    Rho { prompt_cache_key: PromptCacheKey },
    Claude { session_id: Uuid },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
enum AgentSpawnedBy {
    Direct,
    Engineer,
    UserOwned { by: AgentId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
enum ClaudeEffort {
    Medium,
    Xhigh,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
enum SessionBinding {
    ClaudeFable { effort: ClaudeEffort },
    ClaudeOpus { effort: ClaudeEffort },
    ResponsesSol(InferenceProfile),
    ResponsesLuna(InferenceProfile),
    ClaudeAdvisor { effort: ClaudeEffort },
    AdvisorSol(InferenceProfile),
    ResponsesAstra(InferenceProfile),
    AdvisorAstra(InferenceProfile),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct Place {
    /// Empty on the oldest agents, from before worksets: they ran on the
    /// host in `cwd`.
    #[senax(default)]
    workset: String,
    cwd: Utf8PathBuf,
    #[senax(default)]
    origin: Option<Utf8PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
enum RuntimeChange {
    ClaudeRewindPending(Option<ClaudeRewind>),
    ClaudeRewound { session_id: Uuid },
    PromptCacheKey(PromptCacheKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
struct MessageId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
enum Party {
    Human,
    Agent(AgentId),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
struct Report {
    notices: Vec<RequestNotice>,
    notebook: rho_notebook::Report,
    messages: Vec<MessageId>,
    acknowledged: Vec<MessageId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum Entry {
    Step {
        at: UnixMs,
        exec: Option<String>,
        prose: String,
        carry: Carry,
        usage: Option<ResponseUsage>,
    },
    RequestSent {
        at: UnixMs,
        why: Wake,
        report: Report,
        compact: bool,
    },
    Received {
        at: UnixMs,
        id: MessageId,
        from: Party,
        body: Vec<Block>,
    },
    Sent {
        at: UnixMs,
        id: MessageId,
        to: Party,
        text: String,
        #[senax(default)]
        kind: SendKind,
    },
    Notice {
        at: UnixMs,
        notice: Notice,
    },
    CompactionTrigger {
        at: UnixMs,
        manual: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
enum AgentEvent<'a> {
    Retired {
        at: UnixMs,
    },
    TitleAttempted {
        at: UnixMs,
    },
    Titled {
        title: Option<String>,
        at: UnixMs,
    },
    Rewound {
        to: AgentEventPos,
        at: UnixMs,
    },
    Failed {
        partial: PendingInferenceResponse,
        error: Cow<'a, str>,
        retrying: bool,
        at: UnixMs,
    },
    Transcript {
        uuid: Uuid,
        line: TranscriptLine,
        at: UnixMs,
        wake: Option<WakeFacts>,
    },
    Created {
        role: AgentRole,
        binding: SessionBinding,
        runtime: AgentRuntime,
        place: Place,
        spawned_by: AgentSpawnedBy,
        spawn_name: Option<String>,
        created_at: UnixMs,
        parent: Option<AgentId>,
    },
    RoleChanged {
        role: AgentRole,
        binding: Option<SessionBinding>,
        at: UnixMs,
    },
    Notice {
        text: Cow<'a, str>,
        at: UnixMs,
    },
    RuntimeRebound {
        change: RuntimeChange,
        at: UnixMs,
    },
    ExecObserved {
        id: ExecId,
        milestone: ExecMilestone,
        at: UnixMs,
    },
    ClaudeExecAdmitted {
        call: ExecCall,
        at: UnixMs,
    },
    ClaudeOutput {
        batch: ClaudeOutputBatch,
    },
    ClaudeOutputHandedOff {
        id: Uuid,
        at: UnixMs,
    },
    #[senax(rename = "TypedEntry")]
    Entry(Entry),
}

// -- the conversion -----------------------------------------------------------

/// What one `main` agent comes to, read in the first pass.
struct Survey {
    created: Option<Created>,
    /// Its newest generated title.
    title: Option<String>,
    /// A row of it did not decode: its rows are set aside, not converted.
    undecodable: bool,
}

struct Created {
    at: UnixMs,
    workset: String,
    cwd: Utf8PathBuf,
    claude: bool,
    spec: AgentSpec,
}

/// Rewrites the `main` tables as worksets and mail, then drops them.
/// The agents of a `main` workset share one rho2 workset, each agent's
/// rows in order, the agents in creation order. Agents a row of which no
/// decoder knows keep their rows as they were, in [`SET_ASIDE`].
pub(crate) fn migrate(write: &mut WriteTxn) {
    write.init_store_tables();
    let (surveys, messages) = survey(write);
    let mail_ids = post_mail(write, &messages);

    let mut agents = Vec::new();
    for (id, survey) in surveys {
        match survey {
            Survey {
                created: Some(created),
                undecodable: false,
                title,
            } => agents.push((id, created, title)),
            _ => {
                eprintln!(
                    "rho: migration sets aside agent {}: a row of it does not decode",
                    id.encoded()
                );
                set_aside(write, id);
            }
        }
    }
    agents.sort_by_key(|(id, created, _)| (created.at, *id));
    let mut worksets: HashMap<String, WorksetId> = HashMap::new();
    for (id, mut created, title) in agents {
        if let Some(title) = title {
            created.spec.props.entry("name".into()).or_insert(title);
        }
        let workset = *worksets
            .entry(created.workset.clone())
            .or_insert_with(|| new_workset(write, &created));
        convert_agent(write, workset, id, &created, &mail_ids);
    }
    for table in DROPPED {
        write.delete_table(table);
    }
}

fn agent_rows(write: &mut WriteTxn, agent: AgentId) -> Vec<(u64, Vec<u8>)> {
    write
        .open_table(AGENT_LOG)
        .range((agent, 0)..=(agent, u64::MAX))
        .map(|(key, value)| (key.value().1, value.value().to_vec()))
        .collect()
}

fn set_aside(write: &mut WriteTxn, agent: AgentId) {
    let rows = agent_rows(write, agent);
    let mut table = write.open_table(SET_ASIDE);
    for (pos, bytes) in rows {
        table.insert((agent, pos), bytes.as_slice());
    }
}

/// One pass over the whole log: each agent's `Created`, title and whether
/// it decodes, and every message once (a send to another agent is also
/// that agent's `Received`, under the same id).
fn survey(write: &mut WriteTxn) -> (BTreeMap<AgentId, Survey>, HashMap<u64, Message>) {
    let mut surveys: BTreeMap<AgentId, Survey> = BTreeMap::new();
    let mut messages = HashMap::new();
    for (key, value) in write.open_table(AGENT_LOG).iter() {
        let (agent, _) = key.value();
        let survey = surveys.entry(agent).or_insert(Survey {
            created: None,
            title: None,
            undecodable: false,
        });
        let event = match decode(value.value()) {
            Ok(event) => event,
            Err(_) => {
                survey.undecodable = true;
                continue;
            }
        };
        match &event {
            AgentEvent::Created {
                role,
                binding,
                runtime,
                place,
                spawned_by,
                spawn_name,
                created_at,
                parent,
            } => {
                survey.created = Some(created(
                    *role,
                    *binding,
                    runtime,
                    place,
                    *spawned_by,
                    spawn_name,
                    *created_at,
                    *parent,
                ))
            }
            AgentEvent::Titled {
                title: Some(title), ..
            } => survey.title = Some(title.clone()),
            AgentEvent::Entry(entry) => {
                if let Some((id, message)) = message(agent, entry) {
                    messages
                        .entry(id)
                        .and_modify(|known: &mut Message| {
                            // The sender's row knows the kind; keep the
                            // earlier time.
                            known.kind = known.kind.or(message.kind);
                            known.at = known.at.min(message.at);
                        })
                        .or_insert(message);
                }
            }
            _ => {}
        }
    }
    (surveys, messages)
}

#[allow(clippy::too_many_arguments)]
fn created(
    role: AgentRole,
    binding: SessionBinding,
    runtime: &AgentRuntime,
    place: &Place,
    spawned_by: AgentSpawnedBy,
    spawn_name: &Option<String>,
    created_at: UnixMs,
    parent: Option<AgentId>,
) -> Created {
    let mut props = Props::new();
    match (role, spawned_by, parent) {
        (AgentRole::Advisor { .. }, _, parent) => {
            props.insert("preset".into(), "advisor".into());
            if let Some(parent) = parent {
                props.insert("parent".into(), parent.encoded());
            }
        }
        (_, AgentSpawnedBy::Engineer, Some(parent)) => {
            props.insert("preset".into(), "child".into());
            props.insert("parent".into(), parent.encoded());
        }
        (_, AgentSpawnedBy::UserOwned { by }, _) => {
            props.insert("preset".into(), "engineer".into());
            props.insert("started_by".into(), by.encoded());
            props.insert("user_facing".into(), "true".into());
        }
        _ => {
            props.insert("preset".into(), "engineer".into());
            props.insert("user_facing".into(), "true".into());
        }
    }
    if let Some(name) = spawn_name.clone() {
        props.insert("name".into(), name);
    }
    props.insert("cwd".into(), place.cwd.to_string());
    let claude = matches!(runtime, AgentRuntime::Claude { .. });
    Created {
        at: created_at,
        workset: place.workset.clone(),
        cwd: place.cwd.clone(),
        claude,
        spec: AgentSpec {
            runtime: runtime_of(binding, runtime),
            props,
        },
    }
}

fn runtime_of(binding: SessionBinding, runtime: &AgentRuntime) -> Runtime {
    let claude_effort = |effort| match effort {
        ClaudeEffort::Medium => rho_claude::Effort::Medium,
        ClaudeEffort::Xhigh => rho_claude::Effort::Xhigh,
        ClaudeEffort::High => rho_claude::Effort::High,
    };
    let native = |model, profile| Runtime::Native {
        model,
        profile,
        cache_key: match runtime {
            AgentRuntime::Rho { prompt_cache_key } => *prompt_cache_key,
            AgentRuntime::Claude { .. } => PromptCacheKey::generate(),
        },
    };
    let claude = |model, effort| Runtime::Claude {
        model,
        effort: claude_effort(effort),
        session_id: match runtime {
            AgentRuntime::Claude { session_id } => *session_id,
            AgentRuntime::Rho { .. } => Uuid::new_v4(),
        },
    };
    match binding {
        SessionBinding::ResponsesSol(profile) | SessionBinding::AdvisorSol(profile) => {
            native(InferenceModel::Gpt61Sol, profile)
        }
        // Luna is gone; Terra took its place.
        SessionBinding::ResponsesLuna(profile) => native(InferenceModel::Gpt61Terra, profile),
        SessionBinding::ResponsesAstra(profile) | SessionBinding::AdvisorAstra(profile) => {
            native(InferenceModel::Gpt6Astra, profile)
        }
        SessionBinding::ClaudeFable { effort } | SessionBinding::ClaudeAdvisor { effort } => {
            claude(rho_claude::Model::Fable, effort)
        }
        SessionBinding::ClaudeOpus { effort } => claude(rho_claude::Model::Opus, effort),
    }
}

/// A message as mail: the first row that told it, by its `main` id.
struct Message {
    at: UnixMs,
    from: Address,
    to: Address,
    kind: Option<SendKind>,
    content: Vec<ContentPart>,
}

fn address(party: Party) -> Address {
    match party {
        Party::Human => Address::User,
        Party::Agent(agent) => Address::Agent(agent),
    }
}

fn content(body: &[Block]) -> Vec<ContentPart> {
    body.iter()
        .map(|block| match block {
            Block::Text(text) => ContentPart::Text { text: text.clone() },
            Block::Image(image) => ContentPart::Image {
                media_type: image.media_type.clone(),
                data: image.data.clone(),
            },
        })
        .collect()
}

/// The message a row tells, by its `main` id.
fn message(agent: AgentId, entry: &Entry) -> Option<(u64, Message)> {
    match entry {
        Entry::Received { at, id, from, body } => Some((
            id.0,
            Message {
                at: *at,
                from: address(*from),
                to: Address::Agent(agent),
                kind: None,
                content: content(body),
            },
        )),
        Entry::Sent {
            at,
            id,
            to,
            text,
            kind,
        } => Some((
            id.0,
            Message {
                at: *at,
                from: Address::Agent(agent),
                to: address(*to),
                kind: (*to == Party::Human).then_some(*kind),
                content: vec![ContentPart::Text { text: text.clone() }],
            },
        )),
        _ => None,
    }
}

/// Posts the mail in time order; one thread per pair of correspondents,
/// named by their first mail.
fn post_mail(write: &mut WriteTxn, mails: &HashMap<u64, Message>) -> HashMap<u64, MailId> {
    let mut order = mails.iter().collect::<Vec<_>>();
    order.sort_by_key(|(id, mail)| (mail.at, **id));
    let mut threads: HashMap<(Address, Address), ThreadId> = HashMap::new();
    let mut ids = HashMap::new();
    for (main_id, mail) in order {
        let pair = if mail.from <= mail.to {
            (mail.from, mail.to)
        } else {
            (mail.to, mail.from)
        };
        let id = write
            .post_mail(
                mail.at,
                NewMail {
                    from: mail.from,
                    to: mail.to,
                    thread: threads.get(&pair).copied(),
                    kind: mail.kind,
                    content: mail.content.clone(),
                },
                None,
            )
            .expect("a migrated thread names its first mail");
        threads.entry(pair).or_insert(ThreadId(id));
        ids.insert(*main_id, id);
    }
    ids
}

/// What the model was shown for a message (`main`'s `render_message`).
fn shown(from: Party, body: &[Block]) -> Vec<ContentPart> {
    let mut text = match from {
        Party::Human => "Message from the human:\n".to_owned(),
        Party::Agent(id) => format!("Message from agent {}:\n", id.encoded()),
    };
    let mut images = Vec::new();
    for block in body {
        match block {
            Block::Text(part) => text.push_str(part),
            Block::Image(image) => images.push(ContentPart::Image {
                media_type: image.media_type.clone(),
                data: image.data.clone(),
            }),
        }
    }
    let mut content = vec![ContentPart::Text { text }];
    content.extend(images);
    content
}

fn new_workset(write: &mut WriteTxn, first: &Created) -> WorksetId {
    let (workset, kind) = match first.workset.parse::<WorksetId>() {
        Ok(workset) => (workset, WorksetKind::Isolated),
        // Adopted, or from before worksets: the host as it is.
        Err(_) => (
            WorksetId(rand::random()),
            WorksetKind::Host {
                cwd: first.cwd.clone(),
            },
        ),
    };
    write.create_workset(
        workset,
        first.at,
        kind,
        format!("# Migrated from rho main (workset {:?}).", first.workset),
    );
    workset
}

fn convert_agent(
    write: &mut WriteTxn,
    workset: WorksetId,
    agent: AgentId,
    created: &Created,
    mail_ids: &HashMap<u64, MailId>,
) {
    let rows = agent_rows(write, agent);
    let mut spec = created.spec.clone();
    let mut placed: BTreeMap<u64, Wpos> = BTreeMap::new();
    let mut at = created.at;
    for (pos, bytes) in rows {
        let event = decode(&bytes).expect("the survey decoded every row of this agent");
        // A row without a time goes with the one before it.
        at = at_of(&event).unwrap_or(at);
        let Some(record) = convert(agent, created, &event, at, &mut spec, &placed, mail_ids) else {
            continue;
        };
        match write.append(workset, &record) {
            Ok(wpos) => {
                placed.insert(pos, wpos);
            }
            // A rewind to before the agent's first kept row, say: the row
            // goes, the rest stands.
            Err(error) => eprintln!(
                "rho: migration drops a row of agent {}: {error:#}",
                agent.encoded()
            ),
        }
    }
}

/// When the row was written; `None` for one that carries no time.
fn at_of(event: &AgentEvent<'_>) -> Option<UnixMs> {
    Some(match event {
        AgentEvent::Retired { at }
        | AgentEvent::TitleAttempted { at }
        | AgentEvent::Titled { at, .. }
        | AgentEvent::Rewound { at, .. }
        | AgentEvent::Failed { at, .. }
        | AgentEvent::Transcript { at, .. }
        | AgentEvent::RoleChanged { at, .. }
        | AgentEvent::Notice { at, .. }
        | AgentEvent::RuntimeRebound { at, .. }
        | AgentEvent::ExecObserved { at, .. }
        | AgentEvent::ClaudeExecAdmitted { at, .. }
        | AgentEvent::ClaudeOutputHandedOff { at, .. } => *at,
        AgentEvent::Created { created_at, .. } => *created_at,
        AgentEvent::ClaudeOutput { .. } => return None,
        AgentEvent::Entry(entry) => match entry {
            Entry::Step { at, .. }
            | Entry::RequestSent { at, .. }
            | Entry::Received { at, .. }
            | Entry::Sent { at, .. }
            | Entry::Notice { at, .. }
            | Entry::CompactionTrigger { at, .. } => *at,
        },
    })
}

/// The agent's spec after `event`, when it changes it.
fn changed_spec(event: &AgentEvent<'_>, current: &AgentSpec) -> Option<AgentSpec> {
    let mut spec = current.clone();
    match event {
        AgentEvent::RoleChanged {
            binding: Some(binding),
            ..
        } => {
            let identity = match &current.runtime {
                Runtime::Native { cache_key, .. } => AgentRuntime::Rho {
                    prompt_cache_key: *cache_key,
                },
                Runtime::Claude { session_id, .. } => AgentRuntime::Claude {
                    session_id: *session_id,
                },
            };
            spec.runtime = runtime_of(*binding, &identity);
        }
        AgentEvent::RuntimeRebound {
            change: RuntimeChange::PromptCacheKey(key),
            ..
        } => match &mut spec.runtime {
            Runtime::Native { cache_key, .. } => *cache_key = *key,
            Runtime::Claude { .. } => return None,
        },
        AgentEvent::RuntimeRebound {
            change: RuntimeChange::ClaudeRewound { session_id: new },
            ..
        } => match &mut spec.runtime {
            Runtime::Claude { session_id, .. } => *session_id = *new,
            Runtime::Native { .. } => return None,
        },
        _ => return None,
    }
    (spec != *current).then_some(spec)
}

/// One row as rho2 keeps it, if it keeps it. `spec`: the agent's spec as
/// the conversion has written it so far; `at`: the row's time.
fn convert(
    id: AgentId,
    created: &Created,
    event: &AgentEvent<'static>,
    at: UnixMs,
    spec: &mut AgentSpec,
    placed: &BTreeMap<u64, Wpos>,
    mail_ids: &HashMap<u64, MailId>,
) -> Option<Record<'static>> {
    let record = |event| Some(Record { at, event });
    match event {
        AgentEvent::Created { .. } => {
            *spec = created.spec.clone();
            record(Event::AgentCreated {
                agent: id,
                spec: created.spec.clone(),
            })
        }
        AgentEvent::RoleChanged { .. } | AgentEvent::RuntimeRebound { .. } => {
            if let AgentEvent::RuntimeRebound {
                change: RuntimeChange::ClaudeRewindPending(pending),
                ..
            } = event
            {
                return record(Event::Claude {
                    agent: id,
                    row: ClaudeRow::RewindPending(pending.clone()),
                });
            }
            *spec = changed_spec(event, spec)?;
            record(Event::AgentChanged {
                agent: id,
                spec: spec.clone(),
            })
        }
        AgentEvent::Retired { .. }
        | AgentEvent::TitleAttempted { .. }
        | AgentEvent::Titled { .. }
        // Told ahead of a user message `main` no longer sends; nothing
        // in rho2 carries it.
        | AgentEvent::Notice { .. } => None,
        AgentEvent::Rewound { to, .. } => {
            // The first kept row at or after `to`.
            let (_, to) = placed.range(to.pos..).next()?;
            let to = *to;
            record(Event::Rewound { agent: id, to })
        }
        AgentEvent::Failed {
            partial,
            error,
            retrying,
            ..
        } => record(Event::Failed {
            agent: id,
            partial: partial.clone(),
            error: error.clone(),
            retrying: *retrying,
        }),
        AgentEvent::Transcript { uuid, line, wake, .. } => record(Event::Claude {
            agent: id,
            row: ClaudeRow::Transcript {
                uuid: *uuid,
                line: line.clone(),
                wake: wake.clone(),
            },
        }),
        AgentEvent::ExecObserved { id: exec, milestone, .. } => record(Event::ExecObserved {
            agent: id,
            exec: exec.clone(),
            milestone: *milestone,
        }),
        AgentEvent::ClaudeExecAdmitted { call, .. } => record(Event::Claude {
            agent: id,
            row: ClaudeRow::ExecAdmitted { call: call.clone() },
        }),
        AgentEvent::ClaudeOutput { batch } => record(Event::Claude {
            agent: id,
            row: ClaudeRow::Output {
                batch: batch.clone(),
            },
        }),
        AgentEvent::ClaudeOutputHandedOff { id: handed, .. } => record(Event::Claude {
            agent: id,
            row: ClaudeRow::OutputHandedOff { id: *handed },
        }),
        AgentEvent::Entry(entry) => {
            convert_entry(id, created.claude, entry, mail_ids).map(|event| Record { at, event })
        }
    }
}

fn convert_entry(
    id: AgentId,
    claude: bool,
    entry: &Entry,
    mail_ids: &HashMap<u64, MailId>,
) -> Option<Event<'static>> {
    let native = |entry| Some(Event::Native { agent: id, entry });
    let report = |report: &Report| crate::entry::Report {
        notices: report.notices.clone(),
        notebook: report.notebook.clone(),
    };
    match entry {
        Entry::Received {
            id: message,
            from,
            body,
            ..
        } => Some(Event::Input {
            agent: id,
            mail: mail_ids.get(&message.0).copied(),
            content: shown(*from, body),
            wake: match from {
                Party::Human => InputWake::Person,
                Party::Agent(_) => InputWake::Agent,
            },
        }),
        // Its mail is in the mail log; the send was a cell.
        Entry::Sent { .. } => None,
        Entry::Notice { notice, .. } if claude => Some(Event::Claude {
            agent: id,
            row: ClaudeRow::Notice(notice.clone()),
        }),
        Entry::Notice { notice, .. } => native(crate::entry::Entry::Notice {
            notice: notice.clone(),
        }),
        Entry::RequestSent {
            why, report: sent, ..
        } if claude => Some(Event::Claude {
            agent: id,
            row: ClaudeRow::RequestSent {
                why: *why,
                report: report(sent),
            },
        }),
        Entry::RequestSent {
            why,
            report: sent,
            compact,
            ..
        } => native(crate::entry::Entry::RequestSent {
            why: *why,
            report: report(sent),
            compact: *compact,
        }),
        Entry::Step {
            exec,
            prose,
            carry,
            usage,
            ..
        } => native(crate::entry::Entry::Step {
            exec: exec.clone(),
            prose: prose.clone(),
            carry: carry.clone(),
            usage: usage.clone(),
        }),
        Entry::CompactionTrigger { manual, .. } => {
            native(crate::entry::Entry::CompactionTrigger { manual: *manual })
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AdvisorIntelligence, AgentIdDomain, EngineerIntelligence};
    use rho_db::RhoDb;

    use super::*;
    use crate::inference::Call;
    use crate::inference::config::ReasoningEffort;
    use crate::store::StoreRead as _;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &AgentIdDomain(7)).unwrap()
    }

    const PROFILE: InferenceProfile = InferenceProfile {
        effort: ReasoningEffort::High,
        fast_mode: false,
    };

    fn created(
        binding: SessionBinding,
        runtime: AgentRuntime,
        workset: &str,
        spawned_by: AgentSpawnedBy,
        parent: Option<AgentId>,
        at: u64,
    ) -> AgentEvent<'static> {
        AgentEvent::Created {
            role: AgentRole::Engineer {
                intelligence: EngineerIntelligence::Medium,
            },
            binding,
            runtime,
            place: Place {
                workset: workset.into(),
                cwd: "/src/repo".into(),
                origin: None,
            },
            spawned_by,
            spawn_name: None,
            created_at: UnixMs(at),
            parent,
        }
    }

    fn entry(entry: Entry) -> AgentEvent<'static> {
        AgentEvent::Entry(entry)
    }

    fn received(at: u64, id: u64, from: Party, text: &str) -> AgentEvent<'static> {
        entry(Entry::Received {
            at: UnixMs(at),
            id: MessageId(id),
            from,
            body: vec![Block::Text(text.into())],
        })
    }

    fn sent(at: u64, id: u64, to: Party, text: &str) -> AgentEvent<'static> {
        entry(Entry::Sent {
            at: UnixMs(at),
            id: MessageId(id),
            to,
            text: text.into(),
            kind: SendKind::Result,
        })
    }

    fn request(at: u64) -> AgentEvent<'static> {
        entry(Entry::RequestSent {
            at: UnixMs(at),
            why: Wake::Message,
            report: Report::default(),
            compact: false,
        })
    }

    fn step(at: u64, code: &str) -> AgentEvent<'static> {
        entry(Entry::Step {
            at: UnixMs(at),
            exec: Some(code.into()),
            prose: String::new(),
            carry: Carry::new(
                serde_json::json!({}),
                vec![Call::new("c", code.into())],
                false,
            ),
            usage: None,
        })
    }

    fn write_main(write: &mut WriteTxn, agent: AgentId, rows: Vec<AgentEvent<'static>>) {
        let mut table = write.open_table(AGENT_LOG);
        for (pos, event) in rows.into_iter().enumerate() {
            let mut bytes = bytes::BytesMut::new();
            senax_encoder::Encoder::encode(&event, &mut bytes).unwrap();
            table.insert((agent, pos as u64), &bytes[..]);
        }
    }

    fn text(content: &[ContentPart]) -> String {
        rho_agent_types::transcript::text_content(content)
    }

    /// A `main` store: an engineer and its child in one workset, a Claude
    /// agent from before worksets, and an agent no decoder knows.
    #[tokio::test]
    async fn a_main_store_becomes_worksets_and_mail() {
        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(dir.path().join("db"));
        let (a, b, c, d) = (agent(1), agent(2), agent(3), agent(4));
        let key = PromptCacheKey::generate();
        let session = Uuid::new_v4();
        {
            let mut write = db.write().await;
            write
                .open_table(crate::db::FORMAT)
                .insert((), MAIN_FORMAT.to_owned());
            write_main(
                &mut write,
                a,
                vec![
                    created(
                        SessionBinding::ResponsesSol(PROFILE),
                        AgentRuntime::Rho {
                            prompt_cache_key: key,
                        },
                        "0a1b2c3d4e5f",
                        AgentSpawnedBy::Direct,
                        None,
                        10,
                    ),
                    received(11, 900, Party::Human, "first"),
                    request(12),
                    step(13, "print(1)"),
                    sent(14, 901, Party::Human, "done one"),
                    sent(15, 902, Party::Agent(b), "help me"),
                    received(30, 903, Party::Human, "second"),
                    request(31),
                    step(32, "print(2)"),
                    AgentEvent::Rewound {
                        to: AgentEventPos { pos: 6 },
                        at: UnixMs(33),
                    },
                    AgentEvent::Titled {
                        title: Some("Fix the bug".into()),
                        at: UnixMs(34),
                    },
                ],
            );
            write_main(
                &mut write,
                b,
                vec![
                    created(
                        SessionBinding::ResponsesLuna(PROFILE),
                        AgentRuntime::Rho {
                            prompt_cache_key: PromptCacheKey::generate(),
                        },
                        "0a1b2c3d4e5f",
                        AgentSpawnedBy::Engineer,
                        Some(a),
                        15,
                    ),
                    received(16, 902, Party::Agent(a), "help me"),
                    request(17),
                ],
            );
            write_main(
                &mut write,
                c,
                vec![
                    AgentEvent::Created {
                        role: AgentRole::Advisor {
                            intelligence: AdvisorIntelligence::Medium1,
                        },
                        binding: SessionBinding::ClaudeAdvisor {
                            effort: ClaudeEffort::Xhigh,
                        },
                        runtime: AgentRuntime::Claude {
                            session_id: session,
                        },
                        place: Place {
                            workset: String::new(),
                            cwd: "/home/u/src/rho".into(),
                            origin: None,
                        },
                        spawned_by: AgentSpawnedBy::Engineer,
                        spawn_name: Some("advisor".into()),
                        created_at: UnixMs(1),
                        parent: Some(a),
                    },
                    received(2, 904, Party::Human, "review"),
                    request(3),
                ],
            );
            write
                .open_table(AGENT_LOG)
                .insert((d, 0), &b"\xffno decoder knows this"[..]);
            write.commit();
        }

        crate::db::prepare(&db).await;

        let read = db.read();
        assert!(!read.has_table("agent_log"), "the main log is gone");
        let mails = read.mail_since(MailId(0), 100);
        let summary = mails
            .iter()
            .map(|m| (m.from, m.to, text(&m.content), m.thread, m.kind))
            .collect::<Vec<_>>();
        let (user, aa, ab, ac) = (
            Address::User,
            Address::Agent(a),
            Address::Agent(b),
            Address::Agent(c),
        );
        // In time order, the send to the child once; one thread per pair.
        assert_eq!(
            summary,
            vec![
                (user, ac, "review".into(), ThreadId(MailId(1)), None),
                (user, aa, "first".into(), ThreadId(MailId(2)), None),
                (
                    aa,
                    user,
                    "done one".into(),
                    ThreadId(MailId(2)),
                    Some(SendKind::Result)
                ),
                (aa, ab, "help me".into(), ThreadId(MailId(4)), None),
                (user, aa, "second".into(), ThreadId(MailId(2)), None),
            ]
        );

        let worksets = read.worksets();
        assert_eq!(worksets.len(), 2);
        let isolated: WorksetId = "0a1b2c3d4e5f".parse().unwrap();
        let head = read.workset(isolated).unwrap();
        assert_eq!(head.kind, WorksetKind::Isolated);
        assert_eq!(head.agents, vec![a, b]);
        let (_, host) = worksets.iter().find(|(id, _)| *id != isolated).unwrap();
        assert_eq!(
            host.kind,
            WorksetKind::Host {
                cwd: "/home/u/src/rho".into()
            }
        );
        assert_eq!(host.agents, vec![c]);

        let ha = read.agent(a).unwrap();
        assert_eq!(
            ha.spec.runtime,
            Runtime::Native {
                model: InferenceModel::Gpt61Sol,
                profile: PROFILE,
                cache_key: key
            }
        );
        let props = |head: &crate::store::AgentHead| {
            head.spec
                .props
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            props(&ha),
            "cwd=/src/repo name=Fix the bug preset=engineer user_facing=true"
        );
        assert_eq!(ha.delivered, MailId(5));
        let hb = read.agent(b).unwrap();
        assert!(matches!(
            hb.spec.runtime,
            Runtime::Native {
                model: InferenceModel::Gpt61Terra,
                ..
            }
        ));
        assert_eq!(
            props(&hb),
            format!("cwd=/src/repo parent={} preset=child", a.encoded())
        );
        let hc = read.agent(c).unwrap();
        assert_eq!(
            hc.spec.runtime,
            Runtime::Claude {
                model: rho_claude::Model::Fable,
                effort: rho_claude::Effort::Xhigh,
                session_id: session
            }
        );
        assert_eq!(
            props(&hc),
            format!(
                "cwd=/home/u/src/rho name=advisor parent={} preset=advisor",
                a.encoded()
            )
        );

        // The rewind to the second message hides it and what answered it;
        // the inputs say what the model was shown, and name their mail.
        let visible = read.agent_rows(a, Wpos::ZERO);
        let inputs = visible
            .iter()
            .filter_map(|(_, record)| match &record.event {
                Event::Input {
                    mail,
                    content,
                    wake,
                    ..
                } => Some((*mail, text(content), *wake)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            inputs,
            vec![(
                Some(MailId(2)),
                "Message from the human:\nfirst".into(),
                InputWake::Person
            )]
        );
        let steps = visible
            .iter()
            .filter(|(_, record)| {
                matches!(
                    &record.event,
                    Event::Native {
                        entry: crate::entry::Entry::Step { .. },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(steps, 1);
        let b_inputs = read
            .agent_rows(b, Wpos::ZERO)
            .into_iter()
            .filter_map(|(_, record)| match record.event {
                Event::Input {
                    mail,
                    content,
                    wake,
                    ..
                } => Some((mail, text(&content), wake)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            b_inputs,
            vec![(
                Some(MailId(4)),
                format!("Message from agent {}:\nhelp me", a.encoded()),
                InputWake::Agent
            )]
        );
        assert!(
            read.agent_rows(c, Wpos::ZERO)
                .iter()
                .any(|(_, record)| matches!(
                    record.event,
                    Event::Claude {
                        row: ClaudeRow::RequestSent { .. },
                        ..
                    }
                )),
            "a Claude agent's request is a Claude row"
        );

        assert!(read.agent(d).is_none());
        let aside = read
            .open_table(SET_ASIDE)
            .iter()
            .map(|(key, value)| (key.value(), value.value().to_vec()))
            .collect::<Vec<_>>();
        assert_eq!(aside, vec![((d, 0), b"\xffno decoder knows this".to_vec())]);
        assert_eq!(
            read.open_table(crate::db::FORMAT)
                .get(())
                .map(|v| v.value()),
            Some(crate::db::CURRENT_FORMAT.to_owned())
        );
        drop(read);
        assert_eq!(
            crate::db::savepoints(&db)
                .await
                .into_iter()
                .map(|(_, hop)| hop)
                .collect::<Vec<_>>(),
            vec![Some(format!(
                "{MAIN_FORMAT}->{}",
                crate::db::CURRENT_FORMAT
            ))]
        );
    }
}
