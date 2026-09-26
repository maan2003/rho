//! The agent2 chat protocol. The host log is primary; every session begins
//! with a snapshot, then sends append-only chat changes.
use camino::Utf8PathBuf;
pub use rho_agent_types::AgentId;
use rho_agent_types::UnixMs;
use senax_encoder::{Decode, Encode, Pack, Unpack};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub struct MessageId(pub u64);
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Party {
    Human,
    Agent(AgentId),
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ChatKind {
    Message {
        id: MessageId,
        from: Party,
        to: Party,
        text: String,
    },
    Status(String),
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ChatEvent {
    pub seq: u64,
    pub at: UnixMs,
    pub kind: ChatKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Effort {
    Low,
    #[default]
    Medium,
    High,
    XHigh,
}
impl std::str::FromStr for Effort {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            _ => Err("use low, medium, high, or xhigh".into()),
        }
    }
}
impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentInfo {
    pub id: AgentId,
    pub workdir: Utf8PathBuf,
    pub model: String,
    pub effort: Effort,
    pub archived: bool,
    pub status: Option<String>,
    pub chat: Vec<ChatEvent>,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct CreateAgent {
    pub workdir: Utf8PathBuf,
    pub model: String,
    pub effort: Effort,
    pub initial_message: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct SendMessage {
    pub agent_id: AgentId,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ArchiveAgent {
    pub agent_id: AgentId,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ListAgents;

#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum Open {
    Session,
    Request(Request),
}
rho_rpc::calls! {
    pub enum Request {
        CreateAgent(CreateAgent) -> AgentId;
        SendMessage(SendMessage) -> ();
        ArchiveAgent(ArchiveAgent) -> ();
        ListAgents(ListAgents) -> Vec<AgentInfo>;
    }
}
impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::Agents2;
    fn debug_reply(&self, frame: &[u8]) -> Option<String> {
        match self {
            Self::Request(request) => Some(request.debug_answer(frame)),
            Self::Session => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    Snapshot { agents: Vec<AgentInfo> },
    Created { agent: AgentInfo },
    Chat { agent_id: AgentId, event: ChatEvent },
    Archived { agent_id: AgentId, archived: bool },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_and_chat_round_trip_without_notebook_fields() {
        let id = AgentId::from_counter(17, &rho_agent_types::AgentIdDomain(42)).unwrap();
        let request = Open::Request(Request::CreateAgent(CreateAgent {
            workdir: "/src/workset".into(),
            model: "gpt-6-sol".into(),
            effort: Effort::High,
            initial_message: Some("hello".into()),
        }));
        let bytes = senax_encoder::pack(&request).unwrap();
        let decoded: Open = senax_encoder::unpack(&mut bytes.as_ref()).unwrap();
        assert_eq!(decoded, request);
        let frame = ServerFrame::Chat {
            agent_id: id.clone(),
            event: ChatEvent {
                seq: 17,
                at: UnixMs(123),
                kind: ChatKind::Message {
                    id: MessageId(91),
                    from: Party::Human,
                    to: Party::Agent(id),
                    text: "hi".into(),
                },
            },
        };
        let bytes = senax_encoder::pack(&frame).unwrap();
        assert_eq!(
            senax_encoder::unpack::<ServerFrame>(&mut bytes.as_ref()).unwrap(),
            frame
        );
    }
}
