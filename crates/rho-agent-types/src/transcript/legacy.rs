use senax_encoder::{Decode, Decoder, Encode, TaggedSenax};

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum OpenAiResponsesProviderData {
    Message {
        item_id: super::ProviderResponseItemId,
    },
    FunctionCall {
        item_id: super::ProviderResponseItemId,
    },
    CustomToolCall {
        item_id: super::ProviderResponseItemId,
    },
    EncryptedReasoning {
        item_id: super::ProviderResponseItemId,
        encrypted_content: String,
    },
    Compaction {
        item_id: super::ProviderResponseItemId,
        encrypted_content: String,
    },
}

impl senax_encoder::TaggedSenax for OpenAiResponsesProviderData {
    const TAG: &'static str = "openai.responses.item";
}

senax_encoder::__private::inventory::submit! {
    super::__SenaxProviderSpecificDataEntry::new(
        OpenAiResponsesProviderData::TAG,
        |mut body: bytes::Bytes| -> senax_encoder::Result<Box<dyn super::ProviderSpecificData>> {
            use bytes::Buf as _;
            let value = OpenAiResponsesProviderData::decode(&mut body)?;
            if body.remaining() != 0 {
                return Err(senax_encoder::EncoderError::Decode(format!(
                    "Trailing bytes while decoding OpenAI Responses provider data: {}",
                    body.remaining()
                )));
            }
            Ok(Box::new(value))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{InferenceResponseItem, ProviderSpecificData};

    #[test]
    fn openai_provider_data_decodes_through_registered_tag() {
        let value = InferenceResponseItem::Compaction {
            provider_specific: Box::new(OpenAiResponsesProviderData::Compaction {
                item_id: "item-42".try_into().unwrap(),
                encrypted_content: "opaque".into(),
            }) as Box<dyn ProviderSpecificData>,
        };
        let mut encoded = senax_encoder::encode(&value).unwrap();
        assert_eq!(
            senax_encoder::decode::<InferenceResponseItem>(&mut encoded).unwrap(),
            value
        );
        assert_eq!(OpenAiResponsesProviderData::TAG, "openai.responses.item");
    }
}
