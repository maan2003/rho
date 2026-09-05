//! The story, converted for the wire. `rho-ui-proto` is the light client
//! vocabulary and cannot depend on the agent runtime, so the daemon
//! translates, one variant for one variant.

use rho_agent::db::{AgentHead, AgentId, AgentRuntime, AgentSpawnedBy};
use rho_agent::story::{AgentWant, RuntimeKind, StoryEvent, ToolLine, TurnOutcome};
use rho_ui_proto::story::{
    UiAgentHead, UiAgentWant, UiRuntimeKind, UiSpawnedBy, UiStoryEvent, UiStoryPos, UiToolLine,
    UiTurnOutcome,
};

pub fn ui_story_pos(pos: rho_agent::db::StoryPos) -> UiStoryPos {
    UiStoryPos(pos.0)
}

pub fn story_pos(pos: UiStoryPos) -> rho_agent::db::StoryPos {
    rho_agent::db::StoryPos(pos.0)
}

pub fn ui_agent_head(agent_id: AgentId, head: &AgentHead) -> UiAgentHead {
    UiAgentHead {
        agent_id,
        story_pos: ui_story_pos(head.story_pos),
        role: head.config.role,
        runtime_kind: match head.config.runtime {
            AgentRuntime::Rho { .. } => UiRuntimeKind::Rho,
            AgentRuntime::Claude { .. } => UiRuntimeKind::Claude,
        },
        workdirs: head.config.workdirs.clone(),
        spawned_by: ui_spawned_by(head.config.spawned_by),
        parent: head.parent,
        spawn_name: head.config.spawn_name.clone(),
        generated_title: head.generated_title.clone(),
        activity: head.activity.clone(),
        turn_running: head.turn_running,
        created_at: head.config.created_at,
    }
}

fn ui_spawned_by(spawned_by: AgentSpawnedBy) -> UiSpawnedBy {
    match spawned_by {
        AgentSpawnedBy::Direct => UiSpawnedBy::Direct,
        AgentSpawnedBy::PM => UiSpawnedBy::PM,
        AgentSpawnedBy::Engineer => UiSpawnedBy::Engineer,
    }
}

pub fn ui_story_event(event: StoryEvent) -> UiStoryEvent {
    match event {
        StoryEvent::Created {
            role,
            runtime_kind,
            workdirs,
            spawned_by,
            spawn_name,
            at,
        } => UiStoryEvent::Created {
            role,
            runtime_kind: match runtime_kind {
                RuntimeKind::Rho => UiRuntimeKind::Rho,
                RuntimeKind::Claude => UiRuntimeKind::Claude,
            },
            workdirs,
            spawned_by: ui_spawned_by(spawned_by),
            spawn_name,
            at,
        },
        StoryEvent::Parented { parent, at } => UiStoryEvent::Parented { parent, at },
        StoryEvent::UserMessage { text, at } => UiStoryEvent::UserMessage { text, at },
        StoryEvent::AgentMail { from, text, at } => UiStoryEvent::AgentMail { from, text, at },
        StoryEvent::TurnStarted { at } => UiStoryEvent::TurnStarted { at },
        StoryEvent::TurnEnded { outcome, at } => UiStoryEvent::TurnEnded {
            outcome: match outcome {
                TurnOutcome::Completed => UiTurnOutcome::Completed,
                TurnOutcome::Cancelled => UiTurnOutcome::Cancelled,
                TurnOutcome::Errored { message } => UiTurnOutcome::Errored { message },
            },
            at,
        },
        StoryEvent::Reply { text, at } => UiStoryEvent::Reply { text, at },
        StoryEvent::ToolCall { name, what, at } => UiStoryEvent::ToolCall {
            name: name.as_str().to_owned(),
            what: match what {
                ToolLine::Path(path) => UiToolLine::Path(path),
                ToolLine::Command(command) => UiToolLine::Command(command),
                ToolLine::Query(query) => UiToolLine::Query(query),
                ToolLine::Agent(agent_id) => UiToolLine::Agent(agent_id),
                ToolLine::Nothing => UiToolLine::Nothing,
            },
            at,
        },
        StoryEvent::Wants { want, summary, at } => UiStoryEvent::Wants {
            want: match want {
                AgentWant::Show => UiAgentWant::Show,
                AgentWant::Ask => UiAgentWant::Ask,
                AgentWant::Answer => UiAgentWant::Answer,
            },
            summary,
            at,
        },
        StoryEvent::Titled { title, at } => UiStoryEvent::Titled { title, at },
        StoryEvent::Activity { label, at } => UiStoryEvent::Activity { label, at },
        StoryEvent::Cost { usage, at } => UiStoryEvent::Cost {
            usage: crate::ui_agent_usage_bucket(usage),
            at,
        },
        StoryEvent::Rewound { to, at } => UiStoryEvent::Rewound {
            to: ui_story_pos(to),
            at,
        },
        StoryEvent::Compacted { at } => UiStoryEvent::Compacted { at },
        StoryEvent::RoleChanged { role, at } => UiStoryEvent::RoleChanged { role, at },
        StoryEvent::WorkdirAdded { workdir, at } => UiStoryEvent::WorkdirAdded { workdir, at },
        StoryEvent::HistoryUnavailableBefore { at } => {
            UiStoryEvent::HistoryUnavailableBefore { at }
        }
    }
}
