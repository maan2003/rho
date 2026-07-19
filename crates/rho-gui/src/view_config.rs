//! The GUI's persistent view configuration.
//!
//! The daemon stores this as an opaque blob (`ViewConfigSet` / the `Ready`
//! payload) and never interprets it: the view logic lives here, in the
//! client, and only its inputs travel. Decoding is forgiving — an empty or
//! unreadable blob (say, from a newer client) falls back to the default
//! rather than failing the session.

use rho_ui_proto::AgentId;
use senax_encoder::{Decode, Encode, Pack, Unpack};

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ViewConfig {
    /// Retained top-to-bottom rail order, so the rail looks the same after
    /// a GUI restart instead of reshuffling by engagement recency.
    #[senax(default)]
    pub rail_order: Vec<AgentId>,
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
    fn empty_and_garbage_blobs_decode_to_default() {
        assert_eq!(ViewConfig::decode(&[]), ViewConfig::default());
        assert_eq!(ViewConfig::decode(&[0xff, 0x01, 0x02]), ViewConfig::default());
    }

    #[test]
    fn config_round_trips() {
        let config = ViewConfig {
            rail_order: vec![
                AgentId::from_counter(1, &rho_ui_proto::AgentIdDomain(7)).unwrap(),
                AgentId::from_counter(2, &rho_ui_proto::AgentIdDomain(7)).unwrap(),
            ],
            rail_tail_expanded: true,
        };
        assert_eq!(ViewConfig::decode(&config.encode()), config);
    }
}
