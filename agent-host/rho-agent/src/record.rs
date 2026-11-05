//! The workset log: one append-only log per workset, keyed by
//! `(WorksetId, Wpos)` ([ARCH-agent-host](../../specs/ARCH-agent-host.md)).
//!
//! A record is its time and one event. Events about an agent carry its id;
//! the others are about the workset. Agent and workset heads are projections
//! of the log, committed with each append.

use std::borrow::Cow;

use rho_agent_types::transcript::{ExecCall, ExecId, PendingInferenceResponse};
use rho_agent_types::{
    AgentId, ContentPart, ExecMilestone, MessageId, Props, Role, UnixMs, WorksetKind, Wpos,
};
use senax_encoder::{Decode, Encode};
use uuid::Uuid;

use crate::inference::PromptCacheKey;
use crate::inference::config::{InferenceModel, InferenceProfile};
use crate::log::{ClaudeOutputBatch, ClaudeRewind, TranscriptLine, WakeFacts};

/// One row of a workset log.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Record<'a> {
    pub at: UnixMs,
    pub event: Event<'a>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Event<'a> {
    /// The first row of every workset: what it is, and the code the GUI
    /// created it with, which runs once.
    WorksetCreated { kind: WorksetKind, code: String },
    /// Code the GUI evaluated in the workset namespace.
    Evaluated { code: String },

    /// Python created an agent.
    AgentCreated { agent: AgentId, spec: AgentSpec },
    /// The agent's whole spec, replacing the one before.
    AgentChanged { agent: AgentId, spec: AgentSpec },

    /// Something entered the agent's queue: delivered by the agent's next
    /// request that can carry its role, and by every such request after a
    /// position before it. Written by the agent's own loop, so its order
    /// against that loop's requests is true.
    Input {
        agent: AgentId,
        /// The message it hands over, if it is one.
        message: Option<MessageId>,
        content: Vec<ContentPart>,
        #[senax(rename = "wake")]
        role: Role,
    },

    /// Every row of this agent from `to` up to here is no longer its history.
    Rewound { agent: AgentId, to: Wpos },
    /// A request failed with this much of a response in. Never history.
    Failed {
        agent: AgentId,
        partial: PendingInferenceResponse,
        error: Cow<'a, str>,
        retrying: bool,
    },
    /// A provider or host lifecycle observation, for presentation only.
    ExecObserved {
        agent: AgentId,
        exec: ExecId,
        milestone: ExecMilestone,
    },
    /// One of the native runtime's own rows.
    Native {
        agent: AgentId,
        entry: crate::entry::Entry,
    },
    /// One of the Claude runtime's own rows.
    Claude { agent: AgentId, row: ClaudeRow },
}

impl Event<'_> {
    /// The agent the event is about; `None` for a workset event.
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

/// What an agent is: the loop that runs it, and what Python says about it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentSpec {
    pub runtime: Runtime,
    pub props: Props,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Runtime {
    Native {
        model: InferenceModel,
        profile: InferenceProfile,
        cache_key: PromptCacheKey,
    },
    /// Claude Code, started in `/src`.
    Claude {
        model: rho_claude::Model,
        effort: rho_claude::Effort,
        session_id: Uuid,
    },
}

impl Runtime {
    /// The model by the name policy Python gives it.
    pub fn model_name(&self) -> &'static str {
        match self {
            Self::Native { model, .. } => match model {
                InferenceModel::Gpt61Sol => "sol",
                InferenceModel::Gpt61Terra => "terra",
                InferenceModel::Gpt6Astra => "astra",
            },
            Self::Claude { model, .. } => match model {
                rho_claude::Model::Fable => "fable",
                rho_claude::Model::Opus => "opus",
                rho_claude::Model::Sonnet => "sonnet",
            },
        }
    }

    /// The reasoning effort by the name policy Python gives it.
    pub fn effort_name(&self) -> &'static str {
        match self {
            Self::Native { profile, .. } => profile.effort.as_str(),
            Self::Claude { effort, .. } => match effort {
                rho_claude::Effort::Low => "low",
                rho_claude::Effort::Medium => "medium",
                rho_claude::Effort::High => "high",
                rho_claude::Effort::Xhigh => "xhigh",
                rho_claude::Effort::Max => "max",
            },
        }
    }
}

/// The Claude runtime's rows.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ClaudeRow {
    /// One line of the conversation, as the stream told it.
    Transcript {
        uuid: Uuid,
        line: TranscriptLine,
        wake: Option<WakeFacts>,
    },
    /// Rho's permission to execute a cell, committed before it runs.
    ExecAdmitted {
        call: ExecCall,
    },
    /// Notebook output owned by the host before Claude Code is contacted.
    Output {
        batch: ClaudeOutputBatch,
    },
    OutputHandedOff {
        id: Uuid,
    },
    /// A rewind whose destination transcript is not verified yet; `None`
    /// withdraws it. On success the new session is an `AgentChanged`.
    RewindPending(Option<ClaudeRewind>),
    /// A turn or an exec result handed to Claude Code. It delivers every
    /// input logged before it, except `tool` inputs when `no_call`: only an
    /// exec result carries them
    /// ([DECISION-roles](../specs/DECISION-roles.md)).
    RequestSent {
        why: crate::entry::Wake,
        report: crate::entry::Report,
        #[senax(default)]
        no_call: bool,
    },
    Notice(crate::entry::Notice),
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentId, AgentIdDomain, ContentPart, Role};
    use senax_encoder::{decode, encode};

    use super::*;

    /// Rows as they were written before the roles.
    mod before_roles {
        use rho_agent_types::{AgentId, ContentPart, MessageId};
        use senax_encoder::Encode;

        #[derive(Encode)]
        pub enum ClaudeRow {
            RequestSent {
                why: crate::entry::Wake,
                report: crate::entry::Report,
            },
        }

        #[derive(Encode)]
        pub enum InputWake {
            Person,
            Agent,
            Quiet,
        }

        #[derive(Encode)]
        pub enum Event {
            Input {
                agent: AgentId,
                message: Option<MessageId>,
                content: Vec<ContentPart>,
                wake: InputWake,
            },
        }
    }

    #[test]
    fn a_request_row_from_before_the_roles_delivered_everything() {
        let bytes = encode(&before_roles::ClaudeRow::RequestSent {
            why: crate::entry::Wake::Message,
            report: crate::entry::Report::default(),
        })
        .unwrap();
        let row: ClaudeRow = decode(&mut bytes.as_ref()).unwrap();
        assert_eq!(
            row,
            ClaudeRow::RequestSent {
                why: crate::entry::Wake::Message,
                report: crate::entry::Report::default(),
                no_call: false,
            }
        );
    }

    #[test]
    fn an_input_row_from_before_the_roles_keeps_who_spoke() {
        let agent = AgentId::from_counter(1, &AgentIdDomain(7)).unwrap();
        let content = vec![ContentPart::Text { text: "hi".into() }];
        for (wake, role) in [
            (before_roles::InputWake::Person, Role::User),
            (before_roles::InputWake::Agent, Role::Tool),
            (before_roles::InputWake::Quiet, Role::System),
        ] {
            let bytes = encode(&before_roles::Event::Input {
                agent,
                message: None,
                content: content.clone(),
                wake,
            })
            .unwrap();
            let event: Event<'static> = decode(&mut bytes.as_ref()).unwrap();
            assert!(
                matches!(event, Event::Input { role: decoded, .. } if decoded == role),
                "{event:?}"
            );
        }
    }
}
