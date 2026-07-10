use senax_encoder::{Decode, Encode, Pack, Unpack};
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Decode, Deserialize, Eq, Hash, PartialEq, Encode, Serialize, Pack, Unpack,
)]
pub enum ReasoningEffort {
    Low,
    Medium,
    Xhigh,
}

/// Which Responses-API model a deep session talks to. Not part of
/// [`InferenceProfile`]: agent modes carry it separately, so persisted configs
/// stay unchanged.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InferenceModel {
    Gpt55,
    Gpt56Sol,
    Gpt56Luna,
    Gpt56Terra,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Encode, Serialize, Pack, Unpack)]
pub struct InferenceProfile {
    pub effort: ReasoningEffort,
    pub fast_mode: bool,
    /// Code-mode-only tool surface: the model gets `exec`/`wait` and reaches
    /// all other tools through JavaScript.
    pub code_mode: bool,
}

impl senax_encoder::Decoder for InferenceProfile {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        const EFFORT_ID: u64 = 0xae8c1bc4a13b4c9c;
        const FAST_MODE_ID: u64 = 0xdfdfaae5d197e253;
        const CODE_MODE_ID: u64 = 0x8209f7083637ac28;

        if reader.remaining() == 0 {
            return Err(senax_encoder::EncoderError::InsufficientData);
        }
        let tag = reader.get_u8();
        if tag != senax_encoder::core::TAG_STRUCT_NAMED {
            return Err(senax_encoder::EncoderError::StructDecode(
                senax_encoder::StructDecodeError::InvalidTag {
                    expected: senax_encoder::core::TAG_STRUCT_NAMED,
                    actual: tag,
                },
            ));
        }

        let mut effort = None;
        let mut fast_mode = None;
        let mut code_mode = None;
        loop {
            match senax_encoder::core::read_field_id_optimized(reader)? {
                0 => break,
                EFFORT_ID => effort = Some(ReasoningEffort::decode(reader)?),
                FAST_MODE_ID => fast_mode = Some(bool::decode(reader)?),
                CODE_MODE_ID => code_mode = Some(bool::decode(reader)?),
                _ => senax_encoder::core::skip_value(reader)?,
            }
        }

        Ok(Self {
            effort: effort.ok_or(senax_encoder::EncoderError::StructDecode(
                senax_encoder::StructDecodeError::MissingRequiredField {
                    field: "effort",
                    struct_name: "InferenceProfile",
                },
            ))?,
            fast_mode: fast_mode.unwrap_or(true),
            code_mode: code_mode.unwrap_or(false),
        })
    }
}

impl Default for InferenceProfile {
    fn default() -> Self {
        Self {
            effort: ReasoningEffort::Medium,
            fast_mode: true,
            code_mode: false,
        }
    }
}
