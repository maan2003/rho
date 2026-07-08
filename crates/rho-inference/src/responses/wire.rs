//! Translation between rho-core's provider-neutral types and the OpenAI
//! Responses API wire format.

use std::sync::Arc;

use anyhow::{Result, bail};
use rho_core::{
    AppendString, ContentPart, ContextBlock, ContextItemEvent, InferenceEvent, InferenceRequest,
    InferenceResponseItem, MessagePhase, ProviderResponseId, ProviderResponseItemId,
    ProviderSpecificData, StreamingContextItem, TokenUsage, ToolCall, ToolCallId, ToolFormat,
    ToolGrammarSyntax, ToolName, ToolResult, ToolSpec, ToolType, text_content,
};
use senax_encoder::{Decode, Decoder, Encode, TaggedSenax};
use serde::Serialize;
use serde_json::{Value, json};

use super::InferenceSession;
use super::session::{
    AutoCompaction, ReasoningContext, ResponsesEffort, ServiceTier, TextVerbosity,
};

#[derive(Clone, Debug, PartialEq, Eq, Encode)]
pub enum OpenAiResponsesProviderData {
    Message {
        item_id: ProviderResponseItemId,
    },
    FunctionCall {
        item_id: ProviderResponseItemId,
    },
    CustomToolCall {
        item_id: ProviderResponseItemId,
    },
    EncryptedReasoning {
        item_id: ProviderResponseItemId,
        encrypted_content: String,
    },
    Compaction {
        item_id: ProviderResponseItemId,
        encrypted_content: String,
    },
}

impl Decoder for OpenAiResponsesProviderData {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        // Temporary migration compatibility: old dev databases encoded the
        // encrypted reasoning provider item as `Reasoning`. Decode that old
        // variant into the current enum, but do not keep it in the real
        // runtime representation. Remove this shim with the b8c0d4e1 agent DB
        // migration.
        #[derive(Decode)]
        enum TemporaryOpenAiResponsesProviderDataDecode {
            Message {
                item_id: ProviderResponseItemId,
            },
            FunctionCall {
                item_id: ProviderResponseItemId,
            },
            CustomToolCall {
                item_id: ProviderResponseItemId,
            },
            EncryptedReasoning {
                item_id: ProviderResponseItemId,
                encrypted_content: String,
            },
            #[senax(rename = "Reasoning")]
            LegacyReasoning {
                item_id: ProviderResponseItemId,
                encrypted_content: String,
            },
            Compaction {
                item_id: ProviderResponseItemId,
                encrypted_content: String,
            },
        }

        Ok(
            match TemporaryOpenAiResponsesProviderDataDecode::decode(reader)? {
                TemporaryOpenAiResponsesProviderDataDecode::Message { item_id } => {
                    Self::Message { item_id }
                }
                TemporaryOpenAiResponsesProviderDataDecode::FunctionCall { item_id } => {
                    Self::FunctionCall { item_id }
                }
                TemporaryOpenAiResponsesProviderDataDecode::CustomToolCall { item_id } => {
                    Self::CustomToolCall { item_id }
                }
                TemporaryOpenAiResponsesProviderDataDecode::EncryptedReasoning {
                    item_id,
                    encrypted_content,
                }
                | TemporaryOpenAiResponsesProviderDataDecode::LegacyReasoning {
                    item_id,
                    encrypted_content,
                } => Self::EncryptedReasoning {
                    item_id,
                    encrypted_content,
                },
                TemporaryOpenAiResponsesProviderDataDecode::Compaction {
                    item_id,
                    encrypted_content,
                } => Self::Compaction {
                    item_id,
                    encrypted_content,
                },
            },
        )
    }
}

impl senax_encoder::TaggedSenax for OpenAiResponsesProviderData {
    const TAG: &'static str = "openai.responses.item";
}

