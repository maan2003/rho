//! The one pass from the store the user has (format `b1e40c93`: raw events
//! keyed by lineage, a folded head per agent, the story log with its
//! source index, the transitional attention table) to the per-agent log
//! and the journal. Deleted in the landing after the user has restarted on
//! it (`AGENT-LOG-DESIGN.md`).
//!
//! Every lineage of an agent is laid out in id order (the order the forks
//! were made), each fork opening with a `Rewound` that takes back the
//! parent's tail from the fork point, so the visible history comes out
//! exactly as the lineage walk used to read it. What only the story knew
//! (turn edges, titles, activity labels, what a turn wanted) becomes raw
//! rows woven in right after the raw row each followed, so the fold reads
//! the same times the story showed.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use redb::{TableDefinition, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::AgentId;
use rho_db::{RecordedTypeName, SenAs, SenValue, WriteTxn};
use rho_workspaces::WorkspaceInfo;
use senax_encoder::{Decode, Encode};

use super::legacy_events::{self, LegacyAgentEvent, Translator};
use super::{
    AGENT_LOG, AgentRole, AgentRuntime, AgentSpawnedBy, AgentUsageBucket, AgentWant, ClaudeRewind,
    JOURNAL, PresentationField, SessionBinding, TurnEdge, TurnOutcome, UnixMillis,
};
use crate::AgentEvent;

/// Rows moved per transaction step, so no one lineage is held in memory
/// whole.
const BATCH_ROWS: usize = 256;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
struct AgentLineageId(u64);

/// The old key under its old name: redb checks the type name a table was
/// written with, and redb-derive makes it from these identifiers.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
struct AgentEventPos {
    lineage_id: AgentLineageId,
    seq: u32,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Key,
    RedbValue,
    Encode,
    Decode,
)]
struct StoryPos(u64);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
struct StoryKey {
    agent_id: AgentId,
    pos: StoryPos,
}

const AGENT_HEADS: TableDefinition<AgentId, SenAs<AgentHead, AgentHeadName>> =
    TableDefinition::new("agent_heads");
const AGENT_EVENTS: TableDefinition<
    AgentEventPos,
    SenAs<LegacyAgentEvent<'static>, legacy_events::AgentEventName>,
> = TableDefinition::new("agent_events");
const LINEAGE_PARENTS: TableDefinition<AgentLineageId, AgentEventPos> =
    TableDefinition::new("lineage_parents");
const AGENT_STORY: TableDefinition<StoryKey, SenAs<StoryEvent, StoryEventName>> =
    TableDefinition::new("agent_story");
const AGENT_STORY_SOURCE: TableDefinition<StoryKey, AgentEventPos> =
    TableDefinition::new("agent_story_source");

/// The old tables were written from other modules, and redb checks the
/// name a value type was written under.
#[derive(Debug)]
struct AgentHeadName;

impl RecordedTypeName for AgentHeadName {
    const NAME: &'static str = "rho-db::Sen<rho_agent::db::AgentHead>";
}

#[derive(Debug)]
struct StoryEventName;

impl RecordedTypeName for StoryEventName {
    const NAME: &'static str = "rho-db::Sen<rho_agent::story::StoryEvent>";
}

