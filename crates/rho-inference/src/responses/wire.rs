use std::collections::BTreeMap;

use anyhow::{Result, bail};
use rho_core::{
    ContextBlock, InferenceRequest, InferenceResponse, InferenceUpdate, ItemKind, Message,
    MessagePhase, ProviderItem, ProviderItemKind, ReasoningItem, ReasoningTextKind, Role,
    TokenUsage, ToolCall, ToolCallId, ToolFormat, ToolGrammarSyntax, ToolResult, ToolSpec,
    ToolType,
};
use serde::Serialize;
use serde_json::{Value, json};

use super::{Compaction, InferenceSession, encode_tool_name};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ResponsesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextRequest>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
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
    ) -> Self {
        let mut previous_response = None;
        for (index, block) in request.input.iter().enumerate() {
            let ContextBlock::InferenceResponse {
                provider_response_id,
                items,
            } = block
            else {
                continue;
            };

            let has_compaction = items.iter().any(|item| {
                matches!(
                    &item.kind,
                    ItemKind::ProviderItem(provider_item)
                        if provider_item.kind == ProviderItemKind::Compaction
                )
            });

            if has_compaction {
                previous_response = None;
            } else {
                previous_response = provider_response_id.clone().map(|id| (id, index + 1));
            }
        }

        Self::from_inference_request_with_previous(session, request, previous_response)
    }

    pub(crate) fn from_inference_request_full_replay(
        session: &InferenceSession,
        request: InferenceRequest,
    ) -> Self {
        Self::from_inference_request_with_previous(session, request, None)
    }

    fn from_inference_request_with_previous(
        session: &InferenceSession,
        request: InferenceRequest,
        previous_response: Option<(String, usize)>,
    ) -> Self {
        let instructions = request
            .input
            .iter()
            .flat_map(|block| match block {
                ContextBlock::Local { items } | ContextBlock::InferenceResponse { items, .. } => {
                    items
                }
            })
            .filter_map(|item| instruction_text_from_item(&item.kind))
            .collect::<Vec<_>>();
        let mut input = Vec::new();
        let input_blocks = if let Some((_, next_block_index)) = previous_response.as_ref() {
            &request.input[*next_block_index..]
        } else {
            request.input.as_slice()
        };
        let input_items = input_blocks
            .iter()
            .flat_map(|block| match block {
                ContextBlock::Local { items } | ContextBlock::InferenceResponse { items, .. } => {
                    items
                }
            })
            .map(|item| item.kind.clone())
            .collect::<Vec<_>>();
        let input_items = trim_before_latest_compaction(&input_items);
        let local_tool_wire_names = request
            .tools
            .iter()
            .map(|tool| (tool.name.clone(), encode_tool_name(&tool.name)))
            .collect::<BTreeMap<_, _>>();
        let tools = request
            .tools
            .into_iter()
            .map(convert_tool_spec)
            .collect::<Vec<_>>();
        for item in input_items.iter().cloned() {
            convert_item_kind(&local_tool_wire_names, item, &mut input);
        }
        let tool_choice = (!tools.is_empty()).then_some("auto");
        let prompt_cache_key = session.prompt_cache_key.clone();
        let previous_response_id = previous_response.map(|(id, _)| id);
        let context_management = session
            .compaction
            .map(|compaction| {
                let compact_threshold = match compaction {
                    Compaction::Default => provider_default_compaction_threshold(),
                    Compaction::Threshold(value) => value,
                };
                vec![ContextManagementRequest {
                    ty: "compaction",
                    compact_threshold: Some(compact_threshold),
                }]
            })
            .unwrap_or_default();

        Self {
            model: session.model.clone(),
            instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
            input,
            store: Some(false),
            tools,
            tool_choice,
            text: Some(TextRequest {
                verbosity: "medium",
            }),
            include: vec!["reasoning.encrypted_content"],
            prompt_cache_key,
            context_management,
            previous_response_id,
        }
    }
}

fn instruction_text_from_item(item: &ItemKind) -> Option<String> {
    match item {
        ItemKind::Message(message) if matches!(message.role, Role::System | Role::Developer) => {
            Some(message.text_content())
        }
        _ => None,
    }
}

