use std::collections::HashMap;

use anyhow::{Context as _, Result, bail};
use rho_core::{
    ContentPart, ContextBlock, InferenceRequest, InferenceResponseItem, MessageSender,
    ProviderSpecificData, TokenUsage, ToolCallId, ToolName, ToolType,
};
use senax_encoder::{Decode, Decoder, Encode, TaggedSenax};
use serde_json::{Value, json};

pub(crate) const MODEL_ID: &str = "gemini-3.5-flash-low";

#[derive(Clone, Copy, Debug, Decode, Encode, Eq, PartialEq)]
pub(crate) enum ThoughtSignatureAttachment {
    Standalone,
    NextPart,
}

#[derive(Clone, Debug, Decode, Encode, Eq, PartialEq)]
pub(crate) struct AntigravityProviderData {
    pub(crate) signature: String,
    pub(crate) attachment: ThoughtSignatureAttachment,
}

impl TaggedSenax for AntigravityProviderData {
    const TAG: &'static str = "google.antigravity.thought-signature";
}

senax_encoder::__private::inventory::submit! {
    rho_core::__SenaxProviderSpecificDataEntry::new(
        AntigravityProviderData::TAG,
        |mut body: bytes::Bytes| -> senax_encoder::Result<Box<dyn ProviderSpecificData>> {
            use bytes::Buf as _;
            let value = AntigravityProviderData::decode(&mut body)?;
            if body.remaining() != 0 {
                return Err(senax_encoder::EncoderError::Decode(format!(
                    "Trailing bytes while decoding Antigravity provider data: {}",
                    body.remaining()
                )));
            }
            Ok(Box::new(value))
        },
    )
}

#[derive(Debug)]
pub(crate) struct ParsedResponse {
    pub(crate) items: Vec<InferenceResponseItem>,
    pub(crate) usage: Option<TokenUsage>,
}

