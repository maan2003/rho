//! Dedicated native-client realtime stream vocabulary.
//!
//! The stream first performs OAuth signaling, then carries generic delegated
//! agent work selected by the GUI. Provider events never cross this boundary.

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::AgentId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct RealtimeRequestId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum RealtimeClientFrame {
    Delegate {
        request_id: RealtimeRequestId,
        agent_id: AgentId,
        text: String,
    },
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum RealtimeServerFrame {
    Delegated {
        request_id: RealtimeRequestId,
        text: String,
    },
    Error(String),
    Closed,
}
