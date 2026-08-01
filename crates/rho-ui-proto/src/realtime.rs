//! Dedicated native-client realtime stream vocabulary.
//!
//! The stream first performs OAuth signaling, then carries generic delegated
//! Iris control work plus the GUI context captured for the utterance.
//! Provider events never cross this boundary.

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::AgentId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct RealtimeRequestId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum RealtimeResponsePhase {
    Commentary,
    Speakable,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum RealtimeClientFrame {
    Delegate {
        request_id: RealtimeRequestId,
        /// Agent selected by the GUI when the utterance completed, when any.
        /// Iris remains global; this is deictic context, not a routing target.
        context_agent: Option<AgentId>,
        text: String,
        /// Active role-bearing realtime transcript when this handoff was made.
        transcript_delta: String,
    },
    /// User transcript left after the final handoff when the voice session
    /// ends.
    TranscriptTail {
        context_agent: Option<AgentId>,
        text: String,
    },
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum RealtimeServerFrame {
    /// Iris output produced without a corresponding provider delegation.
    StandaloneItem {
        phase: RealtimeResponsePhase,
        text: String,
    },
    DelegatedItem {
        request_id: RealtimeRequestId,
        phase: RealtimeResponsePhase,
        text: String,
    },
    Delegated {
        request_id: RealtimeRequestId,
        text: String,
    },
    Steered {
        request_id: RealtimeRequestId,
    },
    Error(String),
    Closed,
}