senax_encoder::__private::inventory::submit! {
    rho_core::__SenaxProviderSpecificDataEntry::new(
        OpenAiResponsesProviderData::TAG,
        |mut body: bytes::Bytes| -> senax_encoder::Result<Box<dyn ProviderSpecificData>> {
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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ResponsesRequest {
    pub model: String,
    pub instructions: Arc<str>,
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<&'static str>,
    pub prompt_cache_key: uuid::Uuid,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_management: Vec<ContextManagementRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TextRequest {
    pub verbosity: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ReasoningRequest {
    pub context: &'static str,
    pub effort: &'static str,
    pub summary: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ContextManagementRequest {
    #[serde(rename = "type")]
    pub ty: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<u64>,
}

impl ResponsesRequest {
    pub(crate) fn from_inference_request(
        session: &InferenceSession,
        request: InferenceRequest,
        cached_response_id: Option<&str>,
    ) -> Self {
        let mut previous_response = None;
        if let Some(cached_response_id) = cached_response_id {
            for (index, block) in request.input.iter().enumerate() {
                let ContextBlock::InferenceResponse {
                    provider_response_id,
                    items,
                } = &**block
                else {
                    continue;
                };

                let has_compaction = items
                    .iter()
                    .any(|item| matches!(item, InferenceResponseItem::Compaction { .. }));

                if has_compaction {
                    previous_response = None;
                } else if provider_response_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == cached_response_id)
                {
                    previous_response = provider_response_id
                        .as_ref()
                        .map(|id| (id.as_str().to_owned(), index + 1));
                }
            }
        }

        Self::from_inference_request_with_previous(session, request, previous_response)
    }

    fn from_inference_request_with_previous(
        session: &InferenceSession,
        request: InferenceRequest,
        previous_response: Option<(String, usize)>,
    ) -> Self {
        let input_blocks = if let Some((_, next_block_index)) = previous_response.as_ref() {
            &request.input[*next_block_index..]
        } else {
            request.input.as_slice()
        };

        // Flatten the heterogeneous blocks into a single item timeline, then
        // drop everything before the most recent compaction marker.
        let mut timeline = Vec::new();
        for block in input_blocks {
            append_block_items(block, &request.agent_id_labels, &mut timeline);
        }
        let timeline = trim_before_latest_compaction(&timeline);

        let tools = request
            .tools
            .iter()
            .cloned()
            .map(convert_tool_spec)
            .collect::<Vec<_>>();
        let mut input = Vec::new();
        for item in timeline {
            convert_timeline_item(item.clone(), &mut input);
        }

        let tool_choice = (!tools.is_empty()).then_some("auto");
        let prompt_cache_key = session
            .prompt_cache_key
            .to_wire_uuid(&session.base_url, [0; 32]);
        let previous_response_id = previous_response.map(|(id, _)| id);
        let config = &session.responses_config;
        let context_management = config
            .auto_compaction
            .as_ref()
            .map(|compaction| {
                let compact_threshold = match compaction {
                    AutoCompaction::Threshold(value) => *value,
                };
                vec![ContextManagementRequest {
                    ty: "compaction",
                    compact_threshold: Some(compact_threshold),
                }]
            })
            .unwrap_or_default();

        Self {
            model: config.model.as_str().to_owned(),
            instructions: request.instructions,
            input,
            store: Some(false),
            tools,
            tool_choice,
            text: Some(TextRequest {
                verbosity: match config.text_verbosity {
                    TextVerbosity::Low => "low",
                    #[cfg(test)]
                    TextVerbosity::Medium => "medium",
                },
            }),
            reasoning: Some(ReasoningRequest {
                context: match config.reasoning_context {
                    ReasoningContext::CurrentTurn => "current_turn",
                    ReasoningContext::AllTurns => "all_turns",
                },
                effort: match config.effort {
                    ResponsesEffort::Low => "low",
                    ResponsesEffort::Medium => "medium",
                    ResponsesEffort::Xhigh => "xhigh",
                },
                summary: "auto",
            }),
            service_tier: Some(match config.service_tier {
                ServiceTier::Priority => "priority",
                ServiceTier::Normal => "default",
            }),
            include: vec!["reasoning.encrypted_content"],
            prompt_cache_key,
            context_management,
            previous_response_id,
        }
    }
}

#[derive(Clone)]
enum WireTimelineItem {
    UserMessage(Vec<ContentPart>),
    CompactionTrigger,
    ToolResult(ToolResult),
    ResponseItem(InferenceResponseItem),
}

fn append_block_items(
    block: &ContextBlock,
    agent_id_labels: &std::collections::BTreeMap<rho_core::AgentId, std::sync::Arc<str>>,
    out: &mut Vec<WireTimelineItem>,
) {
    match block {
        ContextBlock::UserMessage { sender, content } => match sender {
            rho_core::MessageSender::User => {
                out.push(WireTimelineItem::UserMessage(content.clone()));
            }
            // Agent mail rides the user role; the header identifies the
            // sender so the model can tell peers from the actual user.
            rho_core::MessageSender::Agent { id } => {
                let sender = agent_id_labels
                    .get(id)
                    .map_or_else(|| format!("ag-{}", id.encoded()), ToString::to_string);
                let text = format!(
                    "Message Type: MESSAGE\nSender: {sender}\nPayload:\n{}",
                    rho_core::text_content(content)
                );
                out.push(WireTimelineItem::UserMessage(vec![
                    rho_core::ContentPart::Text { text },
                ]));
            }
        },
        ContextBlock::CompactionTrigger => {
            out.push(WireTimelineItem::CompactionTrigger);
        }
        ContextBlock::ToolResults { results } => {
            out.extend(results.iter().cloned().map(WireTimelineItem::ToolResult));
        }
        ContextBlock::InferenceResponse { items, .. } => {
            out.extend(items.iter().cloned().map(WireTimelineItem::ResponseItem))
        }
    }
}

fn convert_timeline_item(item: WireTimelineItem, out: &mut Vec<Value>) {
    match item {
        WireTimelineItem::UserMessage(content) => convert_user_message(&content, out),
        WireTimelineItem::CompactionTrigger => out.push(json!({
            "type": "compaction_trigger",
        })),
        WireTimelineItem::ToolResult(result) => out.push(convert_tool_result(result)),
        WireTimelineItem::ResponseItem(item) => convert_response_item(item, out),
    }
}

fn convert_response_item(item: InferenceResponseItem, out: &mut Vec<Value>) {
    match item {
        InferenceResponseItem::AssistantMessage {
            provider_specific,
            content,
            phase,
        } => {
            if let Some(OpenAiResponsesProviderData::Message { item_id }) = provider_specific
                .as_any()
                .downcast_ref::<OpenAiResponsesProviderData>()
            {
                let mut item = assistant_message_value(&content, phase);
                item["id"] = json!(item_id.as_str());
                out.push(item);
            } else {
                convert_assistant_message(&content, phase, out);
            }
        }
        InferenceResponseItem::ToolCall {
            provider_specific,
            id,
            name,
            tool_type,
            arguments,
        } => {
            if let Some(data) = provider_specific
                .as_any()
                .downcast_ref::<OpenAiResponsesProviderData>()
            {
                match (data, tool_type) {
                    (OpenAiResponsesProviderData::FunctionCall { item_id }, ToolType::Function)
                    | (OpenAiResponsesProviderData::CustomToolCall { item_id }, ToolType::Custom) => {
                        convert_tool_call_with_item_id(
                            ToolCall {
                                id,
                                name,
                                tool_type,
                                arguments,
                            },
                            item_id,
                            out,
                        )
                    }
                    _ => convert_tool_call(
                        ToolCall {
                            id,
                            name,
                            tool_type,
                            arguments,
                        },
                        out,
                    ),
                }
            } else {
                convert_tool_call(
                    ToolCall {
                        id,
                        name,
                        tool_type,
                        arguments,
                    },
                    out,
                );
            }
        }
        InferenceResponseItem::EncryptedReasoning {
            provider_specific,
            summary,
        } => {
            push_reasoning_provider_specific(provider_specific, &summary, out);
        }
        InferenceResponseItem::Compaction {
            provider_specific, ..
        }
        | InferenceResponseItem::Unknown {
            provider_specific, ..
        } => {
            push_provider_specific(provider_specific, out);
        }
        InferenceResponseItem::RawReasoning { .. } => {}
    }
}

fn push_reasoning_provider_specific(
    provider_specific: Box<dyn ProviderSpecificData>,
    summary: &[String],
    out: &mut Vec<Value>,
) -> bool {
    let Some(data) = provider_specific
        .as_any()
        .downcast_ref::<OpenAiResponsesProviderData>()
    else {
        return false;
    };
    let (item_id, encrypted_content) = match data {
        OpenAiResponsesProviderData::EncryptedReasoning {
            item_id,
            encrypted_content,
        } => (item_id, encrypted_content),
        _ => return false,
    };
    if encrypted_content.is_empty() {
        return false;
    }
    let summary = summary
        .iter()
        .map(|text| json!({"type": "summary_text", "text": text}))
        .collect::<Vec<_>>();
    out.push(json!({
        "type": "reasoning",
        "id": item_id.as_str(),
        "encrypted_content": encrypted_content,
        "summary": summary,
    }));
    true
}

fn push_provider_specific(
    provider_specific: Box<dyn ProviderSpecificData>,
    out: &mut Vec<Value>,
) -> bool {
    let Some(data) = provider_specific
        .as_any()
        .downcast_ref::<OpenAiResponsesProviderData>()
    else {
        return false;
    };
    match data {
        OpenAiResponsesProviderData::Compaction {
            item_id,
            encrypted_content,
        } if !encrypted_content.is_empty() => out.push(json!({
            "type": "compaction",
            "id": item_id.as_str(),
            "encrypted_content": encrypted_content,
        })),
        _ => return false,
    }
    true
}

#[cfg(test)]
pub(crate) fn openai_provider_specific_data(
    provider_specific: &dyn ProviderSpecificData,
) -> Option<&OpenAiResponsesProviderData> {
    provider_specific
        .as_any()
        .downcast_ref::<OpenAiResponsesProviderData>()
}

fn convert_tool_call(call: ToolCall, out: &mut Vec<Value>) {
    let item_id = if call.id.as_str().starts_with(match call.tool_type {
        ToolType::Function => "fc_",
        ToolType::Custom => "ctc_",
    }) {
        ProviderResponseItemId::try_from(call.id.as_str()).ok()
    } else {
        None
    };
    convert_tool_call_with_optional_item_id(call, item_id.as_ref(), out)
}

fn convert_tool_call_with_item_id(
    call: ToolCall,
    item_id: &ProviderResponseItemId,
    out: &mut Vec<Value>,
) {
    convert_tool_call_with_optional_item_id(call, Some(item_id), out)
}

fn convert_tool_call_with_optional_item_id(
    call: ToolCall,
    item_id: Option<&ProviderResponseItemId>,
    out: &mut Vec<Value>,
) {
    let call_id = call.id.as_str();
    match call.tool_type {
        ToolType::Function => {
            let id = item_id
                .map(ProviderResponseItemId::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("fc_{call_id}"));
            out.push(json!({
                "type": "function_call",
                "id": id,
                "call_id": call_id,
                "name": call.name.as_str(),
                "arguments": call.arguments,
            }));
        }
        ToolType::Custom => {
            let id = item_id
                .map(ProviderResponseItemId::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("ctc_{call_id}"));
            out.push(json!({
                "type": "custom_tool_call",
                "id": id,
                "call_id": call_id,
                "name": call.name.as_str(),
                "input": call.arguments,
            }));
        }
    }
}

fn convert_user_message(content: &[ContentPart], out: &mut Vec<Value>) {
    out.push(json!({
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": text_content(content),
        }],
    }));
}

fn convert_assistant_message(
    content: &[ContentPart],
    phase: Option<MessagePhase>,
    out: &mut Vec<Value>,
) {
    out.push(assistant_message_value(content, phase));
}

fn assistant_message_value(content: &[ContentPart], phase: Option<MessagePhase>) -> Value {
    let mut item = json!({
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text_content(content),
            "annotations": [],
        }],
    });
    item["phase"] = json!(message_phase_wire(
        phase.unwrap_or(MessagePhase::FinalAnswer)
    ));
    item
}

fn message_phase_wire(phase: MessagePhase) -> &'static str {
    match phase {
        MessagePhase::Commentary => "commentary",
        MessagePhase::FinalAnswer => "final_answer",
    }
}

