//! Dedicated native-client realtime stream vocabulary.
//!
//! The stream performs OAuth signaling and publishes client-local semantic
//! context. Provider control events stay on the daemon's OpenAI sideband.

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// The answer to [`crate::host::Open::Realtime`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Opened {
    Answer {
        answer_sdp: String,
    },
    /// The host closes the stream after sending it.
    Refused {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum RealtimeClientFrame {
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum RealtimeServerFrame {
    SidebandReady,
    Error(String),
    Closed,
}
