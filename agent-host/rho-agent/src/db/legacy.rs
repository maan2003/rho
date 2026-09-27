//! Temporary decoder for pre-code-first native rows. No runtime writes these.
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

impl crate::AgentEvent<'_> {
    #[cfg(test)]
    pub fn native_event(&self) -> Option<&NativeEvent> {
        match self {
            Self::Native(event) => Some(event),
            _ => None,
        }
    }
}