fn convert_tool_result(result: ToolResult) -> Value {
    let output_type = match result.tool_type {
        ToolType::Function => "function_call_output",
        ToolType::Custom => "custom_tool_call_output",
    };
    json!({
        "type": output_type,
        "call_id": result.call_id.as_str(),
        "output": result.body.output.as_ref(),
    })
}

fn convert_tool_spec(tool: ToolSpec) -> Value {
    let mut wire = match tool.tool_type {
        ToolType::Function => json!({
            "type": "function",
            "name": tool.name.as_str(),
            "strict": Value::Null,
            "description": tool.description,
            "parameters": tool.input_schema,
        }),
        ToolType::Custom => {
            let mut wire = json!({
                "type": "custom",
                "name": tool.name.as_str(),
                "description": tool.description,
            });
            if let Some(format) = tool.format {
                wire["format"] = convert_tool_format(format);
            }
            wire
        }
    };
    if wire["description"].as_str().is_some_and(str::is_empty) {
        wire.as_object_mut().expect("object").remove("description");
    }
    wire
}

fn convert_tool_format(format: ToolFormat) -> Value {
    match format {
        ToolFormat::Text => json!({
            "type": "text",
        }),
        ToolFormat::Grammar { syntax, definition } => json!({
            "type": "grammar",
            "syntax": match syntax {
                ToolGrammarSyntax::Lark => "lark",
                ToolGrammarSyntax::Regex => "regex",
            },
            "definition": definition,
        }),
    }
}