fn convert_item_kind(
    local_tool_wire_names: &BTreeMap<String, String>,
    item: ItemKind,
    out: &mut Vec<Value>,
) {
    match item {
        ItemKind::Message(message) => convert_message(message, out),
        ItemKind::ToolCall(call) => {
            let wire_name = local_tool_wire_names
                .get(&call.name)
                .cloned()
                .unwrap_or_else(|| encode_tool_name(&call.name));
            let call_id = call.id.0;
            match call.tool_type {
                ToolType::Function => {
                    let id = if call_id.starts_with("fc_") {
                        call_id.clone()
                    } else {
                        format!("fc_{call_id}")
                    };
                    out.push(json!({
                        "type": "function_call",
                        "id": id,
                        "call_id": call_id,
                        "name": wire_name,
                        "arguments": call.arguments.to_string(),
                    }));
                }
                ToolType::Custom => {
                    let id = if call_id.starts_with("ctc_") {
                        call_id.clone()
                    } else {
                        format!("ctc_{call_id}")
                    };
                    let input = call
                        .arguments
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| call.arguments.to_string());
                    out.push(json!({
                        "type": "custom_tool_call",
                        "id": id,
                        "call_id": call_id,
                        "name": wire_name,
                        "input": input,
                    }));
                }
            }
        }
        ItemKind::ToolResult(result) => out.push(convert_tool_result(result)),
        ItemKind::ReasoningText(_) => {}
        ItemKind::ProviderItem(item) if should_replay_provider_item(item.kind) => {
            out.push(item.payload);
        }
        ItemKind::ProviderItem(_) => {}
    }
}

fn should_replay_provider_item(kind: ProviderItemKind) -> bool {
    match kind {
        ProviderItemKind::Reasoning | ProviderItemKind::Compaction => true,
        ProviderItemKind::Unknown => false,
    }
}

fn convert_message(message: Message, out: &mut Vec<Value>) {
    let text = message.text_content();
    match message.role {
        Role::System | Role::Developer => {}
        Role::User => out.push(json!({
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": text,
            }],
        })),
        Role::Assistant => {
            let mut item = json!({
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                }],
            });
            item["phase"] = json!(message_phase_wire(
                message.phase.unwrap_or(MessagePhase::FinalAnswer)
            ));
            out.push(item);
        }
    }
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
        "call_id": result.call_id.0,
        "output": result.rendered_output(),
    })
}

