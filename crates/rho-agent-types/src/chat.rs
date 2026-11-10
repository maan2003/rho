//! Chat: streams of topics of messages, kept by each host in one log
//! ([ARCH-agent-host](../../../agent-host/specs/ARCH-agent-host.md)).
//!
//! A host's chat is one log of [`ChatEvent`]s, dense from one. Everything
//! chat names is named by the position of the event that made it: a
//! [`StreamId`] by its `StreamCreated`, a [`TopicId`] by its `TopicStarted`,
//! `DirectStarted` or `PeerAccepted`, a [`MessageId`] by its `Posted`.
//!
//! Chat and the agents are two hierarchies that meet only here: an agent
//! receives a message because it is a member of the conversation or
//! because the message mentions it. A client holds the whole log and folds
//! it; the host folds the same events into its indexes in the commit that
//! appends them.

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::{AgentId, ContentPart, UnixMs};

/// A position in a host's chat log: dense from one, never reused. Zero is
/// "before any event".
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct ChatSeq(pub u64);

impl ChatSeq {
    pub const ZERO: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("chat log overflow"))
    }
}

macro_rules! chat_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
        )]
        pub struct $name(pub ChatSeq);
    };
}

chat_id!(
    /// A stream, named by the event that created it.
    StreamId
);
chat_id!(
    /// A conversation, named by the event that started it: a topic in a
    /// stream, a direct conversation between agents, or an agent's
    /// conversation with a peer on another host.
    TopicId
);
chat_id!(
    /// A message, named by the event that posted it.
    MessageId
);

/// Who a message is from or mentions: the user, one of the host's agents,
/// or the other end of a peer conversation.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub enum Address {
    User,
    Agent(AgentId),
    /// The peer of the peer conversation the message is in: who that is,
    /// the conversation says. Nothing mentions it.
    Peer,
}

/// The name of a recipient inside a harness, as GCCP carries it
/// ([docs/gccp.md](../../../docs/gccp.md)): 1 to 64 bytes of `a-z`, `0-9`,
/// `.`, `_` and `-`. Only the harness that owns it gives it meaning.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack)]
pub struct LocalId(String);

impl LocalId {
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for LocalId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let allowed = |byte: u8| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        };
        if text.is_empty() || text.len() > Self::MAX_LEN || !text.bytes().all(allowed) {
            return Err(format!(
                "a local id is 1 to {} of a-z, 0-9, '.', '_' and '-', not {text:?}",
                Self::MAX_LEN
            ));
        }
        Ok(Self(text.to_owned()))
    }
}

impl std::fmt::Display for LocalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A GCCP address: a recipient on a harness, here or elsewhere. As text it is
/// `<endpoint id>/<local id>`, the endpoint id in lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack)]
pub struct PeerAddress {
    /// The harness's iroh endpoint id: its public key.
    pub endpoint: [u8; 32],
    pub local: LocalId,
}

impl std::str::FromStr for PeerAddress {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let shape = || format!("an address is <endpoint id>/<local id>, not {text:?}");
        let (endpoint_text, local) = text.split_once('/').ok_or_else(shape)?;
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let mut endpoint = [0; 32];
        if endpoint_text.len() != 64 {
            return Err(format!(
                "an endpoint id is 64 lowercase hex digits, not {endpoint_text:?}"
            ));
        }
        for (byte, pair) in endpoint.iter_mut().zip(endpoint_text.as_bytes().chunks(2)) {
            let (Some(high), Some(low)) = (digit(pair[0]), digit(pair[1])) else {
                return Err(format!(
                    "an endpoint id is 64 lowercase hex digits, not {endpoint_text:?}"
                ));
            };
            *byte = high << 4 | low;
        }
        Ok(Self {
            endpoint,
            local: local.parse()?,
        })
    }
}

impl std::fmt::Display for PeerAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.endpoint {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "/{}", self.local)
    }
}

/// What a mention asks of the one it names. A mention of the user is how
/// an agent asks for attention: the dealer ranks on it (`rho-dealer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum MentionKind {
    /// Asks for something the work needs.
    Ask,
    /// Delivers what was asked for.
    Result,
    /// Worth reading, not now.
    Fyi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Mention {
    pub to: Address,
    pub kind: MentionKind,
}

