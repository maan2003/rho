//! The voice part of a host, [`rho_rpc::parts::Part::Voice`].
//!
//! The stream performs OAuth signaling and publishes client-local semantic
//! context. Provider control events stay on the daemon's OpenAI sideband.

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Opens a voice session with the client's SDP offer. Answered with
/// [`Opened`]; after the answer the stream carries [`RealtimeClientFrame`]s
/// and [`RealtimeServerFrame`]s.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Open {
    pub offer_sdp: String,
}

impl rho_rpc::parts::PartOpen for Open {
    const PART: rho_rpc::parts::Part = rho_rpc::parts::Part::Voice;
}

/// The answer to [`Open`].
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