fn trim_before_latest_compaction(timeline: &[WireTimelineItem]) -> &[WireTimelineItem] {
    timeline
        .iter()
        .rposition(|item| {
            matches!(
                item,
                WireTimelineItem::ResponseItem(InferenceResponseItem::Compaction { .. })
            )
        })
        .map_or(timeline, |index| &timeline[index..])
}

/// Translates the Responses API event stream into provider-neutral
/// [`InferenceEvent`]s. It owns the streaming accumulators — one
/// [`AppendString`] per growing field, keyed by the wire's `output_index` — so
/// a wire delta is pushed into its buffer and re-emitted as a whole
/// [`StreamingContextItem`] snapshot via [`ContextItemEvent::Update`]; the
/// matching `output_item.done` emits any terminal-only form, then
/// [`ContextItemEvent::Finish`].
#[derive(Default)]
pub(crate) struct ResponseState {
    builders: Vec<Option<ItemBuilder>>,
}

/// Per-item accumulation mirroring [`StreamingContextItem`], but holding the
/// growing [`AppendString`] buffers each snapshot is taken from.
enum ItemBuilder {
    Message {
        provider_specific: Box<dyn ProviderSpecificData>,
        phase: Option<MessagePhase>,
        content: Vec<AppendString>,
    },
    ToolCall {
        provider_specific: Box<dyn ProviderSpecificData>,
        id: ToolCallId,
        name: ToolName,
        tool_type: ToolType,
        arguments: AppendString,
    },
    Reasoning {
        provider_specific: Box<dyn ProviderSpecificData>,
        content: Option<AppendString>,
        summary: Vec<AppendString>,
    },
    Compaction {
        provider_specific: Box<dyn ProviderSpecificData>,
    },
}