pub(crate) fn build_request(
    request_id: &str,
    project_id: &str,
    request: &InferenceRequest,
) -> Result<Value> {
    let mut contents = Vec::new();
    let mut call_names = HashMap::<String, String>::new();
    let mut pending_signature = None;

    for block in &request.input {
        match block.as_ref() {
            ContextBlock::UserMessage { sender, content } => {
                flush_pending(&mut contents, pending_signature.take());
                let text = user_text(*sender, content, &request.agent_id_labels)?;
                contents.push(json!({"role": "user", "parts": [{"text": text}]}));
            }
            ContextBlock::InferenceResponse { items, .. } => {
                for item in items {
                    append_response_item(
                        item,
                        &mut contents,
                        &mut call_names,
                        &mut pending_signature,
                    )?;
                }
            }
            ContextBlock::ToolResults { results } => {
                flush_pending(&mut contents, pending_signature.take());
                for result in results {
                    let id = result.call_id.as_str();
                    let name = call_names.get(id).ok_or_else(|| {
                        anyhow::anyhow!("tool result {id} has no preceding Antigravity tool call")
                    })?;
                    contents.push(json!({
                        "role": "user",
                        "parts": [{"functionResponse": {
                            "id": id,
                            "name": name,
                            "response": {"output": result.body.output.as_str()}
                        }}]
                    }));
                }
            }
            ContextBlock::ToolUpdate(_) => {
                bail!("Antigravity agents do not support tool updates")
            }
            ContextBlock::CompactionTrigger => {
                bail!("Antigravity agents do not support compaction")
            }
        }
    }
    flush_pending(&mut contents, pending_signature);
    if contents.is_empty() {
        bail!("Antigravity request has no content")
    }

    let declarations = request
        .tools
        .iter()
        .map(|tool| {
            if tool.tool_type != ToolType::Function || tool.format.is_some() {
                bail!(
                    "Antigravity supports function tools only: {}",
                    tool.name.as_str()
                )
            }
            Ok(json!({
                "name": tool.name.as_str(),
                "description": tool.description,
                "parameters": tool.input_schema,
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut inner = json!({
        "contents": contents,
        "generationConfig": {
            "thinkingConfig": {"thinkingBudget": 4000, "includeThoughts": false}
        }
    });
    if !request.instructions.trim().is_empty() {
        inner["systemInstruction"] = json!({"parts": [{"text": request.instructions.as_ref()}]});
    }
    if !declarations.is_empty() {
        inner["tools"] = json!([{"functionDeclarations": declarations}]);
        inner["toolConfig"] = json!({"functionCallingConfig": {"mode": "auto"}});
    }

    Ok(json!({
        "model": MODEL_ID,
        "project": project_id,
        "request": inner,
        "requestId": format!("agent-{request_id}"),
        "userAgent": "antigravity",
        "requestType": "agent"
    }))
}

fn user_text(
    sender: MessageSender,
    content: &[ContentPart],
    labels: &std::collections::BTreeMap<rho_core::AgentId, std::sync::Arc<str>>,
) -> Result<String> {
    let mut text = String::new();
    for part in content {
        match part {
            ContentPart::Text { text: part } => text.push_str(part),
            ContentPart::Image { .. } => bail!("Antigravity agents do not support images"),
        }
    }
    if let MessageSender::Agent { id } = sender {
        let sender = labels
            .get(&id)
            .map_or_else(|| id.encoded(), ToString::to_string);
        Ok(format!(
            "Message Type: MESSAGE\nSender: {sender}\nPayload:\n{text}"
        ))
    } else {
        Ok(text)
    }
}

fn append_response_item(
    item: &InferenceResponseItem,
    contents: &mut Vec<Value>,
    call_names: &mut HashMap<String, String>,
    pending: &mut Option<AntigravityProviderData>,
) -> Result<()> {
    match item {
        InferenceResponseItem::AssistantMessage { content, .. } => {
            let mut parts = Vec::new();
            for part in content {
                match part {
                    ContentPart::Text { text } => parts.push(json!({"text": text})),
                    ContentPart::Image { .. } => {
                        bail!("Antigravity agents do not support images")
                    }
                }
            }
            if parts.is_empty() {
                return Ok(());
            }
            attach_or_flush(contents, &mut parts[0], pending);
            contents.push(json!({"role": "model", "parts": parts}));
        }
        InferenceResponseItem::ToolCall {
            id,
            name,
            tool_type,
            arguments,
            ..
        } => {
            if *tool_type != ToolType::Function {
                bail!("Antigravity agents cannot replay custom tool calls")
            }
            let arguments: Value = serde_json::from_str(arguments)
                .with_context(|| format!("invalid arguments for tool call {}", id.as_str()))?;
            let mut part = json!({"functionCall": {
                "id": id.as_str(), "name": name.as_str(), "args": arguments
            }});
            attach_or_flush(contents, &mut part, pending);
            call_names.insert(id.as_str().to_owned(), name.as_str().to_owned());
            contents.push(json!({"role": "model", "parts": [part]}));
        }
        InferenceResponseItem::EncryptedReasoning {
            provider_specific, ..
        } => {
            if let Some(data) = provider_specific
                .as_any()
                .downcast_ref::<AntigravityProviderData>()
            {
                flush_pending(contents, pending.replace(data.clone()));
            }
        }
        InferenceResponseItem::RawReasoning { .. }
        | InferenceResponseItem::Compaction { .. }
        | InferenceResponseItem::Unknown { .. } => {}
    }
    Ok(())
}

fn attach_or_flush(
    contents: &mut Vec<Value>,
    part: &mut Value,
    pending: &mut Option<AntigravityProviderData>,
) {
    let Some(signature) = pending.take() else {
        return;
    };
    if signature.attachment == ThoughtSignatureAttachment::NextPart {
        part["thoughtSignature"] = Value::String(signature.signature);
    } else {
        flush_pending(contents, Some(signature));
    }
}

fn flush_pending(contents: &mut Vec<Value>, pending: Option<AntigravityProviderData>) {
    if let Some(signature) = pending {
        contents.push(json!({
            "role": "model",
            "parts": [{"thought": true, "thoughtSignature": signature.signature}]
        }));
    }
}

pub(crate) fn parse_response(value: Value) -> Result<ParsedResponse> {
    let value = value
        .get("response")
        .ok_or_else(|| anyhow::anyhow!("Antigravity response envelope is missing response"))?;
    let usage = value.get("usageMetadata").map(|usage| TokenUsage {
        input_tokens: usage
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: usage
            .get("cachedContentTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_input_tokens: 0,
        output_tokens: usage
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    });
    let candidates = value
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Antigravity response is missing candidates"))?;
    let mut items = Vec::new();
    for candidate in candidates {
        let parts = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("Antigravity candidate is missing content parts"))?;
        for (part_index, part) in parts.iter().enumerate() {
            let signature = part
                .get("thoughtSignature")
                .and_then(Value::as_str)
                .filter(|signature| !signature.is_empty());
            if let Some(signature) = signature {
                let attachment = if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    ThoughtSignatureAttachment::Standalone
                } else {
                    ThoughtSignatureAttachment::NextPart
                };
                items.push(InferenceResponseItem::EncryptedReasoning {
                    provider_specific: Box::new(AntigravityProviderData {
                        signature: signature.to_owned(),
                        attachment,
                    }),
                    summary: Vec::new(),
                });
            }
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                items.push(InferenceResponseItem::AssistantMessage {
                    provider_specific: pending_provider_data(),
                    content: vec![ContentPart::Text {
                        text: text.to_owned(),
                    }],
                    phase: None,
                });
            } else if let Some(call) = part.get("functionCall") {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Antigravity function call is missing name"))?;
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{name}-{part_index}"));
                items.push(InferenceResponseItem::ToolCall {
                    provider_specific: pending_provider_data(),
                    id: ToolCallId::try_from(id)?,
                    name: ToolName::try_from(name)?,
                    tool_type: ToolType::Function,
                    arguments: serde_json::to_string(
                        call.get("args")
                            .unwrap_or(&Value::Object(Default::default())),
                    )?,
                });
            }
        }
    }
    if items.is_empty() {
        bail!("Antigravity response contained no supported output items")
    }
    Ok(ParsedResponse { items, usage })
}

fn pending_provider_data() -> Box<dyn ProviderSpecificData> {
    Box::new(rho_core::UnknownProviderSpecificData {
        tag: "google.antigravity.pending".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rho_core::{ContextBlock, MessageSender, ToolSpec};

    use super::*;

    #[test]
    fn builds_selected_model_and_thinking_budget() {
        let request = InferenceRequest {
            instructions: Arc::from("be useful"),
            input: vec![Arc::new(ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: vec![ContentPart::Text { text: "hi".into() }],
            })],
            agent_id_labels: Default::default(),
            tools: Arc::from([ToolSpec {
                name: ToolName::try_from("shell").unwrap(),
                tool_type: ToolType::Function,
                description: "run".into(),
                input_schema: json!({"type": "object"}),
                format: None,
            }]),
        };
        let value = build_request("1", "project", &request).unwrap();
        assert_eq!(value["model"], MODEL_ID);
        assert_eq!(
            value["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            4000
        );
    }

    #[test]
    fn response_preserves_attached_signature_before_tool_call() {
        let parsed = parse_response(json!({
            "response": {"candidates": [{"content": {"parts": [{
                "thoughtSignature": "sig",
                "functionCall": {"id": "call-1", "name": "shell", "args": {"x": 1}}
            }]}}]}
        }))
        .unwrap();
        assert!(matches!(
            parsed.items[0],
            InferenceResponseItem::EncryptedReasoning { .. }
        ));
        assert!(matches!(
            parsed.items[1],
            InferenceResponseItem::ToolCall { .. }
        ));
    }

    #[test]
    fn replay_attaches_signature_to_following_function_call() {
        let request = InferenceRequest {
            instructions: Arc::from(""),
            input: vec![
                Arc::new(ContextBlock::UserMessage {
                    sender: MessageSender::User,
                    content: vec![ContentPart::Text {
                        text: "do it".into(),
                    }],
                }),
                Arc::new(ContextBlock::InferenceResponse {
                    items: vec![
                        InferenceResponseItem::EncryptedReasoning {
                            provider_specific: Box::new(AntigravityProviderData {
                                signature: "signature".into(),
                                attachment: ThoughtSignatureAttachment::NextPart,
                            }),
                            summary: Vec::new(),
                        },
                        InferenceResponseItem::ToolCall {
                            provider_specific: pending_provider_data(),
                            id: ToolCallId::try_from("call-1").unwrap(),
                            name: ToolName::try_from("shell").unwrap(),
                            tool_type: ToolType::Function,
                            arguments: "{\"command\":\"pwd\"}".into(),
                        },
                    ],
                    provider_response_id: None,
                }),
            ],
            agent_id_labels: Default::default(),
            tools: Arc::from([]),
        };
        let value = build_request("1", "project", &request).unwrap();
        assert_eq!(
            value["request"]["contents"][1]["parts"][0]["thoughtSignature"],
            "signature"
        );
        assert_eq!(
            value["request"]["contents"][1]["parts"][0]["functionCall"]["id"],
            "call-1"
        );
    }

    #[test]
    fn rejects_custom_tools_and_images_before_transport() {
        let image = InferenceRequest {
            instructions: Arc::from(""),
            input: vec![Arc::new(ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: vec![ContentPart::Image {
                    media_type: "image/png".into(),
                    data: vec![1],
                }],
            })],
            agent_id_labels: Default::default(),
            tools: Arc::from([]),
        };
        assert!(build_request("1", "project", &image).is_err());

        let custom = InferenceRequest {
            instructions: Arc::from(""),
            input: vec![Arc::new(ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: vec![ContentPart::Text { text: "hi".into() }],
            })],
            agent_id_labels: Default::default(),
            tools: Arc::from([ToolSpec {
                name: ToolName::try_from("patch").unwrap(),
                tool_type: ToolType::Custom,
                description: "patch".into(),
                input_schema: json!({}),
                format: None,
            }]),
        };
        assert!(build_request("1", "project", &custom).is_err());
    }
}
