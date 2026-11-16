//! What a GUI and an agent host say about agents. The protocol itself is
//! [`worksets`]; this module holds the types its calls share with the host's
//! other tables (accounts, visualizations) and
//! [`transcript`], the vocabulary of a transcript row.

use senax_encoder::{Decode, Encode, Pack, Unpack};

pub mod transcript;
pub mod worksets;

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

/// What an agent's replies cost, summed over a span of its turns.
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