impl ItemBuilder {
    fn snapshot(&self) -> StreamingContextItem {
        match self {
            ItemBuilder::Message {
                provider_specific,
                phase,
                content,
            } => StreamingContextItem::AssistantMessage {
                provider_specific: provider_specific.clone(),
                content: content.iter().map(AppendString::snapshot).collect(),
                phase: *phase,
            },
            ItemBuilder::ToolCall {
                provider_specific,
                id,
                name,
                tool_type,
                arguments,
            } => StreamingContextItem::ToolCall {
                provider_specific: provider_specific.clone(),
                id: id.clone(),
                name: name.clone(),
                tool_type: *tool_type,
                arguments: arguments.snapshot(),
            },
            ItemBuilder::Reasoning {
                provider_specific,
                content,
                summary,
            } => StreamingContextItem::RawReasoning {
                provider_specific: provider_specific.clone(),
                content: content
                    .as_ref()
                    .map(AppendString::snapshot)
                    .unwrap_or_else(|| AppendString::new().snapshot()),
                summary: summary.iter().map(AppendString::snapshot).collect(),
            },
            ItemBuilder::Compaction { provider_specific } => StreamingContextItem::Compaction {
                provider_specific: provider_specific.clone(),
            },
        }
    }
}

impl ResponseState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn apply_event(&mut self, event: &Value) -> Result<(bool, Vec<InferenceEvent>)> {
        let mut updates = Vec::new();
        let index = output_index(event);
        match event["type"].as_str().unwrap_or_default() {
            "response.output_item.added" => {
                if let Some(item) = event.get("item")
                    && let Some(builder) = builder_from_added(item)?
                {
                    self.set_builder(index, builder);
                    self.emit_update(index, &mut updates);
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    if let Some(ItemBuilder::Message { content, .. }) = self.builder_mut(index) {
                        let part = event["content_index"].as_u64().unwrap_or(0) as usize;
                        push_part(content, part, delta);
                    }
                    self.emit_update(index, &mut updates);
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    if let Some(ItemBuilder::Reasoning { summary, .. }) = self.builder_mut(index) {
                        let part = event["summary_index"].as_u64().unwrap_or(0) as usize;
                        push_part(summary, part, delta);
                    }
                    self.emit_update(index, &mut updates);
                }
            }
            "response.reasoning_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    if let Some(ItemBuilder::Reasoning { content, .. }) = self.builder_mut(index) {
                        content
                            .get_or_insert_with(AppendString::new)
                            .push_str(delta);
                    }
                    self.emit_update(index, &mut updates);
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    if let Some(ItemBuilder::ToolCall { arguments, .. }) = self.builder_mut(index) {
                        arguments.push_str(delta);
                    }
                    self.emit_update(index, &mut updates);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    self.finish_item(index, item, &mut updates);
                }
            }
            "response.completed" | "response.done" => {
                let usage = usage_from_event(event);
                let provider_response_id = event
                    .get("response")
                    .and_then(|response| response["id"].as_str())
                    .or_else(|| event["id"].as_str())
                    .and_then(|id| ProviderResponseId::try_from(id).ok());
                updates.push(InferenceEvent::Finished {
                    usage,
                    provider_response_id,
                });
                return Ok((true, updates));
            }
            "response.incomplete" => {
                let reason = event
                    .get("response")
                    .and_then(|response| response["incomplete_details"]["reason"].as_str())
                    .unwrap_or("unknown reason");
                bail!("response incomplete: {reason}");
            }
            "response.failed" => {
                let detail = event
                    .get("response")
                    .and_then(|response| {
                        response["error"]["message"]
                            .as_str()
                            .or_else(|| response["error"]["code"].as_str())
                    })
                    .unwrap_or("unknown error");
                bail!("response failed: {detail}");
            }
            "error" => {
                let detail = event["error"]["message"]
                    .as_str()
                    .or_else(|| event["message"].as_str())
                    .unwrap_or("unknown error");
                let error_code = event["error"]["code"]
                    .as_str()
                    .or_else(|| event["code"].as_str())
                    .or_else(|| event["error"]["type"].as_str());
                match error_code {
                    Some(code) => bail!("stream error: {detail} (type={code})"),
                    None => bail!("stream error: {detail}"),
                }
            }
            _ => {}
        }

        Ok((false, updates))
    }
}

