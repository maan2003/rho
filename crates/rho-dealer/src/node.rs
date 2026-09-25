//! What a node is called, in memory and in ledger keys.

use rho_agent_types::AgentId;

/// What Slack calls a place a conversation happens: a direct or group
/// conversation, a channel, or a followed thread.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlackUnit {
    pub workspace: String,
    pub channel: String,
    /// `None` is the conversation or the channel itself.
    pub thread: Option<String>,
}

/// A thing in the user's world. Notes and labels are made by rho; every
/// other node exists because its source says so.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeId {
    Note(uuid::Uuid),
    Label(uuid::Uuid),
    Agent(AgentId),
    Slack(SlackUnit),
    PullRequest { repo: String, number: u64 },
}

impl NodeId {
    /// The node as text, the way ledger keys name it. No form contains
    /// `|`, which ends the node in a key.
    pub fn key(&self) -> String {
        match self {
            Self::Note(id) => format!("note:{}", id.simple()),
            Self::Label(id) => format!("label:{}", id.simple()),
            Self::Agent(id) => format!("agent:{}", id.encoded()),
            Self::Slack(unit) => match &unit.thread {
                Some(thread) => format!("slack:{}/{}/{thread}", unit.workspace, unit.channel),
                None => format!("slack:{}/{}", unit.workspace, unit.channel),
            },
            Self::PullRequest { repo, number } => format!("pr:{repo}#{number}"),
        }
    }

    /// Reads [`key`](Self::key) back. `None` for anything this build does
    /// not know, which a newer build may have written.
    pub fn parse(key: &str) -> Option<Self> {
        let (kind, rest) = key.split_once(':')?;
        Some(match kind {
            "note" => Self::Note(uuid::Uuid::try_parse(rest).ok()?),
            "label" => Self::Label(uuid::Uuid::try_parse(rest).ok()?),
            "agent" => Self::Agent(AgentId::from_encoded(rest).ok()?),
            "slack" => {
                let mut parts = rest.splitn(3, '/');
                let workspace = parts.next()?.to_owned();
                let channel = parts.next()?.to_owned();
                Self::Slack(SlackUnit {
                    workspace,
                    channel,
                    thread: parts.next().map(str::to_owned),
                })
            }
            "pr" => {
                let (repo, number) = rest.rsplit_once('#')?;
                Self::PullRequest {
                    repo: repo.to_owned(),
                    number: number.parse().ok()?,
                }
            }
            _ => return None,
        })
    }

    pub fn is_minted(&self) -> bool {
        matches!(self, Self::Note(_) | Self::Label(_))
    }

    pub fn agent(&self) -> Option<AgentId> {
        match self {
            Self::Agent(id) => Some(*id),
            _ => None,
        }
    }

    pub fn slack(&self) -> Option<&SlackUnit> {
        match self {
            Self::Slack(unit) => Some(unit),
            _ => None,
        }
    }
}

/// A node goes over the wire as its key, so a node kind a newer build
/// adds fails to decode here rather than reading as something else.
impl senax_encoder::Encoder for NodeId {
    fn encode(&self, writer: &mut bytes::BytesMut) -> senax_encoder::Result<()> {
        self.key().encode(writer)
    }

    fn is_default(&self) -> bool {
        false
    }
}

impl senax_encoder::Decoder for NodeId {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        let key = String::decode(reader)?;
        Self::parse(&key)
            .ok_or_else(|| senax_encoder::EncoderError::Decode(format!("unknown node {key}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_node_reads_back_from_its_key() {
        let nodes = [
            NodeId::Note(uuid::Uuid::new_v4()),
            NodeId::Label(uuid::Uuid::new_v4()),
            NodeId::Agent(AgentId::from_encoded("00jvj4xuk96p").unwrap()),
            NodeId::Slack(SlackUnit {
                workspace: "T1".into(),
                channel: "C2".into(),
                thread: None,
            }),
            NodeId::Slack(SlackUnit {
                workspace: "T1".into(),
                channel: "C2".into(),
                thread: Some("1700000000.000100".into()),
            }),
            NodeId::PullRequest {
                repo: "maan2003/rho".into(),
                number: 42,
            },
        ];
        for node in nodes {
            assert!(!node.key().contains('|'));
            assert_eq!(NodeId::parse(&node.key()), Some(node));
        }
        assert_eq!(NodeId::parse("gadget:1"), None);
    }
}
