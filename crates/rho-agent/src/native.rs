//! Native conversation authority. Requests and responses commit ordered,
//! grouped context entries directly. A response ID always belongs to its own
//! response entry, never to the containing event. Provider input is disposable.
use rho_agent_types::UnixMs;
use rho_inference::types::{ContextBlock, PendingInferenceResponse};
use senax_encoder::{Decode, Encode};

use crate::{ContextChange, WakeFacts};

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum NativeEvent {
    RequestStarted {
        input: Vec<ContextBlock>,
        context: Option<ContextChange>,
        wake: Option<WakeFacts>,
        at: UnixMs,
    },
    ResponseFinished {
        /// Live completion commits one InferenceResponse entry; migrated
        /// records preserve all original entries and response boundaries.
        output: Vec<ContextBlock>,
        context_used: Option<u64>,
        usage: Option<crate::db::AgentUsageBucket>,
        at: UnixMs,
    },
    RequestFailed {
        partial: PendingInferenceResponse,
        error: String,
        retrying: bool,
        at: UnixMs,
    },
}

impl NativeEvent {
    pub(crate) fn blocks(&self) -> &[ContextBlock] {
        match self {
            Self::RequestStarted { input, .. } => input,
            Self::ResponseFinished { output, .. } => output,
            Self::RequestFailed { .. } => &[],
        }
    }
}

impl crate::AgentEvent<'_> {
    pub fn native_event(&self) -> Option<&NativeEvent> {
        match self {
            Self::Native(event) => Some(event),
            _ => None,
        }
    }
}
