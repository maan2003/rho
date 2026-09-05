//! The once-only pass that turns every `AgentRecord` into a `Created`
//! event at the front of its log, a head, and the daemon's leftover
//! opinions. Deleted in the landing after the user has restarted on it
//! (`AGENT-LOG-DESIGN.md`).

use redb::TableDefinition;
use rho_db::{RecordedTypeName, SenAs, SenValue, WriteTxn};
use rho_workspaces::WorkspaceInfo;
use senax_encoder::{Decode, Encode};

use super::{
    AGENT_ATTENTION, AGENT_EVENTS, AGENT_HEADS, AgentAttention, AgentConfig, AgentDisposition,
    AgentEventPos, AgentHead, AgentId, AgentLineageId, AgentRole, AgentRuntime, AgentSpawnedBy,
    ClaudeRewind, CounterKey, LINEAGE_PARENTS, SessionBinding, StoryPos, TurnReport, UnixMillis,
    next_counter,
};
use crate::AgentEvent;

const AGENTS: TableDefinition<AgentId, SenAs<AgentRecord, AgentRecordName>> =
    TableDefinition::new("agents");

/// The record was written from `rho_agent::db`, and redb checks the name
/// it was written under; this file is a different module.
#[derive(Debug)]
struct AgentRecordName;

impl RecordedTypeName for AgentRecordName {
    const NAME: &'static str = "rho-db::Sen<rho_agent::db::AgentRecord>";
}

/// The record as it was last written. Only this file decodes it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct AgentRecord {
    display_name: Option<String>,
    #[senax(default)]
    generated_title: Option<String>,
    #[senax(default)]
    activity: Option<String>,
    workdirs: Vec<WorkspaceInfo>,
    created_at: UnixMillis,
    updated_at: UnixMillis,
    current_lineage: AgentLineageId,
    parent_agent: Option<AgentId>,
    spawned_by: AgentSpawnedBy,
    role: AgentRole,
    binding: SessionBinding,
    runtime: AgentRuntime,
    #[senax(default)]
    claude_rewind: Option<ClaudeRewind>,
    #[senax(default)]
    last_user_message: UnixMillis,
    #[senax(default)]
    last_turn_ended: Option<UnixMillis>,
    #[senax(default)]
    last_user_message_text: String,
    #[senax(default)]
    labels: Vec<String>,
    #[senax(default)]
    disposition: AgentDisposition,
    #[senax(default)]
    turn_report: Option<TurnReport>,
    #[senax(default)]
    user_interacted: bool,
}

/// What the pass found, for the report the user is shown before restarting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub agents: usize,
    pub spawn_names: usize,
    pub claude_runtimes: usize,
    pub pending_rewinds: usize,
}

impl MigrationReport {
    pub fn line(&self) -> String {
        format!(
            "agent records to logs: {} agents, {} spawn names, {} Claude runtimes, \
             {} pending rewinds",
            self.agents, self.spawn_names, self.claude_runtimes, self.pending_rewinds
        )
    }
}

pub(super) fn migrate(write: &mut WriteTxn) {
    let report = run(write);
    eprintln!("{}", report.line());
}