/// One change to a host's chat.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ChatEvent {
    StreamCreated {
        name: String,
    },
    /// A topic in `stream`, with no members yet.
    TopicStarted {
        stream: StreamId,
        name: String,
        by: Address,
    },
    /// A conversation of two agents and no one else: its members are
    /// fixed, and no one joins it.
    DirectStarted {
        members: [AgentId; 2],
    },
    /// An agent accepts a GCCP address under a name it trusts: a
    /// conversation of that agent and the peer, and no one else. From here
    /// on the host delivers what the peer sends the agent, and sends the
    /// peer what the agent posts.
    PeerAccepted {
        agent: AgentId,
        peer: PeerAddress,
        name: String,
    },
    /// An acceptance is taken back: nothing more crosses in the
    /// conversation, either way. Accepting the address again starts a new
    /// one.
    PeerRemoved {
        topic: TopicId,
    },
    /// An agent becomes a member of a topic: it receives the topic's
    /// messages from here on.
    Joined {
        topic: TopicId,
        agent: AgentId,
    },
    /// An agent stops being a member of a topic: it receives only the
    /// messages that mention it from here on.
    Left {
        topic: TopicId,
        agent: AgentId,
    },
    /// A topic as it now is, whole: renamed, moved to another stream, or
    /// resolved (done) or not.
    TopicChanged {
        topic: TopicId,
        stream: StreamId,
        name: String,
        resolved: bool,
    },
    /// Messages now in topic `to`, wherever they were. A move delivers
    /// nothing again: who received a message was fixed when it was posted.
    Moved {
        messages: Vec<MessageId>,
        to: TopicId,
    },
    /// A message in `topic`, the topic it was posted in: a later `Moved`
    /// says where it is now. Its content never changes. Every member but
    /// its sender receives it, and so does each agent it mentions. Neither
    /// posting nor a mention makes anyone a member. In a peer conversation
    /// it is from the agent, for the host to send, or from the peer.
    Posted {
        topic: TopicId,
        from: Address,
        mentions: Vec<Mention>,
        content: Vec<ContentPart>,
    },
}

/// One entry of a host's chat log.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ChatRecord {
    pub seq: ChatSeq,
    pub at: UnixMs,
    pub event: ChatEvent,
}

/// A posted message, with where it is now.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Message {
    pub id: MessageId,
    pub at: UnixMs,
    pub topic: TopicId,
    pub from: Address,
    pub mentions: Vec<Mention>,
    pub content: Vec<ContentPart>,
}

impl Message {
    /// The message a record posts, if it posts one, in the topic it was
    /// posted in.
    pub fn from_record(record: &ChatRecord) -> Option<Self> {
        let ChatEvent::Posted {
            topic,
            from,
            mentions,
            content,
        } = &record.event
        else {
            return None;
        };
        Some(Self {
            id: MessageId(record.seq),
            at: record.at,
            topic: *topic,
            from: *from,
            mentions: mentions.clone(),
            content: content.clone(),
        })
    }

    pub fn mentions(&self, address: Address) -> Option<MentionKind> {
        self.mentions
            .iter()
            .find(|mention| mention.to == address)
            .map(|mention| mention.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_reads_back_as_it_is_written() {
        let text = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f/x4.b_c-9";
        let address: PeerAddress = text.parse().unwrap();
        assert_eq!(address.endpoint[1], 1);
        assert_eq!(address.endpoint[31], 0x1f);
        assert_eq!(address.local.as_str(), "x4.b_c-9");
        assert_eq!(address.to_string(), text);
    }

    #[test]
    fn an_address_outside_the_grammar_is_no_address() {
        let endpoint = "00".repeat(32);
        for bad in [
            endpoint.clone(),
            format!("{endpoint}/"),
            format!("{endpoint}/Upper"),
            format!("{endpoint}/a/b"),
            format!("{endpoint}/{}", "a".repeat(65)),
            format!("{}/a", "0".repeat(63)),
            format!("{}/a", "0".repeat(66)),
            format!("{}/a", "AB".repeat(32)),
            format!("{}/a", "zz".repeat(32)),
        ] {
            assert!(bad.parse::<PeerAddress>().is_err(), "{bad}");
        }
        assert!(
            format!("{endpoint}/{}", "a".repeat(64))
                .parse::<PeerAddress>()
                .is_ok()
        );
    }
}
