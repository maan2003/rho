//! Values the workset log's rows and the usage tables share between the
//! host and the workers.

use redb_derive::{Key, Value as RedbValue};
use senax_encoder::{Decode, Encode};
use uuid::Uuid;

pub const AGENT_USAGE_BUCKET_MS: u64 = 5 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue, Encode, Decode)]
pub struct AgentUsageModel(u8);

impl AgentUsageModel {
    pub const UNKNOWN: Self = Self(0);
    pub const GPT: Self = Self(1);
    pub const FABLE: Self = Self(2);
    pub const OPUS: Self = Self(3);
    pub const TERRA: Self = Self(4);
    pub const LUNA: Self = Self(5);
    pub const ASTRA: Self = Self(7);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
            Self::OPUS => "opus",
            Self::TERRA => "terra",
            Self::LUNA => "luna",
            Self::ASTRA => "astra",
            _ => "unknown",
        }
    }

    /// The model [`Self::name`] names.
    pub fn named(name: &str) -> Self {
        [
            Self::GPT,
            Self::FABLE,
            Self::OPUS,
            Self::TERRA,
            Self::LUNA,
            Self::ASTRA,
        ]
        .into_iter()
        .find(|model| model.name() == name)
        .unwrap_or(Self::UNKNOWN)
    }
}

impl Default for AgentUsageModel {
    fn default() -> Self {
        Self::UNKNOWN
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct AgentUsageBucket {
    pub bucket_start_ms: u64,
    pub model: AgentUsageModel,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    pub approximate: bool,
}

impl AgentUsageBucket {
    pub fn add(&mut self, other: &Self) {
        if self.requests == 0 {
            self.model = other.model;
        } else if other.model != AgentUsageModel::UNKNOWN && self.model != other.model {
            self.model = AgentUsageModel::UNKNOWN;
        }
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.cache_write_1h_tokens = self
            .cache_write_1h_tokens
            .saturating_add(other.cache_write_1h_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.requests = self.requests.saturating_add(other.requests);
        self.approximate |= other.approximate;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ClaudeRewind {
    pub source_session_id: Uuid,
    pub session_id: Uuid,
    pub resume_at: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct TranscriptCall {
    pub id: String,
    pub name: String,
    /// The arguments as JSON, whole.
    pub arguments: String,
}