/// The head as the old build last folded it. Only this file decodes it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct AgentHead {
    config: AgentConfig,
    #[senax(default)]
    story_pos: StoryPos,
    generated_title: Option<String>,
    activity: Option<String>,
    #[senax(default)]
    turn_running: bool,
    #[senax(default)]
    story_built: bool,
    #[senax(default)]
    parent: Option<AgentId>,
    current_lineage: AgentLineageId,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct AgentConfig {
    role: AgentRole,
    binding: SessionBinding,
    runtime: AgentRuntime,
    workdirs: Vec<WorkspaceInfo>,
    spawned_by: AgentSpawnedBy,
    spawn_name: Option<String>,
    created_at: UnixMillis,
    claude_rewind: Option<ClaudeRewind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
enum RuntimeKind {
    Rho,
    Claude,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum ToolLine {
    Path(Utf8PathBuf),
    Command(String),
    Query(String),
    Agent(AgentId),
    Nothing,
}

/// The story as the old build wrote it, variant for variant: the encoder
/// names variants and fields by their identifiers.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum StoryEvent {
    Created {
        role: AgentRole,
        runtime_kind: RuntimeKind,
        workdirs: Vec<WorkspaceInfo>,
        spawned_by: AgentSpawnedBy,
        spawn_name: Option<String>,
        at: UnixMillis,
    },
    Parented {
        parent: AgentId,
        at: UnixMillis,
    },
    UserMessage {
        text: String,
        at: UnixMillis,
    },
    AgentMail {
        from: AgentId,
        text: String,
        at: UnixMillis,
    },
    TurnStarted {
        at: UnixMillis,
    },
    TurnEnded {
        outcome: TurnOutcome,
        at: UnixMillis,
    },
    Reply {
        text: String,
        at: UnixMillis,
    },
    ToolCall {
        name: rho_core::ToolName,
        what: ToolLine,
        at: UnixMillis,
    },
    Wants {
        want: AgentWant,
        summary: Option<String>,
        at: UnixMillis,
    },
    Titled {
        title: String,
        at: UnixMillis,
    },
    Activity {
        label: Option<String>,
        at: UnixMillis,
    },
    Cost {
        usage: AgentUsageBucket,
        at: UnixMillis,
    },
    Rewound {
        to: StoryPos,
        at: UnixMillis,
    },
    Compacted {
        at: UnixMillis,
    },
    RoleChanged {
        role: AgentRole,
        at: UnixMillis,
    },
    WorkdirAdded {
        workdir: WorkspaceInfo,
        at: UnixMillis,
    },
    HistoryUnavailableBefore {
        at: UnixMillis,
    },
}

impl StoryEvent {
    fn at(&self) -> UnixMillis {
        match self {
            Self::Created { at, .. }
            | Self::Parented { at, .. }
            | Self::UserMessage { at, .. }
            | Self::AgentMail { at, .. }
            | Self::TurnStarted { at }
            | Self::TurnEnded { at, .. }
            | Self::Reply { at, .. }
            | Self::ToolCall { at, .. }
            | Self::Wants { at, .. }
            | Self::Titled { at, .. }
            | Self::Activity { at, .. }
            | Self::Cost { at, .. }
            | Self::Rewound { at, .. }
            | Self::Compacted { at }
            | Self::RoleChanged { at, .. }
            | Self::WorkdirAdded { at, .. }
            | Self::HistoryUnavailableBefore { at } => *at,
        }
    }

    /// The raw row this story row becomes, for the rows only the story
    /// carried. Everything else is a raw row's projection already, or
    /// (cost, rewinds) is told by something the new log keeps elsewhere.
    fn raw(self) -> Option<AgentEvent<'static>> {
        Some(match self {
            Self::TurnStarted { at } => AgentEvent::Turn {
                edge: TurnEdge::Started,
                at,
            },
            Self::TurnEnded { outcome, at } => AgentEvent::Turn {
                edge: TurnEdge::Ended(outcome),
                at,
            },
            Self::Wants { want, summary, at } => AgentEvent::Wants { want, summary, at },
            Self::Titled { title, at } => AgentEvent::Presented {
                title: PresentationField::Set(title),
                activity: PresentationField::Unchanged,
                at,
            },
            Self::Activity { label, at } => AgentEvent::Presented {
                title: PresentationField::Unchanged,
                activity: label.map_or(PresentationField::Clear, PresentationField::Set),
                at,
            },
            _ => return None,
        })
    }
}

/// What the pass found, for the line the user is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub agents: usize,
    pub rows: u64,
    pub forks: usize,
    /// Rows only the story carried, now raw rows.
    pub story_rows: u64,
    /// Agents whose head said a title or activity the story had not.
    pub heads_told: usize,
}

impl MigrationReport {
    pub fn line(&self) -> String {
        format!(
            "fused migration: {} agents, {} rows, {} forks, {} story rows woven in, \
             {} heads told again",
            self.agents, self.rows, self.forks, self.story_rows, self.heads_told
        )
    }
}

pub(super) fn migrate(write: &mut WriteTxn) {
    let report = run(write);
    eprintln!("{}", report.line());
}

/// Where the rows of one agent go, and what its title and activity fold
/// to along the way.
struct Sink<'a> {
    agent_id: AgentId,
    pos: u64,
    seq: &'a mut u64,
    title: Option<String>,
    activity: Option<String>,
}

impl Sink<'_> {
    fn push(&mut self, write: &mut WriteTxn, event: &AgentEvent<'_>) -> u64 {
        let at = self.pos;
        write
            .open_table(AGENT_LOG)
            .insert(&(self.agent_id, at), SenValue::borrowed(event));
        *self.seq += 1;
        write
            .open_table(JOURNAL)
            .insert(&*self.seq, &(self.agent_id, at));
        self.pos += 1;
        // The same fold the head reads with, so the head's own say at the
        // end is only what the rows had not said.
        let mut presented = |title: &PresentationField, activity: &PresentationField| {
            for (field, slot) in [(title, &mut self.title), (activity, &mut self.activity)] {
                match field {
                    PresentationField::Unchanged => {}
                    PresentationField::Set(value) => *slot = Some(value.clone()),
                    PresentationField::Clear => *slot = None,
                }
            }
        };
        match event {
            AgentEvent::Presented {
                title, activity, ..
            } => presented(title, activity),
            AgentEvent::Turn {
                edge: TurnEdge::Ended(_),
                ..
            } => self.activity = None,
            _ => {}
        }
        at
    }
}