impl ResponseState {
    fn builder(&self, index: usize) -> Option<&ItemBuilder> {
        self.builders.get(index).and_then(Option::as_ref)
    }

    fn builder_mut(&mut self, index: usize) -> Option<&mut ItemBuilder> {
        self.builders.get_mut(index).and_then(Option::as_mut)
    }

    fn set_builder(&mut self, index: usize, builder: ItemBuilder) {
        if self.builders.len() <= index {
            self.builders.resize_with(index + 1, || None);
        }
        self.builders[index] = Some(builder);
    }

    /// Emit the current snapshot of the builder at `index`, if one exists.
    fn emit_update(&self, index: usize, updates: &mut Vec<InferenceEvent>) {
        if let Some(builder) = self.builder(index) {
            updates.push(InferenceEvent::ContextItem {
                index,
                event: ContextItemEvent::Update(builder.snapshot()),
            });
        }
    }

    /// Close out the item at `index`. Items whose final form only exists at
    /// `done` (encrypted reasoning, the compaction payload, unrecognized items)
    /// get one last `Update` carrying it; then every item gets `Finish`.
    fn finish_item(&mut self, index: usize, item: &Value, updates: &mut Vec<InferenceEvent>) {
        if item["type"].as_str() == Some("message")
            && let Some(done_phase) = message_phase_from_output_item(item)
            && let Some(ItemBuilder::Message { phase, .. }) = self.builder_mut(index)
            && *phase != Some(done_phase)
        {
            *phase = Some(done_phase);
            self.emit_update(index, updates);
        }

        match item["type"].as_str().unwrap_or_default() {
            "message" => {
                if let Some(ItemBuilder::Message {
                    provider_specific, ..
                }) = self.builder_mut(index)
                {
                    *provider_specific = Box::new(openai_provider_data_from_item(item));
                    self.emit_update(index, updates);
                }
            }
            "function_call" | "custom_tool_call" => {
                if let Some(ItemBuilder::ToolCall {
                    provider_specific, ..
                }) = self.builder_mut(index)
                {
                    *provider_specific = Box::new(openai_provider_data_from_item(item));
                    self.emit_update(index, updates);
                }
            }
            "reasoning" => {
                if item["encrypted_content"].is_string()
                    && let Some(ItemBuilder::Reasoning {
                        provider_specific, ..
                    }) = self.builder_mut(index)
                {
                    *provider_specific = Box::new(openai_provider_data_from_item(item));
                }
            }
            "compaction" => {
                if item["encrypted_content"].is_string()
                    && let Some(ItemBuilder::Compaction { provider_specific }) =
                        self.builder_mut(index)
                {
                    *provider_specific = Box::new(openai_provider_data_from_item(item));
                }
            }
            _ => {}
        }

        let terminal = match item["type"].as_str().unwrap_or_default() {
            "message" | "function_call" | "custom_tool_call" => None,
            "reasoning" if item["encrypted_content"].is_string() => {
                let summary = match self.builder(index) {
                    Some(ItemBuilder::Reasoning { summary, .. }) => {
                        summary.iter().map(AppendString::snapshot).collect()
                    }
                    _ => Vec::new(),
                };
                Some(StreamingContextItem::EncryptedReasoning {
                    provider_specific: Box::new(openai_provider_data_from_item(item)),
                    summary,
                })
            }
            "reasoning" => None,
            "compaction" if item["encrypted_content"].is_string() => {
                Some(StreamingContextItem::Compaction {
                    provider_specific: Box::new(openai_provider_data_from_item(item)),
                })
            }
            "compaction" => None,
            _other => None,
        };
        if let Some(snapshot) = terminal {
            updates.push(InferenceEvent::ContextItem {
                index,
                event: ContextItemEvent::Update(snapshot),
            });
        }
        updates.push(InferenceEvent::ContextItem {
            index,
            event: ContextItemEvent::Finish,
        });
    }
}