fn convert_tool_spec(tool: ToolSpec) -> Value {
    let mut wire = match tool.tool_type {
        ToolType::Function => json!({
            "type": "function",
            "name": encode_tool_name(&tool.name),
            "strict": Value::Null,
            "description": tool.description,
            "parameters": tool.input_schema,
        }),
        ToolType::Custom => {
            let mut wire = json!({
                "type": "custom",
                "name": encode_tool_name(&tool.name),
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

fn provider_default_compaction_threshold() -> u64 {
    (super::DEFAULT_CONTEXT_WINDOW * 9 / 10).max(1000)
}

fn trim_before_latest_compaction(input_items: &[ItemKind]) -> &[ItemKind] {
    input_items
        .iter()
        .rposition(|item| {
            matches!(
                item,
                ItemKind::ProviderItem(provider_item)
                    if provider_item.kind == ProviderItemKind::Compaction
            )
        })
        .map_or(input_items, |index| &input_items[index..])
}

#[derive(Default)]
pub(crate) struct ResponseState {
    outputs: BTreeMap<usize, OutputAccumulator>,
    tool_names_by_wire: BTreeMap<String, String>,
    usage: Option<TokenUsage>,
    provider_response_id: Option<String>,
}

/// Everything streamed for a single `output_index`, reconciled in `finish`.
#[derive(Default)]
struct OutputAccumulator {
    reasoning_summary: String,
    /// A finalized item from `response.output_item.done`; when present it wins
    /// over the streamed `message_text` / `tool_call` fallbacks below.
    explicit: Option<ItemKind>,
    message_text: String,
    tool_call: ToolCallAccumulator,
}

#[derive(Clone)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    tool_type: ToolType,
    arguments_json: String,
}

impl Default for ToolCallAccumulator {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            tool_type: ToolType::Function,
            arguments_json: String::new(),
        }
    }
}

impl ResponseState {
    pub(crate) fn with_tool_names(tool_names_by_wire: BTreeMap<String, String>) -> Self {
        Self {
            tool_names_by_wire,
            ..Default::default()
        }
    }

    fn local_tool_name(&self, wire_name: &str) -> String {
        self.tool_names_by_wire
            .get(wire_name)
            .cloned()
            .unwrap_or_else(|| wire_name.to_owned())
    }

    fn output_mut(&mut self, output_index: usize) -> &mut OutputAccumulator {
        self.outputs.entry(output_index).or_default()
    }

    fn tool_call_at_mut(
        &mut self,
        output_index: usize,
        tool_type: ToolType,
    ) -> &mut ToolCallAccumulator {
        let call = &mut self.output_mut(output_index).tool_call;
        call.tool_type = tool_type;
        call
    }

    pub(crate) fn finish(self) -> InferenceResponse {
        let ResponseState {
            outputs,
            tool_names_by_wire: _,
            usage,
            provider_response_id,
        } = self;

        // `outputs` is keyed by `output_index`, so iterating it yields items in
        // provider order. Within one output, emit the reasoning summary first,
        // then the finalized item if we have one, otherwise the streamed
        // message and tool-call fallbacks.
        let mut items = Vec::new();
        for output in outputs.into_values() {
            let OutputAccumulator {
                reasoning_summary,
                explicit,
                message_text,
                tool_call,
            } = output;

            if !reasoning_summary.is_empty() {
                items.push(ItemKind::ReasoningText(ReasoningItem {
                    kind: ReasoningTextKind::Summary,
                    text: reasoning_summary,
                }));
            }

            if let Some(explicit) = explicit {
                items.push(explicit);
            } else {
                if !message_text.is_empty() {
                    items.push(ItemKind::Message(Message::text(
                        Role::Assistant,
                        message_text,
                    )));
                }
                if let Some(call) = tool_call.finish() {
                    items.push(ItemKind::ToolCall(call));
                }
            }
        }

        InferenceResponse {
            items,
            usage,
            provider_response_id,
        }
    }
}

impl ToolCallAccumulator {
    fn finish(self) -> Option<ToolCall> {
        if self.name.is_empty() {
            return None;
        }
        let arguments = match self.tool_type {
            ToolType::Function => serde_json::from_str(&self.arguments_json)
                .unwrap_or(Value::String(self.arguments_json)),
            ToolType::Custom => Value::String(self.arguments_json),
        };
        Some(ToolCall {
            id: ToolCallId(self.id),
            name: self.name,
            tool_type: self.tool_type,
            arguments,
        })
    }
}

impl ResponseState {
    pub(crate) fn apply_event(&mut self, event: &Value) -> Result<(bool, Vec<InferenceUpdate>)> {
        let mut updates = Vec::new();
        match event["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                    self.output_mut(output_index).message_text.push_str(delta);
                    updates.push(InferenceUpdate::TextDelta {
                        output_index,
                        text: delta.to_owned(),
                    });
                }
            }
            "response.output_text.done" => {
                if let Some(text) = event["text"].as_str() {
                    let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                    self.output_mut(output_index).message_text = text.to_owned();
                    updates.push(InferenceUpdate::OutputItem {
                        output_index,
                        item: ItemKind::Message(Message::text(Role::Assistant, text)),
                    });
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                    self.output_mut(output_index)
                        .reasoning_summary
                        .push_str(delta);
                    updates.push(InferenceUpdate::ReasoningTextDelta {
                        output_index,
                        kind: ReasoningTextKind::Summary,
                        text: delta.to_owned(),
                    });
                }
            }
            "response.reasoning_summary_part.added" => {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                let summary = &mut self.output_mut(output_index).reasoning_summary;
                if !summary.is_empty() {
                    summary.push_str("\n\n");
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                if let Some(delta) = event["delta"].as_str() {
                    self.tool_call_at_mut(output_index, ToolType::Function)
                        .arguments_json
                        .push_str(delta);
                }
            }
            "response.function_call_arguments.done" => {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                if let Some(arguments) = event["arguments"].as_str() {
                    self.tool_call_at_mut(output_index, ToolType::Function)
                        .arguments_json = arguments.to_owned();
                }
            }
            "response.custom_tool_call_input.delta" => {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                if let Some(delta) = event["delta"].as_str() {
                    self.tool_call_at_mut(output_index, ToolType::Custom)
                        .arguments_json
                        .push_str(delta);
                }
            }
            "response.custom_tool_call_input.done" => {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                if let Some(input) = event["input"].as_str() {
                    self.tool_call_at_mut(output_index, ToolType::Custom)
                        .arguments_json = input.to_owned();
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                if let Some(item) = event.get("item") {
                    let tool_type = match item["type"].as_str() {
                        Some("function_call") => Some(ToolType::Function),
                        Some("custom_tool_call") => Some(ToolType::Custom),
                        _ => None,
                    };
                    if let Some(tool_type) = tool_type {
                        let local_name =
                            item["name"].as_str().map(|name| self.local_tool_name(name));
                        let call = self.tool_call_at_mut(output_index, tool_type);
                        if let Some(id) = item["call_id"].as_str() {
                            call.id = id.to_owned();
                        }
                        if let Some(name) = local_name {
                            call.name = name;
                        }
                        if call.arguments_json.is_empty() {
                            let final_input = match tool_type {
                                ToolType::Function => item["arguments"].as_str(),
                                ToolType::Custom => item["input"].as_str(),
                            };
                            if let Some(final_input) = final_input {
                                call.arguments_json = final_input.to_owned();
                            }
                        }
                        if let Some(call) = call.clone().finish() {
                            updates.push(InferenceUpdate::ToolCall { output_index, call });
                        }
                    }

                    if event["type"].as_str() == Some("response.output_item.done")
                        && item["type"].as_str() == Some("message")
                        && let Some(text) = message_text_from_output_item(item)
                    {
                        let mut message = Message::text(Role::Assistant, text.clone());
                        message.phase = message_phase_from_output_item(item);
                        let output = self.output_mut(output_index);
                        output.message_text = text;
                        output.explicit = Some(ItemKind::Message(message.clone()));
                        updates.push(InferenceUpdate::OutputItem {
                            output_index,
                            item: ItemKind::Message(message),
                        });
                    }
                    if event["type"].as_str() == Some("response.output_item.done")
                        && item["type"].as_str() == Some("reasoning")
                        && item["encrypted_content"].is_string()
                    {
                        let provider_item = ProviderItem {
                            kind: ProviderItemKind::Reasoning,
                            payload: item.clone(),
                        };
                        self.output_mut(output_index).explicit =
                            Some(ItemKind::ProviderItem(provider_item.clone()));
                        updates.push(InferenceUpdate::OutputItem {
                            output_index,
                            item: ItemKind::ProviderItem(provider_item),
                        });
                    }
                    if item["type"].as_str() == Some("compaction") {
                        if event["type"].as_str() == Some("response.output_item.added") {
                            updates.push(InferenceUpdate::CompactionStarted { output_index });
                        } else if event["type"].as_str() == Some("response.output_item.done") {
                            let provider_item = ProviderItem {
                                kind: ProviderItemKind::Compaction,
                                payload: item.clone(),
                            };
                            self.output_mut(output_index).explicit =
                                Some(ItemKind::ProviderItem(provider_item.clone()));
                            updates.push(InferenceUpdate::OutputItem {
                                output_index,
                                item: ItemKind::ProviderItem(provider_item),
                            });
                        }
                    }
                    if event["type"].as_str() == Some("response.output_item.done")
                        && should_preserve_unknown_provider_item(item)
                    {
                        let provider_item = ProviderItem {
                            kind: ProviderItemKind::Unknown,
                            payload: item.clone(),
                        };
                        self.output_mut(output_index).explicit =
                            Some(ItemKind::ProviderItem(provider_item.clone()));
                        updates.push(InferenceUpdate::OutputItem {
                            output_index,
                            item: ItemKind::ProviderItem(provider_item),
                        });
                    }
                }
            }
            "response.completed" | "response.done" => {
                self.usage = usage_from_event(event);
                if let Some(usage) = self.usage.clone() {
                    updates.push(InferenceUpdate::Usage(usage));
                }
                self.provider_response_id = event
                    .get("response")
                    .and_then(|response| response["id"].as_str())
                    .or_else(|| event["id"].as_str())
                    .map(str::to_owned);
                if let Some(response_id) = self.provider_response_id.clone() {
                    updates.push(InferenceUpdate::ResponseId(response_id));
                }
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
fn should_preserve_unknown_provider_item(item: &Value) -> bool {
    !matches!(
        item["type"].as_str(),
        Some("message" | "function_call" | "custom_tool_call" | "reasoning" | "compaction")
    )
}

fn message_text_from_output_item(item: &Value) -> Option<String> {
    let mut text = String::new();
    for part in item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let is_text_part = matches!(
            part.get("type").and_then(Value::as_str),
            Some("output_text") | Some("text")
        );
        if is_text_part && let Some(part_text) = part.get("text").and_then(Value::as_str) {
            text.push_str(part_text);
        }
    }

    if text.is_empty() { None } else { Some(text) }
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
