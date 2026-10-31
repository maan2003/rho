//! The workset log: one append-only log per workset, keyed by
//! `(WorksetId, Wpos)` ([ARCH-rho2](../../specs/ARCH-rho2.md)).
//!
//! A record is its time and one event. Events about an agent carry its id;
//! the others are about the workset. Agent and workset heads are projections
//! of the log, committed with each append.

use std::borrow::Cow;

use rho_agent_types::transcript::{ExecCall, ExecId, PendingInferenceResponse};
use rho_agent_types::{
    AgentId, ContentPart, ExecMilestone, InputWake, MailId, Props, UnixMs, WorksetKind, Wpos,
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
    /// request, and by every request after a position before it. Written
    /// by the agent's own loop, so its order against that loop's requests
    /// is true.
    Input {
        agent: AgentId,
        /// The mail it hands over, if it is one.
        mail: Option<MailId>,
        content: Vec<ContentPart>,
        wake: InputWake,
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
    /// A turn handed to Claude Code. Like the native one, it delivers every
    /// input of the agent logged before it.
    RequestSent {
        why: crate::entry::Wake,
        report: crate::entry::Report,
    },
    Notice(crate::entry::Notice),
}
