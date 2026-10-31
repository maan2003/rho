//! What a GUI and an agent host say about agents. The protocol itself is
//! [`rho2`]; this module holds the types its calls share with the host's
//! other tables (usage, quota, accounts, visualizations) and
//! [`transcript`], the vocabulary of a transcript row.

use rho_agent_types::AgentId;
use senax_encoder::{Decode, Encode, Pack, Unpack};

pub mod rho2;
pub mod transcript;

/// A recorded visualization.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Visualization {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct VisualizationContent {
    pub mime_type: String,
    pub content: Vec<u8>,
}

/// Stores an immutable visualization snapshot.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct RecordVisualization {
    pub mime_type: String,
    pub content: Vec<u8>,
}

/// Every provider's quota as it stands.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct QuotaUsage;

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct QuotaHistory;

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GlobalUsage {
    pub since_ms: u64,
}

/// Raw per-agent usage needed to form cost distributions beginning at
/// `since_ms`, with the fixed trailing-window lookback.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct AgentCostDistribution {
    pub since_ms: u64,
}

/// Which Claude accounts exist and which one agents run on.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ClaudeAccounts;

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ClaudeAccountList {
    pub accounts: Vec<String>,
    pub current: String,
}

/// Puts every agent on `name` from its next turn.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct SetClaudeAccount {
    pub name: String,
}

/// Enables or disables one provider account namespace on this host.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct SetAuthAccountEnabled {
    pub name: String,
    pub enabled: bool,
}

/// Window represented by each point in the agent-cost distribution graph.
pub const AGENT_COST_WINDOW_DAYS: u64 = 7;

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaSummary {
    pub model: String,
    /// Host-local ChatGPT OAuth namespace; absent for Claude.
    pub auth_namespace: Option<String>,
    pub remaining_percent: u8,
    pub burn_10m: u16,
    pub burn_2h: u16,
    pub burn_1d: u16,
    pub burn_3d: u16,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaSeries {
    pub model: String,
    /// Host-local ChatGPT OAuth namespace; absent for Claude.
    pub auth_namespace: Option<String>,
    pub points: Vec<QuotaPoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaPoint {
    pub observed_at_ms: u64,
    pub remaining_percent: u8,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentUsageBucket {
    pub bucket_start_ms: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    pub approximate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentUsageSeries {
    pub model: String,
    pub buckets: Vec<AgentUsageBucket>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentCostSeries {
    /// Host-local identity. Clients combining hosts must keep the host in the
    /// distribution key rather than merging equal counters.
    pub agent_id: AgentId,
    pub model: String,
    pub buckets: Vec<AgentUsageBucket>,
}

/// Host-wide authentication settings presented by a GUI host.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AuthState {
    pub namespaces: Vec<String>,
    pub disabled_namespaces: Vec<String>,
    pub active_namespace: Option<String>,
}
