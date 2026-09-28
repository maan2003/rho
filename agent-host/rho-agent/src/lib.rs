//! Agents across two processes: host lifecycle and services, worker execution,
//! and their private IPC. `log` owns durable data types; `db` owns storage.
//!
//! The crate root exposes shared live state and public entry points.

use std::sync::Arc;

use rho_agent_types::transcript::ToolSpec;
use senax_encoder::{Decode, Encode};

pub mod host;
pub mod inference;
mod ipc;
pub use worker::native::{AgentHandle, render_agent_surface};

pub mod db;
pub mod entry;
pub mod journal;
pub mod log;
pub mod multi_agent_tools;
mod papercut;
pub mod prompt;
#[cfg(test)]
mod testing;
mod title;
pub mod worker;
pub use host::{Process as WorksetProcess, WorksetClient};
pub use ipc::workset::{Action as WorksetAction, Attach as WorksetAttach, Reply as WorksetReply};
pub use worker::worker_main;

/// Model-facing prompt and top-level tools for a newly created role. Dynamic
/// agent identity/team text and stateful integration hosts are omitted.
pub struct RenderedAgentSurface {
    pub system_prompt: Arc<str>,
    pub tools: Arc<[ToolSpec]>,
}

pub use log::{
    AgentEvent, ClaudeOutputBatch, InputKind, QueuedInput, RuntimeChange, TranscriptCall,
    TranscriptLine, WakeEvent, WakeFacts, WakeKind, WakeTrigger,
};
pub use rho_agents_client::protocol::transcript::{
    InferenceState, RuntimeState, StreamingResponse,
};

/// Replaceable live state. Neither runtime occupancy nor partial responses
/// belong in durable history.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct AgentStatus {
    pub runtime: RuntimeState,
    pub response: Option<StreamingResponse>,
    /// Tentative outgoing text, retained across provider completion until
    /// its originating cell sends or the cell is replaced.
    #[senax(default)]
    pub draft: Option<String>,
    /// Inputs waiting to enter model context.
    pub queued: usize,
}

impl AgentStatus {
    /// The serialized retirement fence still checks the worker's own state.
    pub fn settled(&self) -> bool {
        !self.runtime.is_working() && self.queued == 0
    }
}

/// Pending inputs in arrival order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputQueues {
    items: Vec<QueuedInput>,
}

impl InputQueues {
    pub fn push(&mut self, item: QueuedInput) {
        self.items.push(item);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Pending items in arrival order, for rendering.
    pub fn iter(&self) -> impl Iterator<Item = &QueuedInput> {
        self.items.iter()
    }

    /// Remove the first pending item matching `pred`.
    pub fn remove_first(&mut self, pred: impl FnMut(&QueuedInput) -> bool) -> Option<QueuedInput> {
        let pos = self.items.iter().position(pred)?;
        Some(self.items.remove(pos))
    }

    pub fn retain(&mut self, pred: impl FnMut(&QueuedInput) -> bool) {
        self.items.retain(pred);
    }
}

pub use host::pool::StartPlace;