/// Every record becomes a `Created` event ahead of that agent's existing
/// events, a head, and one row of leftovers. Returns what it saw.
pub fn run(write: &mut WriteTxn) -> MigrationReport {
    let records = write
        .open_table(AGENTS)
        .iter()
        .map(|(key, value)| (key.value(), value.value().into_owned()))
        .collect::<Vec<_>>();
    let mut report = MigrationReport::default();
    for (agent_id, record) in records {
        report.agents += 1;
        report.spawn_names += usize::from(record.display_name.is_some());
        report.claude_runtimes +=
            usize::from(matches!(record.runtime, AgentRuntime::Claude { .. }));
        report.pending_rewinds += usize::from(record.claude_rewind.is_some());

        // A new lineage holding only the creation event, spliced in under
        // the agent's oldest one: the replay walks parents to the root and
        // plays the segments oldest first, so `Created` comes out ahead of
        // every existing event without moving one of them.
        let lineage_id = AgentLineageId(next_counter(write, CounterKey::LAST_LINEAGE_ID));
        let created = AgentEvent::Created {
            role: record.role,
            binding: record.binding,
            runtime: record.runtime.clone(),
            workdirs: record.workdirs.clone(),
            spawned_by: record.spawned_by,
            spawn_name: record.display_name.clone(),
            created_at: record.created_at,
        };
        write.open_table(AGENT_EVENTS).insert(
            &AgentEventPos::root(lineage_id),
            SenValue::borrowed(&created),
        );
        let root = root_lineage(write, record.current_lineage);
        write
            .open_table(LINEAGE_PARENTS)
            .insert(&root, &AgentEventPos { lineage_id, seq: 1 });

        write.open_table(AGENT_HEADS).insert(
            &agent_id,
            SenValue::borrowed(&AgentHead {
                config: AgentConfig {
                    role: record.role,
                    binding: record.binding,
                    runtime: record.runtime,
                    workdirs: record.workdirs,
                    spawned_by: record.spawned_by,
                    spawn_name: record.display_name,
                    created_at: record.created_at,
                    claude_rewind: record.claude_rewind,
                },
                story_pos: StoryPos::default(),
                generated_title: record.generated_title,
                activity: record.activity,
                current_lineage: record.current_lineage,
            }),
        );
        write.open_table(AGENT_ATTENTION).insert(
            &agent_id,
            SenValue::borrowed(&AgentAttention {
                updated_at: record.updated_at,
                parent_agent: record.parent_agent,
                last_user_message: record.last_user_message,
                last_user_message_text: record.last_user_message_text,
                last_turn_ended: record.last_turn_ended,
                disposition: record.disposition,
                turn_report: record.turn_report,
                user_interacted: record.user_interacted,
            }),
        );
    }
    // Labels and the parent edge are the store's facts already. The
    // project list and the view config stay until the GUI has converted
    // them in slice B: they are the user's own data and nothing else
    // holds them yet.
    write.delete_table("agents");
    report
}