/// Every head becomes a log: creation first, then its lineages in fork
/// order with the story's own rows woven in, then whatever the head said
/// that the story had not. The journal is built agent by agent in
/// creation order. Returns what it saw.
pub fn run(write: &mut WriteTxn) -> MigrationReport {
    let mut report = MigrationReport::default();
    let mut heads = write
        .open_table(AGENT_HEADS)
        .iter()
        .map(|(key, value)| (key.value(), value.value().into_owned()))
        .collect::<Vec<_>>();
    heads.sort_by_key(|(agent_id, head)| (head.config.created_at, *agent_id));
    let parents = write
        .open_table(LINEAGE_PARENTS)
        .iter()
        .map(|(key, value)| (key.value(), value.value()))
        .collect::<BTreeMap<_, _>>();
    let root_of = |mut lineage: AgentLineageId| {
        while let Some(parent) = parents.get(&lineage) {
            lineage = parent.lineage_id;
        }
        lineage
    };
    let mut lineages_by_root: BTreeMap<AgentLineageId, Vec<AgentLineageId>> = BTreeMap::new();
    for lineage in lineage_ids(write)
        .into_iter()
        .chain(parents.keys().copied())
    {
        let group = lineages_by_root.entry(root_of(lineage)).or_default();
        if !group.contains(&lineage) {
            group.push(lineage);
        }
    }

    let fork_points = parents
        .values()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let mut seq = 0_u64;
    let total = heads.len();
    let started = std::time::Instant::now();
    let mut told = std::time::Instant::now();
    for (agent_id, head) in heads {
        report.agents += 1;
        if told.elapsed() >= std::time::Duration::from_secs(5) {
            told = std::time::Instant::now();
            eprintln!(
                "rho-agent: migrating agent {}/{total}, {} rows so far, {:?} in",
                report.agents,
                report.rows + report.story_rows,
                started.elapsed()
            );
        }
        let root = root_of(head.current_lineage);
        let mut lineages = lineages_by_root.remove(&root).unwrap_or_default();
        lineages.retain(|lineage| *lineage != root);
        lineages.sort();
        lineages.insert(0, root);
        let (mut anchored, last_at) = story_rows(write, agent_id);
        let mut sink = Sink {
            agent_id,
            pos: 0,
            seq: &mut seq,
            title: None,
            activity: None,
        };

        // Creation is row zero, told with the parent the head knew.
        let first = write
            .open_table(AGENT_EVENTS)
            .get(&AgentEventPos {
                lineage_id: root,
                seq: 0,
            })
            .map(|value| value.value().into_owned());
        let created = match first {
            Some(LegacyAgentEvent::Created {
                role,
                binding,
                runtime,
                workdirs,
                spawned_by,
                spawn_name,
                created_at,
                ..
            }) => AgentEvent::Created {
                role,
                binding,
                runtime,
                workdirs,
                spawned_by,
                spawn_name,
                created_at,
                parent: head.parent,
            },
            _ => AgentEvent::Created {
                role: head.config.role,
                binding: head.config.binding.clone(),
                runtime: head.config.runtime.clone(),
                workdirs: head.config.workdirs.clone(),
                spawned_by: head.config.spawned_by,
                spawn_name: head.config.spawn_name.clone(),
                created_at: head.config.created_at,
                parent: head.parent,
            },
        };
        sink.push(write, &created);
        for event in anchored.remove(&None).unwrap_or_default() {
            sink.push(write, &event);
            report.story_rows += 1;
        }

        let mut new_pos: BTreeMap<AgentEventPos, u64> = BTreeMap::new();
        let mut lineage_end: BTreeMap<AgentLineageId, u64> = BTreeMap::new();
        // The old loop's state where each fork left its parent, and at
        // every lineage's tail, so a fork resumes with what its parent had
        // in flight: results not yet committed, the queue, the context use.
        let mut at_fork: BTreeMap<AgentEventPos, Translator> = BTreeMap::new();
        let mut at_tail: BTreeMap<AgentLineageId, Translator> = BTreeMap::new();
        for lineage in lineages {
            let mut translator = parents
                .get(&lineage)
                .and_then(|parent| {
                    at_fork
                        .get(parent)
                        .or_else(|| at_tail.get(&parent.lineage_id))
                        .cloned()
                })
                .unwrap_or_else(|| Translator::new(head.config.created_at));
            if let Some(parent) = parents.get(&lineage) {
                // The fork took back the parent's rows from `seq` on. A
                // fork at the parent's very tail took back nothing, and
                // its `to` is the row after the parent's last.
                let to = new_pos
                    .get(parent)
                    .copied()
                    .or_else(|| lineage_end.get(&parent.lineage_id).map(|end| end + 1));
                if let Some(to) = to {
                    report.forks += 1;
                    sink.push(
                        write,
                        &AgentEvent::Rewound {
                            to: super::AgentEventPos::new(to),
                            at: rho_core::UnixMs(0),
                        },
                    );
                }
            }
            let mut cursor = AgentEventPos {
                lineage_id: lineage,
                seq: 0,
            };
            loop {
                let batch = write
                    .open_table(AGENT_EVENTS)
                    .range(
                        cursor..=AgentEventPos {
                            lineage_id: lineage,
                            seq: u32::MAX,
                        },
                    )
                    .take(BATCH_ROWS)
                    .map(|(key, value)| (key.value(), value.value().into_owned()))
                    .collect::<Vec<_>>();
                let Some((last, _)) = batch.last() else {
                    break;
                };
                let last = *last;
                for (key, event) in batch {
                    if matches!(event, LegacyAgentEvent::Created { .. }) {
                        // Already row zero.
                        continue;
                    }
                    if fork_points.contains(&key) {
                        at_fork.insert(key, translator.clone());
                    }
                    // Where this row's say begins, even when it says
                    // nothing yet: a fork here takes back what follows.
                    new_pos.insert(key, sink.pos);
                    for event in translator.translate(event) {
                        let at = sink.push(write, &event);
                        lineage_end.insert(lineage, at);
                    }
                    report.rows += 1;
                    for event in anchored.remove(&Some(key)).unwrap_or_default() {
                        let at = sink.push(write, &event);
                        lineage_end.insert(lineage, at);
                        report.story_rows += 1;
                    }
                }
                if last.seq == u32::MAX {
                    break;
                }
                cursor = AgentEventPos {
                    lineage_id: lineage,
                    seq: last.seq + 1,
                };
            }
            // A fork from the tail resumes before the closing rows, which
            // its `to` (the row after the last one the lineage had) takes
            // back.
            at_tail.insert(lineage, translator.clone());
            for event in translator.finish() {
                sink.push(write, &event);
            }
        }
        // Story rows whose raw source is not in any of this agent's
        // lineages: kept, at the end, rather than lost.
        for (_, events) in anchored {
            for event in events {
                sink.push(write, &event);
                report.story_rows += 1;
            }
        }
        // The old head never kept a generated title behind a spawn name;
        // the log keeps what the sidecar said, and a reader prefers the
        // spawn name on its own.
        let title_stands = head.config.spawn_name.is_some() || sink.title == head.generated_title;
        if !title_stands || sink.activity != head.activity {
            let field = |told: &Option<String>, want: &Option<String>| match (told, want) {
                _ if told == want => PresentationField::Unchanged,
                (_, Some(value)) => PresentationField::Set(value.clone()),
                (_, None) => PresentationField::Clear,
            };
            report.heads_told += 1;
            sink.push(
                write,
                &AgentEvent::Presented {
                    title: if title_stands {
                        PresentationField::Unchanged
                    } else {
                        field(&sink.title, &head.generated_title)
                    },
                    activity: field(&sink.activity, &head.activity),
                    at: last_at.unwrap_or(head.config.created_at),
                },
            );
        }
    }
    eprintln!(
        "rho-agent: {} agents rewritten in {:?}; dropping the old tables",
        report.agents,
        started.elapsed()
    );
    // The projects table stays for the daemon's own conversion; the view
    // config was the old GUI's and nothing reads it.
    for table in [
        "agent_heads",
        "agent_events",
        "lineage_parents",
        "agent_story",
        "agent_story_source",
        "agent_attention_until_slice_b",
        // Slice A's title and activity record, whose outcome the head
        // holds; a store that never ran slice B still has it.
        "agent_presentation_events",
        "view_config",
    ] {
        write.delete_table(table);
    }
    report
}

