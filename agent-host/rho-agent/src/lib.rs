//! Agents across two processes: host lifecycle and services, worker execution,
//! and their private IPC. `log` owns durable data types; `db` owns storage.
//!
//! The crate root exposes shared live state and public entry points.

use senax_encoder::{Decode, Encode};

pub mod host;
pub mod inference;
mod ipc;
pub use worker::native::AgentHandle;

pub mod db;
pub mod entry;
pub mod heap;
pub mod log;
mod papercut;
pub mod record;
pub mod store;
#[cfg(test)]
mod testing;
pub mod worker;
pub use host::{Process as WorksetProcess, WorksetClient};
pub use ipc::workset::{Action as WorksetAction, Attach as WorksetAttach, Reply as WorksetReply};
pub use log::TranscriptCall;
pub use rho_agents_client::protocol::transcript::{
    InferenceState, RuntimeState, StreamingResponse,
};
pub use worker::worker_main;

/// Replaceable live state. Neither runtime occupancy nor partial responses
/// belong in durable history.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct AgentStatus {
    pub runtime: RuntimeState,
    pub response: Option<StreamingResponse>,
    /// Tentative outgoing text, retained across provider completion until
    /// its originating cell sends or the cell is replaced.
    #[senax(default)]
    pub draft: Option<rho_agents_client::protocol::transcript::Draft>,
    /// Inputs waiting to enter model context.
    pub queued: usize,
}

impl AgentStatus {
    /// The serialized retirement fence still checks the worker's own state.
    pub fn settled(&self) -> bool {
        !self.runtime.is_working() && self.queued == 0
    }
}
