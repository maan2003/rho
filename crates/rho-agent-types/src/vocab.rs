//! Who an agent is, what a message holds, and when things happened.

use prefix_id::{PrefixId, PrefixIdDomain};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use serde::{Deserialize, Serialize};
/// Provider/host observations, never Python execution timestamps.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Pack, Unpack,
)]
pub enum ExecMilestone {
    FirstBlock,
    ArgumentsFinished,
    ResponseFinished,
    Boundary,
    /// The host successfully handed the reply to its transport, not evidence
    /// that the remote model consumed it.
    HandedOff,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Pack, Unpack,
)]
pub struct ExecTiming {
    pub first_block_at: Option<UnixMs>,
    pub arguments_finished_at: Option<UnixMs>,
    pub response_finished_at: Option<UnixMs>,
    pub boundary_at: Option<UnixMs>,
    pub handed_off_at: Option<UnixMs>,
}

impl ExecTiming {
    pub fn observe(&mut self, milestone: ExecMilestone, at: UnixMs) {
        let field = match milestone {
            ExecMilestone::FirstBlock => &mut self.first_block_at,
            ExecMilestone::ArgumentsFinished => &mut self.arguments_finished_at,
            ExecMilestone::ResponseFinished => &mut self.response_finished_at,
            ExecMilestone::Boundary => &mut self.boundary_at,
            ExecMilestone::HandedOff => &mut self.handed_off_at,
        };
        field.get_or_insert(at);
    }
}

pub type AgentId = PrefixId<AgentIdDomain>;

/// Keys agent-id encoding with the owning database's persisted machine seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentIdDomain(pub u64);

impl PrefixIdDomain for AgentIdDomain {
    const KIND: &'static str = "agent-id";
    // Where the domain lived when the first tables were written.
    const RECORDED_NAME: &'static str = "rho_core::AgentIdDomain";

    fn machine_seed(&self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentRole {
    Engineer { intelligence: EngineerIntelligence },
    Advisor { intelligence: AdvisorIntelligence },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum EngineerIntelligence {
    Mini,
    Medium,
    High,
    Medium1,
    High1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AdvisorIntelligence {
    Low,
    Medium,
    Medium1,
}

impl Default for AgentRole {
    fn default() -> Self {
        Self::Engineer {
            intelligence: EngineerIntelligence::Medium,
        }
    }
}

impl AgentRole {
    pub fn is_engineer(self) -> bool {
        matches!(self, Self::Engineer { .. })
    }

    pub fn handle_prefix(self) -> &'static str {
        match self {
            Self::Engineer { .. } => "eng",
            Self::Advisor { .. } => "adv",
        }
    }

    /// The agent's full id behind its role's prefix ("eng-…"), as agents
    /// see it in `$RHO_AGENT_ID`.
    pub fn full_handle(self, agent_id: AgentId) -> String {
        format!("{}-{}", self.handle_prefix(), agent_id.encoded())
    }
}

/// The agent a full handle ("eng-…") names. A bare full id parses too.
pub fn parse_full_handle(text: &str) -> Option<AgentId> {
    let id = match text.split_once('-') {
        Some(("eng" | "adv", id)) => id,
        Some(_) => return None,
        None => text,
    };
    AgentId::from_encoded(id).ok()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Pack, Unpack)]
pub enum ContentPart {
    Text {
        text: String,
    },
    /// An encoded image supplied by the user. The bytes are kept in the
    /// shared vocabulary so queued inputs and persisted transcripts retain
    /// the original attachment without provider-specific wrappers.
    Image {
        media_type: String,
        data: Vec<u8>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    Commentary,
    FinalAnswer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum ToolOutputStatus {
    Success,
    Error,
    Cancelled,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
    Pack,
    Unpack,
)]
pub struct UnixMs(pub u64);

impl UnixMs {
    pub fn now() -> Self {
        Self(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time before unix epoch")
                .as_millis()
                .try_into()
                .expect("unix millis overflow"),
        )
    }

    pub fn saturating_duration_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// A deadline, for code that reasons in "this long after that happened".
impl std::ops::Add<std::time::Duration> for UnixMs {
    type Output = Self;

    fn add(self, later: std::time::Duration) -> Self {
        Self(self.0.saturating_add(later.as_millis() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_handle_round_trips_and_rejects_other_prefixes() {
        let agent = AgentId::from_counter(7, &AgentIdDomain(3)).unwrap();
        let advisor = AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Low,
        };
        let handle = advisor.full_handle(agent);
        assert_eq!(handle, format!("adv-{}", agent.encoded()));
        assert_eq!(parse_full_handle(&handle), Some(agent));
        assert_eq!(parse_full_handle(&agent.encoded()), Some(agent));
        assert_eq!(parse_full_handle(&format!("pm-{}", agent.encoded())), None);
        assert_eq!(parse_full_handle(&handle[..8]), None);
    }
}
