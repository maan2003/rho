use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ServiceTier {
    Flex,
    Priority,
    Normal,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum TextVerbosity {
    Low,
    Medium,
    High,
}

/// How the provider should automatically compact long threads.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum AutoCompaction {
    Threshold(u64),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Gpt5Model(pub Cow<'static, str>);

impl Gpt5Model {
    pub const GPT_5_5: Self = Self(Cow::Borrowed("gpt-5.5"));
}

impl Serialize for Gpt5Model {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Gpt5Model {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(|model| Self(Cow::Owned(model)))
    }
}

// This is persisted per agent, and some parts can be changed across requests,
// some not.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct Gpt5Config {
    pub model: Gpt5Model,
    pub auto_compaction: Option<AutoCompaction>,
    pub effort: Effort,
    pub text_verbosity: TextVerbosity,
    pub service_tier: ServiceTier,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum InferenceConfig {
    Gpt5(Gpt5Config),
}

impl InferenceConfig {
    pub fn deep() -> Self {
        Self::Gpt5(Gpt5Config {
            model: Gpt5Model::GPT_5_5,
            auto_compaction: Some(AutoCompaction::Threshold(272_000 * 95 / 100 * 90 / 100)),
            effort: Effort::Medium,
            text_verbosity: TextVerbosity::Low,
            service_tier: ServiceTier::Normal,
        })
    }

    pub fn protect(self) -> InferenceProtectedConfig {
        InferenceProtectedConfig::new(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProtectiveId(u64);

/// A runtime-safe inference config wrapper.
///
/// The protected wrapper gives callers read-only access to the full config
/// while allowing only request-time tuning knobs to change. Identity-like
/// fields such as model remain frozen for the life of this wrapper.
pub struct InferenceProtectedConfig {
    id: ProtectiveId,
    config: InferenceConfig,
}

impl Serialize for InferenceProtectedConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.config.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InferenceProtectedConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        InferenceConfig::deserialize(deserializer).map(InferenceConfig::protect)
    }
}

impl InferenceProtectedConfig {
    fn new(config: InferenceConfig) -> Self {
        static NEXT_PROTECTIVE_ID: AtomicU64 = AtomicU64::new(1);

        Self {
            id: ProtectiveId(NEXT_PROTECTIVE_ID.fetch_add(1, Ordering::Relaxed)),
            config,
        }
    }

    pub fn config(&self) -> &InferenceConfig {
        &self.config
    }

    pub fn update(&mut self, other: Self) -> bool {
        if self.id != other.id {
            return false;
        }
        *self = other;
        true
    }

    pub fn set_effort(&mut self, effort: Effort) {
        match &mut self.config {
            InferenceConfig::Gpt5(config) => {
                config.effort = effort;
            }
        }
    }

    pub fn set_auto_compaction(&mut self, auto_compaction: Option<AutoCompaction>) {
        match &mut self.config {
            InferenceConfig::Gpt5(config) => {
                config.auto_compaction = auto_compaction;
            }
        }
    }

    pub fn set_text_verbosity(&mut self, text_verbosity: TextVerbosity) {
        match &mut self.config {
            InferenceConfig::Gpt5(config) => {
                config.text_verbosity = text_verbosity;
            }
        }
    }

    pub fn set_service_tier(&mut self, service_tier: ServiceTier) {
        match &mut self.config {
            InferenceConfig::Gpt5(config) => {
                config.service_tier = service_tier;
            }
        }
    }
}
