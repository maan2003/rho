//! The workset log: one append-only log per workset, keyed by
//! `(WorksetId, Wpos)` ([ARCH-agent-host](../../specs/ARCH-agent-host.md)).
//!
//! A record is its time and one event. Events about an agent carry its id;
//! the others are about the workset. Agent and workset heads are projections
//! of the log, committed with each append.

use std::borrow::Cow;

use rho_agent_types::transcript::ExecId;
use rho_agent_types::{
    AgentId, CallTiming, ContentPart, MessageId, Props, Role, UnixMs, WorksetKind, Wpos,
};
use senax_encoder::{Decode, Encode};
use uuid::Uuid;

use crate::entry::{Notice, RequestNotice, ResponseUsage};
use crate::inference::config::{InferenceModel, InferenceProfile};
use crate::inference::{Carry, PromptCacheKey};
use crate::log::{AgentUsageBucket, ClaudeRewind, TranscriptCall};

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

    /// A request to the model, and everything it carried. In a Claude agent
    /// it is handed to Claude Code; a native agent replays its context from
    /// these rows.
    Request {
        agent: AgentId,
        /// Notebook output: the first thing the open call's result says.
        output: Vec<ContentPart>,
        inputs: Vec<Input>,
        notices: Vec<RequestNotice>,
        /// A native request asks the provider to compact. Claude Code
        /// compacts by a command of its own.
        compact: bool,
        /// The Claude Code session line of the text this request handed over,
        /// where a rewind forks. Rho names it when it sends.
        turn: Option<Uuid>,
        /// How the call this request answers went.
        timing: Option<CallTiming>,
    },

    /// Every row of this agent from `to` up to here is no longer its history.
    Rewound { agent: AgentId, to: Wpos },
    /// A request failed with this much of a response in. Never history.
    Failed {
        agent: AgentId,
        /// What the model had said.
        partial: String,
        error: Cow<'a, str>,
        retrying: bool,
    },
    /// Something that happened to the agent, as the transcript says it.
    Notice { agent: AgentId, notice: Notice },

    /// A native agent's model replied.
    Step {
        agent: AgentId,
        exec: Option<String>,
        prose: String,
        carry: Carry,
        /// Complete billable usage, absent for interrupted output.
        usage: Option<ResponseUsage>,
    },
    /// A native agent must compact; a `Request` records when an attempt picks
    /// it up.
    CompactionTrigger { agent: AgentId, manual: bool },

    /// A Claude agent's model spoke or called. One row per content block, so a
    /// text and the call after it are two rows. Usage rides on the first row
    /// of a message only, so a reader counts each request once.
    ClaudeReply {
        agent: AgentId,
        text: String,
        calls: Vec<TranscriptCall>,
        usage: Option<AgentUsageBucket>,
        /// Context-window occupancy after this message.
        context_used: Option<u64>,
    },
    /// What calls came back with when no `Request` answered them: Claude
    /// Code's own tools, a call Rho refused, or one that was cut off.
    ClaudeResults {
        agent: AgentId,
        results: Vec<CallResult>,
    },
    /// Claude compacted the context here.
    ClaudeCompacted {
        agent: AgentId,
        context_used: Option<u64>,
    },
    /// A rewind whose destination transcript is not verified yet; `None`
    /// withdraws it. On success the new session is an `AgentChanged`.
    RewindPending {
        agent: AgentId,
        rewind: Option<ClaudeRewind>,
    },
}

impl Event<'_> {
    /// A request that carried something a person typed: where a rewind can go.
    pub fn carries_a_person(&self) -> bool {
        matches!(self, Self::Request { inputs, .. }
            if inputs.iter().any(|input| input.role == Role::User))
    }

    /// The agent the event is about; `None` for a workset event.
    pub fn agent(&self) -> Option<AgentId> {
        match self {
            Self::WorksetCreated { .. } | Self::Evaluated { .. } => None,
            Self::AgentCreated { agent, .. }
            | Self::AgentChanged { agent, .. }
            | Self::Request { agent, .. }
            | Self::Rewound { agent, .. }
            | Self::Failed { agent, .. }
            | Self::Notice { agent, .. }
            | Self::Step { agent, .. }
            | Self::CompactionTrigger { agent, .. }
            | Self::ClaudeReply { agent, .. }
            | Self::ClaudeResults { agent, .. }
            | Self::ClaudeCompacted { agent, .. }
            | Self::RewindPending { agent, .. } => Some(*agent),
        }
    }
}

/// One thing a request carried, and how the model reads it
/// ([DECISION-roles](../specs/DECISION-roles.md)).
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Input {
    pub role: Role,
    /// The message it hands over, if it is one.
    pub message: Option<MessageId>,
    pub content: Vec<ContentPart>,
}

/// What a call came back with.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct CallResult {
    pub call: ExecId,
    pub output: String,
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

#[cfg(test)]
impl Event<'static> {
    /// A request that carries these inputs and says nothing else.
    pub(crate) fn carrying(agent: AgentId, inputs: Vec<Input>) -> Self {
        Self::Request {
            agent,
            output: Vec::new(),
            inputs,
            notices: Vec::new(),
            compact: false,
            turn: None,
            timing: None,
        }
    }
}