/// The story rows only the story carried, each under the raw row it
/// followed (`None`: before any row with a source), and the time of the
/// newest story row.
#[allow(clippy::type_complexity)]
fn story_rows(
    write: &mut WriteTxn,
    agent_id: AgentId,
) -> (
    BTreeMap<Option<AgentEventPos>, Vec<AgentEvent<'static>>>,
    Option<UnixMillis>,
) {
    let range = StoryKey {
        agent_id,
        pos: StoryPos(0),
    }..=StoryKey {
        agent_id,
        pos: StoryPos(u64::MAX),
    };
    let sources = write
        .open_table(AGENT_STORY_SOURCE)
        .range(range.clone())
        .map(|(key, value)| (key.value().pos, value.value()))
        .collect::<BTreeMap<_, _>>();
    let mut anchored: BTreeMap<Option<AgentEventPos>, Vec<AgentEvent<'static>>> = BTreeMap::new();
    let mut anchor = None;
    let mut last_at = None;
    for (key, value) in write.open_table(AGENT_STORY).range(range) {
        let pos = key.value().pos;
        let event = value.value().into_owned();
        last_at = last_at.max(Some(event.at()));
        if let Some(source) = sources.get(&pos) {
            anchor = Some(*source);
            continue;
        }
        if let Some(raw) = event.raw() {
            anchored.entry(anchor).or_default().push(raw);
        }
    }
    (anchored, last_at)
}