fn output_index(event: &Value) -> usize {
    event["output_index"].as_u64().unwrap_or(0) as usize
}

fn openai_item_id(item: &Value) -> ProviderResponseItemId {
    ProviderResponseItemId::try_from(
        item["id"]
            .as_str()
            .expect("OpenAI output item missing required id"),
    )
    .expect("OpenAI output item has invalid id")
}

fn openai_provider_data_from_item(item: &Value) -> OpenAiResponsesProviderData {
    let item_id = openai_item_id(item);
    match item["type"].as_str().unwrap_or_default() {
        "message" => OpenAiResponsesProviderData::Message { item_id },
        "function_call" => OpenAiResponsesProviderData::FunctionCall { item_id },
        "custom_tool_call" => OpenAiResponsesProviderData::CustomToolCall { item_id },
        "reasoning" => OpenAiResponsesProviderData::EncryptedReasoning {
            item_id,
            encrypted_content: item["encrypted_content"]
                .as_str()
                .expect("OpenAI reasoning item missing encrypted_content")
                .to_owned(),
        },
        "compaction" => OpenAiResponsesProviderData::Compaction {
            item_id,
            encrypted_content: item["encrypted_content"]
                .as_str()
                .expect("OpenAI compaction item missing encrypted_content")
                .to_owned(),
        },
        other => panic!("unexpected OpenAI output item type: {other}"),
    }
}

