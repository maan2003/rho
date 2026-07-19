//! The GUI's persistent view configuration.
//!
//! The daemon stores this as an opaque blob (`ViewConfigSet` / the `Ready`
//! payload) and never interprets it: the view logic lives here, in the
//! client, and only its inputs travel. Decoding is forgiving — an empty or
//! unreadable blob (say, from a newer client) falls back to the default
//! rather than failing the session.

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Only choices the user made belong here — derived state (rail ordering,
/// attention, engagement recency) is rebuilt from the daemon's snapshots
/// every session and must never be persisted.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ViewConfig {
    /// Whether the rail's quiet tail is expanded in place.
    #[senax(default)]
    pub rail_tail_expanded: bool,
}

impl ViewConfig {
    pub fn decode(mut data: &[u8]) -> Self {
        if data.is_empty() {
            return Self::default();
        }
        senax_encoder::unpack(&mut data).unwrap_or_default()
    }

    pub fn encode(&self) -> Vec<u8> {
        senax_encoder::pack(self).map(Into::into).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_garbage_blobs_decode_without_failing() {
        assert_eq!(ViewConfig::decode(&[]), ViewConfig::default());
        // Garbage may happen to parse (senax skips unknown fields) — the
        // guarantee is only that it never errors out of the session.
        let _ = ViewConfig::decode(&[0xff, 0x01, 0x02]);
    }

    #[test]
    fn config_round_trips() {
        let config = ViewConfig {
            rail_tail_expanded: true,
        };
        assert_eq!(ViewConfig::decode(&config.encode()), config);
    }
}
