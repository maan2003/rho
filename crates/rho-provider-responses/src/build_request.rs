use std::collections::BTreeMap;

use rho::{
    ItemBlock, ItemKind, Message, MessagePhase, ProviderItemKind, ProviderRequest, Role,
    ToolFormat, ToolGrammarSyntax, ToolResult, ToolSpec, ToolType,
};
use serde_json::{Value, json};

use crate::{
    ContextManagementRequest, ProviderSession, ReasoningContext, ReasoningEffort, ReasoningRequest,
    ReasoningSummary, ResponsesConfig, ResponsesRequest, ServiceTier, TextRequest, ToolChoice,
    Verbosity, encode_tool_name,
};

impl ResponsesRequest {
    pub fn from_provider_request(
        config: &ResponsesConfig,
        session: &ProviderSession,
        request: ProviderRequest,
    ) -> Self {
        let mut previous_response = None;
        for (index, block) in request.input.iter().enumerate() {
            let ItemBlock::ProviderResponse {
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

        Self::from_provider_request_with_previous(config, session, request, previous_response)
    }

    pub(crate) fn from_provider_request_full_replay(
        config: &ResponsesConfig,
        session: &ProviderSession,
        request: ProviderRequest,
    ) -> Self {
        Self::from_provider_request_with_previous(config, session, request, None)
    }

    fn from_provider_request_with_previous(
        config: &ResponsesConfig,
        session: &ProviderSession,
        request: ProviderRequest,
        previous_response: Option<(String, usize)>,
    ) -> Self {
        let instructions = request
            .input
            .iter()
            .flat_map(|block| match block {
                ItemBlock::Local { items } | ItemBlock::ProviderResponse { items, .. } => items,
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
                ItemBlock::Local { items } | ItemBlock::ProviderResponse { items, .. } => items,
            })
            .map(|item| item.kind.clone())
            .collect::<Vec<_>>();
        let input_items = if config.supports_compaction {
            trim_before_latest_compaction(&input_items)
        } else {
            input_items.as_slice()
        };
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
            convert_item_kind(config, &local_tool_wire_names, item, &mut input);
        }
        let tool_choice = match (session.tool_choice, tools.is_empty()) {
            (ToolChoice::None, _) => Some("none"),
            (ToolChoice::Auto, false) => Some("auto"),
            (ToolChoice::Auto, true) => None,
        };
        let temperature = session.temperature;
        let max_output_tokens = session.max_output_tokens;
        let effort = config
            .supports_reasoning_effort
            .then_some(session.reasoning_effort)
            .flatten();
        let summary = config
            .supports_reasoning_summary
            .then_some(reasoning_summary_wire(session.reasoning_summary))
            .flatten();
        let verbosity = session.verbosity;
        let service_tier = session.service_tier;
        let prompt_cache_key = session.prompt_cache_key.clone();
        let previous_response_id = previous_response.map(|(id, _)| id);
        let active_compaction = config
            .supports_compaction
            .then_some(config.compaction.as_ref())
            .flatten();
        if active_compaction.is_some() {
            input.insert(0, json!({"type": "compaction_trigger"}));
        }
        let context_management = active_compaction
            .map(|compaction| {
                vec![ContextManagementRequest {
                    ty: "compaction",
                    compact_threshold: Some(compaction.compact_threshold.unwrap_or_else(|| {
                        provider_default_compaction_threshold(config.context_window)
                    })),
                }]
            })
            .unwrap_or_default();

        Self {
            model: session.model.clone(),
            instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
            input,
            temperature,
            max_output_tokens,
            store: Some(config.surface.store_value()),
            tools,
            tool_choice,
            reasoning: (effort.is_some() || summary.is_some()).then(|| ReasoningRequest {
                context: Some(ReasoningContext::AllTurns),
                effort: effort.map(effort_wire),
                summary,
            }),
            text: config.supports_verbosity.then_some(TextRequest {
                verbosity: verbosity.map(verbosity_wire).unwrap_or("medium"),
            }),
            include: if config.supports_encrypted_reasoning {
                vec!["reasoning.encrypted_content"]
            } else {
                Vec::new()
            },
            prompt_cache_key: config
                .supports_prompt_cache_key
                .then_some(prompt_cache_key)
                .flatten(),
            service_tier: service_tier.map(service_tier_wire),
            context_management,
            previous_response_id,
            extra_body: config.extra_body.clone(),
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
    config: &ResponsesConfig,
    local_tool_wire_names: &BTreeMap<String, String>,
    item: ItemKind,
    out: &mut Vec<Value>,
) {
    match item {
        ItemKind::Message(message) => convert_message(config, message, out),
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
        ItemKind::ProviderItem(item) if should_replay_provider_item(config, item.kind) => {
            out.push(item.payload);
        }
        ItemKind::ProviderItem(_) => {}
    }
}

fn should_replay_provider_item(config: &ResponsesConfig, kind: ProviderItemKind) -> bool {
    match kind {
        ProviderItemKind::Reasoning => config.supports_encrypted_reasoning,
        ProviderItemKind::Compaction => config.supports_compaction,
        ProviderItemKind::Unknown => false,
    }
}

fn convert_message(config: &ResponsesConfig, message: Message, out: &mut Vec<Value>) {
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
            if config.supports_phase {
                item["phase"] = json!(message_phase_wire(
                    message.phase.unwrap_or(MessagePhase::FinalAnswer)
                ));
            }
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

fn effort_wire(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    }
}

fn reasoning_summary_wire(reasoning_summary: ReasoningSummary) -> Option<&'static str> {
    match reasoning_summary {
        ReasoningSummary::Off => None,
        ReasoningSummary::Auto => Some("auto"),
        ReasoningSummary::Concise => Some("concise"),
        ReasoningSummary::Detailed => Some("detailed"),
    }
}

fn verbosity_wire(verbosity: Verbosity) -> &'static str {
    match verbosity {
        Verbosity::Low => "low",
        Verbosity::Medium => "medium",
        Verbosity::High => "high",
    }
}

fn service_tier_wire(service_tier: ServiceTier) -> &'static str {
    match service_tier {
        ServiceTier::Auto => "auto",
        ServiceTier::Default => "default",
        ServiceTier::Flex => "flex",
    }
}

fn provider_default_compaction_threshold(context_window: u64) -> u64 {
    (context_window * 9 / 10).max(1000)
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