fn pending_openai_provider_data() -> Box<dyn ProviderSpecificData> {
    Box::new(rho_core::UnknownProviderSpecificData {
        tag: "openai.responses.pending".to_owned(),
    })
}

/// Appends `delta` to the `part`th buffer, growing the vec with empty buffers
/// to cover any skipped indices.
fn push_part(parts: &mut Vec<AppendString>, part: usize, delta: &str) {
    while parts.len() <= part {
        parts.push(AppendString::new());
    }
    parts[part].push_str(delta);
}

/// Builds the accumulator for an item we just saw begin streaming. Returns
/// `None` for item types we don't recognize; those are captured whole when
/// their `response.output_item.done` arrives (see
/// [`ResponseState::finish_item`]).
fn builder_from_added(item: &Value) -> Result<Option<ItemBuilder>> {
    let builder = match item["type"].as_str().unwrap_or_default() {
        "message" => ItemBuilder::Message {
            provider_specific: Box::new(openai_provider_data_from_item(item)),
            phase: message_phase_from_output_item(item),
            content: Vec::new(),
        },
        "function_call" => tool_call_builder(item, ToolType::Function)?,
        "custom_tool_call" => tool_call_builder(item, ToolType::Custom)?,
        "reasoning" => ItemBuilder::Reasoning {
            provider_specific: if item["encrypted_content"].is_string() {
                Box::new(openai_provider_data_from_item(item))
            } else {
                pending_openai_provider_data()
            },
            content: None,
            summary: Vec::new(),
        },
        "compaction" => ItemBuilder::Compaction {
            provider_specific: if item["encrypted_content"].is_string() {
                Box::new(openai_provider_data_from_item(item))
            } else {
                pending_openai_provider_data()
            },
        },
        _ => return Ok(None),
    };
    Ok(Some(builder))
}

fn tool_call_builder(item: &Value, tool_type: ToolType) -> Result<ItemBuilder> {
    Ok(ItemBuilder::ToolCall {
        provider_specific: Box::new(openai_provider_data_from_item(item)),
        id: ToolCallId::try_from(item["call_id"].as_str().unwrap_or_default())?,
        name: ToolName::try_from(item["name"].as_str().unwrap_or_default())?,
        tool_type,
        arguments: AppendString::new(),
    })
}

fn message_phase_from_output_item(item: &Value) -> Option<MessagePhase> {
    if item.get("type").and_then(Value::as_str)? != "message" {
        return None;
    }
    match item.get("phase")?.as_str()? {
        "commentary" => Some(MessagePhase::Commentary),
        "final_answer" => Some(MessagePhase::FinalAnswer),
        _ => None,
    }
}

fn usage_from_event(event: &Value) -> Option<TokenUsage> {
    let usage = event
        .get("response")
        .and_then(|response| response.get("usage"))
        .or_else(|| event.get("usage"))?;
    let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
    let cached_input_tokens = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    let output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
    if input_tokens == 0 && cached_input_tokens == 0 && output_tokens == 0 {
        None
    } else {
        Some(TokenUsage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
        })
    }
}
