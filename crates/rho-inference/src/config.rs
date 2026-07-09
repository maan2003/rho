use senax_encoder::{Decode, Encode, Pack, Unpack};
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Decode, Deserialize, Eq, Hash, PartialEq, Encode, Serialize, Pack, Unpack,
)]
pub enum DeepEffort {
    Low,
    Medium,
    Xhigh,
}

/// Which Responses-API model a deep session talks to. Not part of
/// [`DeepConfig`]: agent modes carry it separately, so persisted configs
/// stay unchanged.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DeepModel {
    Gpt55,
    Gpt56Sol,
    Gpt56Luna,
    Gpt56Terra,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Encode, Serialize, Pack, Unpack)]
pub struct DeepConfig {
    pub effort: DeepEffort,
    pub fast_mode: bool,
}

impl senax_encoder::Decoder for DeepConfig {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        const EFFORT_ID: u64 = 0xae8c1bc4a13b4c9c;
        const FAST_MODE_ID: u64 = 0xdfdfaae5d197e253;

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
        loop {
            match senax_encoder::core::read_field_id_optimized(reader)? {
                0 => break,
                EFFORT_ID => effort = Some(DeepEffort::decode(reader)?),
                FAST_MODE_ID => fast_mode = Some(bool::decode(reader)?),
                _ => senax_encoder::core::skip_value(reader)?,
            }
        }

        Ok(Self {
            effort: effort.ok_or(senax_encoder::EncoderError::StructDecode(
                senax_encoder::StructDecodeError::MissingRequiredField {
                    field: "effort",
                    struct_name: "DeepConfig",
                },
            ))?,
            fast_mode: fast_mode.unwrap_or(true),
        })
    }
}

impl Default for DeepConfig {
    fn default() -> Self {
        Self {
            effort: DeepEffort::Medium,
            fast_mode: true,
        }
    }
}
