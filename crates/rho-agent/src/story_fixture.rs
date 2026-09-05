//! A store with agents in it, for driving a rig by hand.
//!
//! The rig's daemon is the real daemon and its database is a real
//! database; what it has never had is agents, because starting one needs
//! an inference account the rig does not have. This writes them the way
//! the daemon would: `create_agent` and real story events, nothing the
//! client can tell apart from an agent that lived.
//!
//! It is a fixture, not a migration: it only ever runs against a store
//! someone points it at.

use rho_core::{AgentId, ToolName};
use rho_db::RhoDb;
use rho_inference::PromptCacheKey;
use rho_workspaces::{WorkspaceId, WorkspaceIdDomain, WorkspaceInfo};

use crate::db::{
    AgentProfileWriteTxnExt as _, AgentRole, AgentRuntime, AgentWriteTxnExt as _, SessionBinding,
    UnixMillis,
};
use crate::story::{AgentWant, StoryEvent, ToolLine, TurnOutcome};

/// Where an agent stands when the fixture leaves it. These are the states
/// Home ranks on, one each, so a rig shows the whole spread.
#[derive(Clone, Copy, Debug)]
pub enum Situation {
    /// A turn in flight.
    Working,
    /// Finished and wants a decision only the user can make.
    Asking,
    /// Finished with something to look at.
    Showing,
    /// The turn died.
    Errored,
    /// Finished, asks nothing.
    Quiet,
}

/// Writes one agent per entry, oldest first, each with a whole small
/// story. Returns their ids in the order given.
///
/// `age_minutes` spaces them apart so the ranking has something to sort
/// on: the first entry is the oldest and waits longest.
pub async fn seed(db: &RhoDb, agents: &[(&str, Situation)]) -> Vec<AgentId> {
    // A rig's database can be brand new; the tables the daemon opens on
    // start have to exist before an agent can be written into them.
    {
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
    }
    let now = rho_core::UnixMs::now();
    let mut created = Vec::new();
    for (index, (name, situation)) in agents.iter().enumerate() {
        let minutes_ago = (agents.len() - index) as u64 * 17;
        let started = rho_core::UnixMs(now.0.saturating_sub(minutes_ago * 60_000));
        created.push(seed_one(db, name, *situation, started).await);
    }
    created
}

async fn seed_one(db: &RhoDb, name: &str, situation: Situation, at: UnixMillis) -> AgentId {
    let mut write = db.write().await;
    let agent_id = write.alloc_agent_id();
    write.create_agent(
        at,
        agent_id,
        Some(name.to_owned()),
        vec![WorkspaceInfo::Workspace {
            repo: "/tmp/rho-slack-ux/proj/repo".into(),
            id: WorkspaceId::from_counter(1, &WorkspaceIdDomain(0)).expect("workspace id"),
        }],
        AgentRole::default(),
        SessionBinding::ResponsesGpt55(Default::default()),
        AgentRuntime::Rho {
            prompt_cache_key: PromptCacheKey::generate(),
        },
        None,
    );
    // A minute of work, told: the user's ask, the turn, one tool call, and
    // however the turn left things.
    let later = |minutes: u64| rho_core::UnixMs(at.0.saturating_add(minutes * 60_000));
    let mut told = vec![
        StoryEvent::UserMessage {
            text: format!("{name}: have a look"),
            at,
        },
        StoryEvent::TurnStarted { at },
        StoryEvent::ToolCall {
            name: ToolName::try_from("Read").expect("tool name"),
            what: ToolLine::Path("/tmp/rho-slack-ux/proj/repo/README.md".into()),
            at: later(1),
        },
    ];
    match situation {
        Situation::Working => told.push(StoryEvent::Activity {
            label: Some("reading the deploy log".to_owned()),
            at: later(2),
        }),
        Situation::Asking | Situation::Showing | Situation::Quiet => {
            told.push(StoryEvent::Reply {
                text: format!("{name}: done looking."),
                at: later(2),
            });
            if let Some(want) = match situation {
                Situation::Asking => Some(AgentWant::Ask),
                Situation::Showing => Some(AgentWant::Show),
                _ => None,
            } {
                told.push(StoryEvent::Wants {
                    want,
                    summary: Some(name.to_owned()),
                    at: later(2),
                });
            }
            told.push(StoryEvent::TurnEnded {
                outcome: TurnOutcome::Completed,
                at: later(2),
            });
        }
        Situation::Errored => told.push(StoryEvent::TurnEnded {
            outcome: TurnOutcome::Errored {
                message: "the deploy script exited 1".to_owned(),
            },
            at: later(2),
        }),
    }
    for event in &told {
        write.append_agent_story(agent_id, event);
    }
    write.mark_agent_story_built(agent_id);
    write.commit();
    agent_id
}

/// Tells one more turn on an agent that already exists: a reply, a want,
/// and the turn ending, all dated now. This is how a rig sees an agent
/// come back after the user has dealt with it.
pub async fn nudge(db: &RhoDb, agent_id: AgentId, want: Option<AgentWant>) {
    let now = rho_core::UnixMs::now();
    let mut write = db.write().await;
    write.append_agent_story(agent_id, &StoryEvent::TurnStarted { at: now });
    write.append_agent_story(
        agent_id,
        &StoryEvent::Reply {
            text: "one more thing".to_owned(),
            at: now,
        },
    );
    if let Some(want) = want {
        write.append_agent_story(
            agent_id,
            &StoryEvent::Wants {
                want,
                summary: Some("one more thing".to_owned()),
                at: now,
            },
        );
    }
    write.append_agent_story(
        agent_id,
        &StoryEvent::TurnEnded {
            outcome: TurnOutcome::Completed,
            at: now,
        },
    );
    write.mark_agent_story_built(agent_id);
    write.commit();
}