/// The oldest lineage the agent's chain reaches: the one with no parent.
fn root_lineage(write: &mut WriteTxn, current: AgentLineageId) -> AgentLineageId {
    let parents = write.open_table(LINEAGE_PARENTS);
    let mut lineage_id = current;
    while let Some(parent) = parents.get(&lineage_id) {
        lineage_id = parent.value().lineage_id;
    }
    lineage_id
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rho_core::{ContentPart, MessageDelivery, MessageSender, UnixMs};
    use rho_db::RhoDb;
    use rho_inference::PromptCacheKey;
    use rho_workspaces::{WorkspaceId, WorkspaceIdDomain, WorkspaceInfo};

    use super::*;
    use crate::db::{AgentReadTxnExt, AgentWriteTxnExt, InferenceProfile};
    use crate::{QueuedItem, QueuedItemKind};

    fn user_event(text: &str) -> AgentEvent<'static> {
        AgentEvent::Queued(QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender: MessageSender::User,
                content: Arc::new(vec![ContentPart::Text {
                    text: text.to_owned(),
                }]),
                source_id: None,
            },
            delivery: MessageDelivery::Immediate,
        })
    }

    fn workspace() -> WorkspaceInfo {
        WorkspaceInfo::Workspace {
            repo: "/home/user/src/rho".into(),
            id: WorkspaceId::from_counter(1, &WorkspaceIdDomain(0)).unwrap(),
        }
    }

    fn legacy_record(current_lineage: AgentLineageId, name: Option<&str>) -> AgentRecord {
        AgentRecord {
            display_name: name.map(str::to_owned),
            generated_title: Some("a title".to_owned()),
            activity: Some("reading".to_owned()),
            workdirs: vec![workspace()],
            created_at: UnixMs(1),
            updated_at: UnixMs(9),
            current_lineage,
            parent_agent: None,
            spawned_by: AgentSpawnedBy::Direct,
            role: AgentRole::PM,
            binding: SessionBinding::ResponsesGpt55(InferenceProfile::default()),
            runtime: AgentRuntime::Rho {
                prompt_cache_key: PromptCacheKey::generate(),
            },
            claude_rewind: None,
            last_user_message: UnixMs(5),
            last_turn_ended: Some(UnixMs(7)),
            last_user_message_text: "do the thing".to_owned(),
            labels: vec!["pin".to_owned()],
            disposition: AgentDisposition::Pending,
            turn_report: None,
            user_interacted: true,
        }
    }

    /// The point of the whole pass: creation comes out ahead of events that
    /// were written before it existed, and none of them move.
    #[tokio::test]
    async fn creation_leads_the_log_and_the_head_is_the_record() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent_id = write.alloc_agent_id();

        // An agent as the old build left it: a lineage of events, a fork,
        // and a record pointing at the fork.
        let first = AgentLineageId(super::next_counter(
            &mut write,
            super::CounterKey::LAST_LINEAGE_ID,
        ));
        let root = AgentEventPos::root(first);
        let second = write.append_agent_event(root, &user_event("one"));
        write.append_agent_event(second, &user_event("stale"));
        let forked = AgentLineageId(super::next_counter(
            &mut write,
            super::CounterKey::LAST_LINEAGE_ID,
        ));
        write.open_table(LINEAGE_PARENTS).insert(&forked, &second);
        write.append_agent_event(AgentEventPos::root(forked), &user_event("two"));
        write.open_table(AGENTS).insert(
            &agent_id,
            SenValue::borrowed(&legacy_record(forked, Some("main"))),
        );

        let report = run(&mut write);
        write.commit();

        assert_eq!(report.agents, 1);
        assert_eq!(report.spawn_names, 1);
        assert_eq!(report.claude_runtimes, 0);
        assert_eq!(report.pending_rewinds, 0);

        let read = db.read();
        let (_, events) = read.agent_events(agent_id);
        assert!(matches!(events[0], AgentEvent::Created { .. }));
        assert_eq!(events.len(), 3);
        assert_eq!(events[1], user_event("one"));
        assert_eq!(events[2], user_event("two"));

        let head = read.get_agent(agent_id);
        assert_eq!(head.config.spawn_name.as_deref(), Some("main"));
        assert_eq!(head.config.role, AgentRole::PM);
        assert_eq!(head.generated_title.as_deref(), Some("a title"));
        assert_eq!(head.current_lineage, forked);
        let attention = read.agent_attention(agent_id);
        assert_eq!(attention.last_user_message_text, "do the thing");
        assert_eq!(attention.last_turn_ended, Some(UnixMs(7)));
        assert!(attention.user_interacted);
    }

    /// The head is a cache: made again from the log, it says the same.
    #[tokio::test]
    async fn the_head_rebuilds_from_the_log() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent_id = write.alloc_agent_id();
        let lineage = AgentLineageId(super::next_counter(
            &mut write,
            super::CounterKey::LAST_LINEAGE_ID,
        ));
        write.append_agent_event(AgentEventPos::root(lineage), &user_event("one"));
        write
            .open_table(AGENTS)
            .insert(&agent_id, SenValue::borrowed(&legacy_record(lineage, None)));
        run(&mut write);
        write.set_agent_role(agent_id, AgentRole::Iris);
        let migrated = write.rebuild_agent_head(agent_id);
        write.commit();

        assert_eq!(migrated.config.role, AgentRole::Iris);
        assert_eq!(db.read().get_agent(agent_id), migrated);
    }

    /// The proof the user's own store gets before they restart on this
    /// build: run the pass on a read-only copy and check that every
    /// agent's log comes out as its creation followed by exactly the
    /// events it already had, in the order it had them. Point
    /// `RHO_PROOF_DB` at a copy; it is never run in CI.
    #[tokio::test]
    #[ignore = "needs a copy of a real daemon store in RHO_PROOF_DB"]
    async fn proves_the_ordering_on_a_copy() {
        let path = std::env::var("RHO_PROOF_DB").expect("RHO_PROOF_DB must name a copy");
        let db = RhoDb::open(&path);
        let mut write = db.write().await;

        // What every agent's log looks like before the pass, read the way
        // the old build read it.
        let records = write
            .open_table(AGENTS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect::<Vec<_>>();
        let before = records
            .into_iter()
            .map(|(agent_id, record)| {
                let events = super::super::agent_events_write(&mut write, record.current_lineage);
                (agent_id, events)
            })
            .collect::<Vec<_>>();

        let report = run(&mut write);
        write.commit();

        let read = db.read();
        for (agent_id, old_events) in &before {
            let (_, new_events) = read.agent_events(*agent_id);
            assert!(
                matches!(new_events.first(), Some(AgentEvent::Created { .. })),
                "{agent_id:?} does not begin with its creation"
            );
            assert_eq!(
                &new_events[1..],
                old_events.as_slice(),
                "{agent_id:?} lost or reordered an event"
            );
            let head = read.get_agent(*agent_id);
            assert_eq!(head.config.workdirs.is_empty(), false);
        }
        assert_eq!(report.agents, before.len());
        eprintln!("{}", report.line());
        eprintln!("proof: {} agents replayed unchanged", before.len());
    }
}
