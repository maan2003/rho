use senax_encoder::{Decode, Decoder, Encode, TaggedSenax};

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum OpenAiResponsesProviderData {
    Message {
        item_id: crate::types::ProviderResponseItemId,
    },
    FunctionCall {
        item_id: crate::types::ProviderResponseItemId,
    },
    CustomToolCall {
        item_id: crate::types::ProviderResponseItemId,
    },
    EncryptedReasoning {
        item_id: crate::types::ProviderResponseItemId,
        encrypted_content: String,
    },
    Compaction {
        item_id: crate::types::ProviderResponseItemId,
        encrypted_content: String,
    },
}

impl senax_encoder::TaggedSenax for OpenAiResponsesProviderData {
    const TAG: &'static str = "openai.responses.item";
}

senax_encoder::__private::inventory::submit! {
    crate::types::__SenaxProviderSpecificDataEntry::new(
        OpenAiResponsesProviderData::TAG,
        |mut body: bytes::Bytes| -> senax_encoder::Result<Box<dyn crate::types::ProviderSpecificData>> {
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
