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

/// Which Responses-API model a step talks to, by the name Python gives it.
/// Each name follows its family's newest slug: an agent on `Astra` moves to a
/// new Astra at its next request. Not part of [`InferenceProfile`]: agent
/// modes carry it separately, so persisted configs stay unchanged.
///
/// The `rename`s keep the wire ids of the rows written when the variants were
/// named for their slugs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Encode, Decode)]
pub enum InferenceModel {
    #[senax(rename = "Gpt61Sol")]
    Sol,
    #[senax(rename = "Gpt61Terra")]
    Terra,
    #[senax(rename = "Gpt6Astra")]
    Astra,
}

impl InferenceModel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sol => "gpt-6.2-sol",
            Self::Terra => "gpt-6.1-terra",
            Self::Astra => "gpt-6.2-astra",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows written when the variants were named for their slugs read back
    /// as the same models.
    #[test]
    fn models_keep_their_wire_ids_across_the_rename() {
        #[derive(Encode)]
        enum Before {
            Gpt61Sol,
            Gpt61Terra,
            Gpt6Astra,
        }
        for (before, now) in [
            (Before::Gpt61Sol, InferenceModel::Sol),
            (Before::Gpt61Terra, InferenceModel::Terra),
            (Before::Gpt6Astra, InferenceModel::Astra),
        ] {
            let bytes = senax_encoder::encode(&before).unwrap();
            let read: InferenceModel = senax_encoder::decode(&mut bytes.as_ref()).unwrap();
            assert_eq!(read, now);
        }
    }
}
