//! The desktops part of a host, [`rho_rpc::parts::Part::Desktop`]: which
//! desktops the host's worksets run, and a live view of one.

use senax_encoder::{Decode, Encode, Pack, Unpack};

/// What a desktops stream is for.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    /// The desktops running in this host's worksets: the whole list as
    /// `Vec<`[`DesktopSession`]`>`, pushed whenever it changes.
    Sessions,
    /// One live application over MoQ streams on this connection, so only
    /// over iroh. Answered with [`rho_rpc::parts::Opened`].
    Wayland {
        media_id: u64,
        agent: String,
        session: String,
    },
}

impl rho_rpc::parts::PartOpen for Open {
    const PART: rho_rpc::parts::Part = rho_rpc::parts::Part::Desktop;
}

/// One desktop an agent's workset runs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub struct DesktopSession {
    pub agent: String,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openings_survive_the_envelope() {
        for open in [
            Open::Sessions,
            Open::Wayland {
                media_id: 1,
                agent: "eng-test".to_owned(),
                session: "desk".to_owned(),
            },
        ] {
            let envelope = rho_rpc::parts::Open::of(&open).unwrap();
            assert_eq!(envelope.unpack::<Open>().unwrap(), open);
        }
    }
}
