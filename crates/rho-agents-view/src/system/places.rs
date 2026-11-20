//! Where a channel's agents work, as the user answered it once.
//!
//! The answer lives in the channel's agents: the next agent of a channel
//! joins the newest one's workset ([`channel_start`]). Until an agent of
//! the channel exists, which is also when its start failed before one
//! did, the answer is kept here, on this device, so the next idea asks
//! nothing again. Once an agent exists, the agents say it and this is not
//! read.
//!
//! [`channel_start`]: rho_agents_client::create::channel_start

use std::collections::HashMap;

use camino::Utf8PathBuf;
use redb::{TableDefinition, TableHandle as _};
use rho_agent_types::{EndpointId, MessageId};
use rho_agents_client::create::Start;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};

/// A channel by its host's identity and its root message's id.
type Channel = ([u8; 32], u64);

const PLACES: TableDefinition<Channel, Sen<Answered>> =
    TableDefinition::new("gui_channel_place_v1");

/// The two answers worth keeping: a join names an agent that exists, and
/// then the agents say it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum Answered {
    Repository { repo: String, rev: String },
    Directory { cwd: String },
}

impl Answered {
    fn of(start: &Start) -> Option<Self> {
        match start {
            Start::NewOn { repo, rev } => Some(Self::Repository {
                repo: repo.to_string(),
                rev: rev.clone(),
            }),
            Start::Here { cwd } => Some(Self::Directory {
                cwd: cwd.to_string(),
            }),
            Start::Join { .. } => None,
        }
    }

    fn start(&self) -> Start {
        match self {
            Self::Repository { repo, rev } => Start::NewOn {
                repo: Utf8PathBuf::from(repo),
                rev: rev.clone(),
            },
            Self::Directory { cwd } => Start::Here {
                cwd: Utf8PathBuf::from(cwd),
            },
        }
    }
}

pub(crate) struct ChannelPlaces {
    db: RhoDb,
    held: HashMap<Channel, Answered>,
}

fn key(endpoint: EndpointId, channel: MessageId) -> Channel {
    (endpoint.0, channel.0.0)
}

impl ChannelPlaces {
    /// What this device was told before, read once.
    pub(crate) fn open(db: RhoDb) -> Self {
        let read = db.read();
        let held = match read.has_table(PLACES.name()) {
            true => read
                .open_table(PLACES)
                .iter()
                .map(|(channel, answered)| (channel.value(), answered.value().into_owned()))
                .collect(),
            false => HashMap::new(),
        };
        drop(read);
        Self { db, held }
    }

    /// Where the user said `channel`'s agents work, when they said it here.
    pub(crate) fn get(&self, endpoint: EndpointId, channel: MessageId) -> Option<Start> {
        self.held.get(&key(endpoint, channel)).map(Answered::start)
    }

    /// Keeps the user's answer for `channel`. Blocking on the commit, as
    /// the attached hosts' list does: a few bytes, written where the user
    /// just answered.
    pub(crate) fn remember(&mut self, endpoint: EndpointId, channel: MessageId, start: &Start) {
        let Some(answered) = Answered::of(start) else {
            return;
        };
        let key = key(endpoint, channel);
        if self.held.get(&key) == Some(&answered) {
            return;
        }
        futures::executor::block_on(async {
            let mut write = self.db.write().await;
            write
                .open_table(PLACES)
                .insert(key, SenValue::borrowed(&answered));
            write.commit();
        });
        self.held.insert(key, answered);
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::ChatSeq;

    use super::*;

    /// An answer outlives the window: a device opened again reads it back,
    /// and a later answer for the channel replaces it. A join is not kept.
    #[test]
    fn an_answer_is_read_back_after_a_restart() {
        let db = RhoDb::in_memory();
        let host = EndpointId([7; 32]);
        let channel = MessageId(ChatSeq(3));
        let other = MessageId(ChatSeq(4));
        let mut places = ChannelPlaces::open(db.clone());
        assert_eq!(places.get(host, channel), None);
        places.remember(
            host,
            channel,
            &Start::NewOn {
                repo: "https://example.com/rho.git".into(),
                rev: String::new(),
            },
        );
        places.remember(
            host,
            other,
            &Start::Here {
                cwd: "/src/a".into(),
            },
        );
        places.remember(
            host,
            other,
            &Start::Here {
                cwd: "/src/b".into(),
            },
        );
        places.remember(
            host,
            channel,
            &Start::Join {
                workset: rho_agent_types::WorksetId([1; 6]),
                cwd: "/src/x".into(),
            },
        );
        drop(places);
        let places = ChannelPlaces::open(db);
        assert_eq!(
            places.get(host, channel),
            Some(Start::NewOn {
                repo: "https://example.com/rho.git".into(),
                rev: String::new(),
            })
        );
        assert_eq!(
            places.get(host, other),
            Some(Start::Here {
                cwd: "/src/b".into()
            })
        );
        assert_eq!(places.get(EndpointId([8; 32]), channel), None);
    }
}