/// Every lineage with a row, found by jumping from each lineage's last
/// possible key to the next lineage's first.
fn lineage_ids(write: &mut WriteTxn) -> Vec<AgentLineageId> {
    let events = write.open_table(AGENT_EVENTS);
    let mut ids = Vec::new();
    let mut cursor = events.iter().next().map(|(key, _)| key.value().lineage_id);
    while let Some(lineage_id) = cursor {
        ids.push(lineage_id);
        cursor = events
            .range(
                AgentEventPos {
                    lineage_id,
                    seq: u32::MAX,
                }..,
            )
            .next()
            .map(|(key, _)| key.value().lineage_id)
            .filter(|next| *next != lineage_id);
    }
    ids
}

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;
    use rho_db::RhoDb;

    use super::*;
    use crate::db::tests::user_event;
    use crate::db::{
        AgentIdDomain, AgentReadTxnExt as _, AgentWriteTxnExt as _, InferenceProfile,
        PromptCacheKey,
    };
    use crate::{InputKind, QueuedInput};

    fn legacy_created(created_at: u64) -> LegacyAgentEvent<'static> {
        LegacyAgentEvent::Created {
            role: AgentRole::default(),
            binding: SessionBinding::ResponsesSol(InferenceProfile::default()),
            runtime: AgentRuntime::Rho {
                prompt_cache_key: PromptCacheKey::generate(),
            },
            workdirs: vec![WorkspaceInfo::UserCheckout {
                repo: "/tmp/rho".into(),
            }],
            spawned_by: AgentSpawnedBy::Direct,
            spawn_name: None,
            created_at: UnixMs(created_at),
            parent: None,
        }
    }

    fn legacy_head(current_lineage: AgentLineageId, parent: AgentId) -> AgentHead {
        AgentHead {
            config: AgentConfig {
                role: AgentRole::default(),
                binding: SessionBinding::ResponsesSol(InferenceProfile::default()),
                runtime: AgentRuntime::Rho {
                    prompt_cache_key: PromptCacheKey::generate(),
                },
                workdirs: vec![WorkspaceInfo::UserCheckout {
                    repo: "/tmp/rho".into(),
                }],
                spawned_by: AgentSpawnedBy::Direct,
                spawn_name: None,
                created_at: UnixMs(100),
                claude_rewind: None,
            },
            story_pos: StoryPos(6),
            generated_title: Some("a-title".to_owned()),
            activity: Some("still at it".to_owned()),
            turn_running: false,
            story_built: true,
            parent: Some(parent),
            current_lineage,
        }
    }

    fn story_key(agent_id: AgentId, pos: u64) -> StoryKey {
        StoryKey {
            agent_id,
            pos: StoryPos(pos),
        }
    }

    /// A store at `b1e40c93`: one agent whose history forked once.
    /// Lineage 1 holds creation and three user messages; lineage 2 forks
    /// at seq 3 and adds one. The story tells a turn around the first
    /// message, a title, and what the turn wanted.
    async fn legacy_store(db: &RhoDb) -> (AgentId, AgentId) {
        let mut write = db.write().await;
        write
            .open_table(super::super::FORMAT)
            .insert(&(), &"b1e40c93".to_owned());
        let mut machine = write.open_table(super::super::MACHINE);
        machine.insert(&super::super::MACHINE_SEED_KEY, &7);
        drop(machine);
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(7)).unwrap();
        let parent = AgentId::from_counter(2, &AgentIdDomain(7)).unwrap();
        {
            let mut events = write.open_table(AGENT_EVENTS);
            events.insert(
                &AgentEventPos {
                    lineage_id: AgentLineageId(1),
                    seq: 0,
                },
                SenValue::borrowed(&legacy_created(100)),
            );
            for (seq, text) in ["one", "two", "three"].into_iter().enumerate() {
                events.insert(
                    &AgentEventPos {
                        lineage_id: AgentLineageId(1),
                        seq: seq as u32 + 1,
                    },
                    SenValue::borrowed(&legacy_events::legacy_of(user_event(text))),
                );
            }
            events.insert(
                &AgentEventPos {
                    lineage_id: AgentLineageId(2),
                    seq: 0,
                },
                SenValue::borrowed(&legacy_events::legacy_of(user_event("four"))),
            );
        }
        write.open_table(LINEAGE_PARENTS).insert(
            &AgentLineageId(2),
            &AgentEventPos {
                lineage_id: AgentLineageId(1),
                seq: 3,
            },
        );
        write.open_table(AGENT_HEADS).insert(
            &agent_id,
            SenValue::borrowed(&legacy_head(AgentLineageId(2), parent)),
        );
        {
            let mut story = write.open_table(AGENT_STORY);
            let rows = [
                StoryEvent::Created {
                    role: AgentRole::default(),
                    runtime_kind: RuntimeKind::Rho,
                    workdirs: Vec::new(),
                    spawned_by: AgentSpawnedBy::Direct,
                    spawn_name: None,
                    at: UnixMs(100),
                },
                StoryEvent::UserMessage {
                    text: "one".to_owned(),
                    at: UnixMs(101),
                },
                StoryEvent::TurnStarted { at: UnixMs(102) },
                StoryEvent::Wants {
                    want: AgentWant::Ask,
                    summary: Some("wants a look".to_owned()),
                    at: UnixMs(103),
                },
                StoryEvent::TurnEnded {
                    outcome: TurnOutcome::Completed,
                    at: UnixMs(104),
                },
                StoryEvent::Titled {
                    title: "a-title".to_owned(),
                    at: UnixMs(105),
                },
            ];
            for (pos, row) in rows.iter().enumerate() {
                story.insert(&story_key(agent_id, pos as u64), SenValue::borrowed(row));
            }
        }
        {
            let mut sources = write.open_table(AGENT_STORY_SOURCE);
            sources.insert(
                &story_key(agent_id, 0),
                &AgentEventPos {
                    lineage_id: AgentLineageId(1),
                    seq: 0,
                },
            );
            sources.insert(
                &story_key(agent_id, 1),
                &AgentEventPos {
                    lineage_id: AgentLineageId(1),
                    seq: 1,
                },
            );
        }
        write.commit();
        (agent_id, parent)
    }

    fn texts(events: &[AgentEvent<'static>]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Accepted(QueuedInput {
                    kind: InputKind::Message { content },
                    ..
                }) => Some(rho_core::text_content(content)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_fork_becomes_a_rewind_and_the_story_is_woven_in() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let (agent_id, parent) = legacy_store(&db).await;
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();

        let read = db.read();
        let (next, events) = read.agent_events(agent_id);
        assert!(matches!(
            events[0],
            AgentEvent::Created { parent: Some(p), .. } if p == parent
        ));
        assert_eq!(texts(&events), ["one", "two", "four"]);
        // Created, one, Turn, Wants, Turn, Presented(title), two, three,
        // Rewound, four, Presented(the head's activity).
        assert_eq!(next.pos, 11);
        let (_, all) = read.agent_event_records(agent_id);
        assert!(matches!(
            all[2].1,
            AgentEvent::Turn {
                edge: TurnEdge::Started,
                at: UnixMs(102)
            }
        ));
        let rewound = all
            .iter()
            .find(|(_, event)| matches!(event, AgentEvent::Rewound { .. }))
            .expect("the fork became a rewind");
        assert_eq!(rewound.0.pos, 8);
        assert!(matches!(rewound.1, AgentEvent::Rewound { to, .. } if to.pos == 7));
        let head = read.get_agent(agent_id);
        assert_eq!(head.generated_title.as_deref(), Some("a-title"));
        assert_eq!(head.activity.as_deref(), Some("still at it"));
        assert_eq!(head.parent, Some(parent));
        assert!(!head.turn_running);
        assert_eq!(head.last_turn_ended, Some(UnixMs(104)));
        assert_eq!(read.journal_head().0, 11);
        for table in ["agent_heads", "agent_events", "agent_story"] {
            assert!(!read.has_table(table), "{table} still there");
        }
    }

    /// The savepoint `prepare` takes puts the copy back at the old
    /// layout: run this before `proves_the_layout_on_a_copy`, it leaves
    /// the copy as it found it.
    #[tokio::test]
    #[ignore = "needs a copy of a real daemon store in RHO_PROOF_DB"]
    async fn rolls_back_to_the_savepoint_on_a_copy() {
        let path = std::env::var("RHO_PROOF_DB").expect("RHO_PROOF_DB must name a copy");
        let db = RhoDb::open(&path);
        let format = |db: &RhoDb| {
            db.read()
                .open_table(crate::db::FORMAT)
                .get(&())
                .map(|value| value.value())
        };
        assert_eq!(format(&db).as_deref(), Some("b1e40c93"));
        let heads = db.read().open_table(AGENT_HEADS).iter().count();
        let mut tables = db.read().table_names();
        tables.sort();
        let savepoints = db.write().await.persistent_savepoints();

        let started = std::time::Instant::now();
        crate::db::prepare(&db).await;
        eprintln!("savepoint and migration in {:?}", started.elapsed());
        assert_eq!(
            format(&db).as_deref(),
            Some(crate::db::CURRENT_AGENT_DB_FORMAT)
        );
        assert!(!db.read().has_table("agent_heads"));

        let started = std::time::Instant::now();
        let hop = crate::db::rollback(&db).await.expect("rollback");
        eprintln!("rollback of {hop} in {:?}", started.elapsed());
        assert_eq!(format(&db).as_deref(), Some("b1e40c93"));
        let read = db.read();
        assert_eq!(read.open_table(AGENT_HEADS).iter().count(), heads);
        let mut after = read.table_names();
        after.sort();
        assert_eq!(after, tables, "the tables are not what they were");
        drop(read);
        // Ours is gone; whatever the store held before is not ours to drop.
        assert_eq!(db.write().await.persistent_savepoints(), savepoints);
    }

    /// Migrates the copy in place the way the daemon would, savepoint and
    /// all, so `rho debug savepoints` and `rho debug rollback` can be
    /// tried on it.
    #[tokio::test]
    #[ignore = "needs a copy of a real daemon store in RHO_PROOF_DB"]
    async fn prepares_a_copy() {
        let path = std::env::var("RHO_PROOF_DB").expect("RHO_PROOF_DB must name a copy");
        let db = RhoDb::open(&path);
        let started = std::time::Instant::now();
        crate::db::prepare(&db).await;
        eprintln!("prepared in {:?}", started.elapsed());
    }

    /// What `agent_log` holds on a copy, by event kind and by the kind of
    /// context block inside `Sent`/`Replied`, with a zstd estimate.
    #[tokio::test]
    #[ignore = "needs a copy of a real daemon store in RHO_PROOF_DB"]
    async fn measures_agent_log_on_a_copy() {
        use std::collections::BTreeMap;

        use rho_core::{ContextBlock, InferenceResponseItem};
        let path = std::env::var("RHO_PROOF_DB").expect("RHO_PROOF_DB must name a copy");
        let db = RhoDb::open(&path);
        let read = db.read();
        let mut by_event: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
        let mut by_block: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
        let mut biggest: Vec<(usize, String)> = Vec::new();
        let mut raw = 0u64;
        let mut compressed = 0u64;
        let mut sample = Vec::new();
        let mut rows = 0u64;
        for (key, value) in read.open_table(super::super::AGENT_LOG).iter() {
            let value = value.value();
            let event: &AgentEvent<'_> = value.as_ref();
            let mut bytes = bytes::BytesMut::new();
            senax_encoder::Encoder::encode(event, &mut bytes).unwrap();
            rows += 1;
            raw += bytes.len() as u64;
            let kind = match event {
                AgentEvent::Created { .. } => "Created",
                AgentEvent::Accepted(_) => "Accepted",
                AgentEvent::Sent { .. } => "Sent",
                AgentEvent::Replied { .. } => "Replied",
                AgentEvent::QueueCleared => "QueueCleared",
                AgentEvent::Cleared { .. } => "Cleared",
                AgentEvent::Turn { .. } => "Turn",
                AgentEvent::Presented { .. } => "Presented",
                AgentEvent::Wants { .. } => "Wants",
                AgentEvent::Rewound { .. } => "Rewound",
                AgentEvent::Failed { .. } => "Failed",
                _ => "other",
            };
            let slot = by_event.entry(kind).or_default();
            slot.0 += 1;
            slot.1 += bytes.len() as u64;
            let blocks: &[ContextBlock] = match event {
                AgentEvent::Sent { blocks, .. } | AgentEvent::Replied { blocks, .. } => blocks,
                _ => &[],
            };
            let mut count = |kind: &'static str, size: usize| {
                let slot = by_block.entry(kind).or_default();
                slot.0 += 1;
                slot.1 += size as u64;
            };
            for block in blocks {
                let mut b = bytes::BytesMut::new();
                senax_encoder::Encoder::encode(block, &mut b).unwrap();
                match block {
                    ContextBlock::UserMessage { .. } => count("UserMessage", b.len()),
                    ContextBlock::ToolResults { .. } => count("ToolResults", b.len()),
                    ContextBlock::ToolUpdate(_) => count("ToolUpdate", b.len()),
                    ContextBlock::CompactionTrigger => count("CompactionTrigger", b.len()),
                    ContextBlock::InferenceResponse { items, .. } => {
                        for item in items {
                            let mut b = bytes::BytesMut::new();
                            senax_encoder::Encoder::encode(item, &mut b).unwrap();
                            let kind = match item {
                                InferenceResponseItem::AssistantMessage { .. } => {
                                    "AssistantMessage"
                                }
                                InferenceResponseItem::ToolCall { .. } => "ToolCall",
                                InferenceResponseItem::EncryptedReasoning { .. } => {
                                    "EncryptedReasoning"
                                }
                                InferenceResponseItem::RawReasoning { .. } => "RawReasoning",
                                InferenceResponseItem::Compaction { .. } => "Compaction",
                                InferenceResponseItem::Unknown { .. } => "Unknown",
                            };
                            count(kind, b.len());
                        }
                    }
                }
            }
            if biggest.len() < 5 || bytes.len() > biggest[4].0 {
                biggest.push((bytes.len(), format!("{:?} {kind}", key.value())));
                biggest.sort_by(|a, b| b.0.cmp(&a.0));
                biggest.truncate(5);
            }
            // zstd every 50th row on its own, as a per-row compression estimate
            if rows % 50 == 0 {
                sample.push(bytes.len() as u64);
                compressed += zstd::bulk::compress(&bytes, 3).unwrap().len() as u64;
            }
        }
        eprintln!("agent_log: {rows} rows, {raw} encoded bytes");
        for (kind, (count, bytes)) in &by_event {
            eprintln!("  event {kind:>14}: {count:>8} rows {bytes:>12} bytes");
        }
        for (kind, (count, bytes)) in &by_block {
            eprintln!("  block {kind:>18}: {count:>8} blocks {bytes:>12} bytes");
        }
        for (size, what) in &biggest {
            eprintln!("  biggest row: {size} bytes {what}");
        }
        let sampled: u64 = sample.iter().sum();
        eprintln!(
            "  zstd -3 per row on 1/50 sample: {sampled} -> {compressed} bytes ({:.1}x)",
            sampled as f64 / compressed.max(1) as f64
        );
    }

    /// `RHO_PROOF_DB` at a copy; it is never run in CI.
    #[tokio::test]
    #[ignore = "needs a copy of a real daemon store in RHO_PROOF_DB"]
    async fn proves_the_layout_on_a_copy() {
        let path = std::env::var("RHO_PROOF_DB").expect("RHO_PROOF_DB must name a copy");
        let db = RhoDb::open(&path);
        {
            let read = db.read();
            let format = read
                .open_table(crate::db::FORMAT)
                .get(&())
                .map(|value| value.value());
            eprintln!("format {format:?}; tables {:?}", read.table_names());
            assert_eq!(
                format.as_deref(),
                Some("b1e40c93"),
                "the copy is not at the layout this migration reads"
            );
        }
        let mut write = db.write().await;
        let heads = write
            .open_table(AGENT_HEADS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect::<Vec<_>>();
        let before = heads
            .iter()
            .map(|(agent_id, head)| {
                (
                    *agent_id,
                    head.clone(),
                    legacy_visible_events(&mut write, head.current_lineage),
                )
            })
            .collect::<Vec<_>>();
        let started = std::time::Instant::now();
        let report = run(&mut write);
        write.commit();
        eprintln!("{} in {:?}", report.line(), started.elapsed());

        let read = db.read();
        let mut checked = 0;
        let mut legacy_rows = 0usize;
        for (agent_id, old_head, old_events) in &before {
            let head = read.get_agent(*agent_id);
            checked += 1;
            legacy_rows += old_events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        LegacyAgentEvent::Queued(_)
                            | LegacyAgentEvent::Dequeued { .. }
                            | LegacyAgentEvent::InferenceResponse { .. }
                            | LegacyAgentEvent::ToolResult { .. }
                            | LegacyAgentEvent::PresentationUpdated { .. }
                    )
                })
                .count();
            let (_, new_events) = read.agent_events(*agent_id);
            assert!(matches!(
                new_events.first(),
                Some(AgentEvent::Created { .. })
            ));
            // The context a reader rebuilds is what the old build rebuilt
            // from its own rows; the queues match times aside (old rows
            // carried none).
            let old = legacy_events::legacy_replay(old_events.clone());
            let new = crate::agent::replay::replay(new_events);
            assert_eq!(
                new.history, old.history,
                "{agent_id:?} replays a different history"
            );
            assert_eq!(new.owed, old.owed, "{agent_id:?} owes different calls");
            assert_eq!(
                new.context_used, old.context_used,
                "{agent_id:?} context use"
            );
            let dated = |inputs: &[QueuedInput]| {
                inputs
                    .iter()
                    .cloned()
                    .map(|input| QueuedInput {
                        at: UnixMs(0),
                        ..input
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(dated(&new.user), dated(&old.user), "{agent_id:?} queue");
            assert_eq!(
                new.mail
                    .iter()
                    .map(|m| (m.sender, m.content.clone()))
                    .collect::<Vec<_>>(),
                old.mail
                    .iter()
                    .map(|m| (m.sender, m.content.clone()))
                    .collect::<Vec<_>>(),
                "{agent_id:?} mail"
            );
            if old_head.config.spawn_name.is_none() {
                assert_eq!(head.generated_title, old_head.generated_title);
            }
            assert_eq!(head.activity, old_head.activity);
            assert_eq!(head.parent, old_head.parent);
            assert_eq!(head.config.role, old_head.config.role);
            assert_eq!(head.config.spawn_name, old_head.config.spawn_name);
        }
        eprintln!(
            "proof: {checked} agents replay the same context; {legacy_rows} old-loop rows translated"
        );
    }

    /// The old lineage walk: the selected lineage's rows after the rows
    /// of every ancestor up to each fork point, creation left out.
    fn legacy_visible_events(
        write: &mut WriteTxn,
        lineage: AgentLineageId,
    ) -> Vec<LegacyAgentEvent<'static>> {
        let parents = write
            .open_table(LINEAGE_PARENTS)
            .iter()
            .map(|(key, value)| (key.value(), value.value()))
            .collect::<BTreeMap<_, _>>();
        let mut chain = vec![(lineage, u32::MAX)];
        let mut current = lineage;
        while let Some(parent) = parents.get(&current) {
            chain.push((parent.lineage_id, parent.seq));
            current = parent.lineage_id;
        }
        chain.reverse();
        let mut events = Vec::new();
        for (lineage_id, end) in chain {
            let range = AgentEventPos { lineage_id, seq: 0 }..AgentEventPos {
                lineage_id,
                seq: end,
            };
            events.extend(
                write
                    .open_table(AGENT_EVENTS)
                    .range(range)
                    .map(|(_, value)| value.value().into_owned())
                    .filter(|event| !matches!(event, LegacyAgentEvent::Created { .. })),
            );
        }
        events
    }
}
