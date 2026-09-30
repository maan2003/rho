use senax_encoder::{Decode, Encode, Pack, Unpack};
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Decode, Deserialize, Eq, Hash, PartialEq, Encode, Serialize, Pack, Unpack,
)]
pub enum ReasoningEffort {
    Low,
    Medium,
    Xhigh,
    High,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

/// Which Responses-API model a step talks to. Not part of
/// [`InferenceProfile`]: agent modes carry it separately, so persisted configs
/// stay unchanged.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InferenceModel {
    Gpt61Sol,
    Gpt6Luna,
    Gpt6Astra,
}

impl InferenceModel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gpt61Sol => "gpt-6.1-sol",
            Self::Gpt6Luna => "gpt-6-luna",
            Self::Gpt6Astra => "gpt-6-astra",
        }
    }
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Encode, Decode, Serialize, Pack, Unpack,
)]
pub struct InferenceProfile {
    pub effort: ReasoningEffort,
    pub fast_mode: bool,
}

impl Default for InferenceProfile {
    fn default() -> Self {
        Self {
            effort: ReasoningEffort::Medium,
            fast_mode: true,
        }
    }
}
