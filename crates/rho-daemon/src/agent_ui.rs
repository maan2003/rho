//! Projection from runtime agent state into the smaller UI wire shape.

use std::sync::Arc;

use rho_agent::{AgentState, AgentStateKind, QueuedItem, QueuedItemKind};
use rho_core::{
    ApplyPatchMetadata, ContextBlock, InferenceResponseItem, MessageSender, StreamingContextItem,
    StreamingContextItemState, ToolFileStatus, ToolResultMetadata, text_content,
};
use rho_ui_proto::remote::{
    UiAgentState, UiAgentStatus, UiAgentUsage, UiApplyPatchMetadata, UiBlock, UiTool,
    UiToolFileChange, UiToolFileStatus, UiToolMetadata, UiToolStatus,
};

pub(crate) fn project_agent_state(state: &AgentState) -> UiAgentState {
    let mut blocks = ui_blocks(&state.blocks);
    merge_active_tool_state(&mut blocks, &state.kind);
    blocks.extend(in_flight_blocks(&state.kind));
    blocks.extend(state.queued_inputs.iter().map(|input| match input {
        QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender, content, ..
            },
            delivery,
        } => UiBlock::QueuedMessage {
            text: text_content(content),
            delivery: *delivery,
            sender: match sender {
                MessageSender::User => None,
                MessageSender::Agent { id } => Some(*id),
            },
        },
        QueuedItem {
            kind: QueuedItemKind::Compaction,
            ..
        } => UiBlock::Notice {
            text: "compacting context".to_owned(),
        },
        QueuedItem {
            kind: QueuedItemKind::ToolUpdate(update),
            ..
        } => UiBlock::Notice {
            text: format!("tool update: {}", update.output),
        },
    }));
    UiAgentState {
        blocks,
        status: ui_status(&state.kind),
        context_used: state.context_used,
        usage: UiAgentUsage {
            provider: state.usage_provider.name().to_owned(),
            total: rho_ui_proto::AgentUsageBucket {
                bucket_start_ms: state.total_usage.bucket_start_ms,
                input_tokens: state.total_usage.input_tokens,
                cache_read_tokens: state.total_usage.cache_read_tokens,
                cache_write_tokens: state.total_usage.cache_write_tokens,
                cache_write_1h_tokens: state.total_usage.cache_write_1h_tokens,
                output_tokens: state.total_usage.output_tokens,
                requests: state.total_usage.requests,
                approximate: state.total_usage.approximate,
            },
        },
    }
}

fn ui_tool_metadata(metadata: &ToolResultMetadata) -> UiToolMetadata {
    match metadata {
        ToolResultMetadata::ApplyPatch(metadata) => {
            UiToolMetadata::ApplyPatch(ui_apply_patch_metadata(metadata))
        }
    }
}

fn ui_apply_patch_metadata(metadata: &ApplyPatchMetadata) -> UiApplyPatchMetadata {
    UiApplyPatchMetadata {
        changes: metadata
            .changes
            .iter()
            .map(|change| UiToolFileChange {
                path: change.path.clone(),
                status: ui_tool_file_status(change.status),
            })
            .collect(),
    }
}

fn ui_tool_file_status(status: ToolFileStatus) -> UiToolFileStatus {
    match status {
        ToolFileStatus::Added => UiToolFileStatus::Added,
        ToolFileStatus::Modified => UiToolFileStatus::Modified,
        ToolFileStatus::Deleted => UiToolFileStatus::Deleted,
        ToolFileStatus::Moved => UiToolFileStatus::Moved,
    }
}

fn ui_blocks(blocks: &[Arc<ContextBlock>]) -> Vec<UiBlock> {
    let mut ui_blocks = Vec::new();
    for block in blocks {
        match block.as_ref() {
            ContextBlock::UserMessage { sender, content } => match sender {
                MessageSender::User => ui_blocks.push(UiBlock::UserMessage {
                    text: text_content(content),
                }),
                MessageSender::Agent { id } => ui_blocks.push(UiBlock::AgentMessage {
                    sender: *id,
                    text: text_content(content),
                }),
            },
            ContextBlock::CompactionTrigger => ui_blocks.push(UiBlock::Notice {
                text: "compacting context".to_owned(),
            }),
            ContextBlock::InferenceResponse { items, .. } => {
                ui_blocks.extend(items.iter().filter_map(ui_block_from_response_item));
            }
            ContextBlock::ToolUpdate(update) => ui_blocks.push(UiBlock::Notice {
                text: format!("tool update: {}", update.output),
            }),
            ContextBlock::ToolResults { results } => {
                for result in results {
                    if let Some(UiBlock::Tool(tool)) = ui_blocks.iter_mut().rev().find(|block| {
                        matches!(block, UiBlock::Tool(tool) if tool.id == result.call_id.as_str())
                    }) {
                        tool.status = result.body.status.into();
                        tool.started_at = Some(result.started_at);
                        tool.finished_at = Some(result.finished_at);
                        tool.metadata = result.metadata.as_ref().map(ui_tool_metadata);
                    }
                }
            }
        }
    }
    ui_blocks
}

fn merge_active_tool_state(blocks: &mut [UiBlock], kind: &AgentStateKind) {
    let AgentStateKind::ToolCalling {
        previews, results, ..
    } = kind
    else {
        return;
    };

    for preview in previews.values() {
        if let Some(tool) = find_tool_block_mut(blocks, preview.call.id.as_str()) {
            tool.name = preview.call.name.as_str().to_owned();
            tool.arguments = preview.call.arguments.clone();
            tool.status = UiToolStatus::Running;
            tool.started_at = Some(preview.started_at);
            tool.finished_at = None;
        }
    }

    for result in results {
        if let Some(tool) = find_tool_block_mut(blocks, result.call_id.as_str()) {
            tool.status = result.body.status.into();
            tool.started_at = Some(result.started_at);
            tool.finished_at = Some(result.finished_at);
            tool.metadata = result.metadata.as_ref().map(ui_tool_metadata);
        }
    }
}

fn find_tool_block_mut<'a>(blocks: &'a mut [UiBlock], id: &str) -> Option<&'a mut UiTool> {
    blocks.iter_mut().rev().find_map(|block| match block {
        UiBlock::Tool(tool) if tool.id == id => Some(tool),
        _ => None,
    })
}

fn ui_block_from_response_item(item: &InferenceResponseItem) -> Option<UiBlock> {
    match item {
        InferenceResponseItem::AssistantMessage { content, phase, .. } => {
            Some(UiBlock::AssistantMessage {
                text: text_content(content),
                phase: phase.map(Into::into),
            })
        }
        InferenceResponseItem::RawReasoning {
            content, summary, ..
        } => Some(UiBlock::Reasoning {
            text: reasoning_text(content, summary),
        }),
        InferenceResponseItem::ToolCall {
            id,
            name,
            arguments,
            ..
        } => Some(UiBlock::Tool(UiTool {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.clone(),
            preview: None,
            status: UiToolStatus::Running,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            metadata: None,
        })),
        InferenceResponseItem::Compaction { .. } => Some(UiBlock::Notice {
            text: "compacting context".to_owned(),
        }),
        InferenceResponseItem::EncryptedReasoning { summary, .. } => {
            (!summary.is_empty()).then(|| UiBlock::Reasoning {
                text: summary.join("\n"),
            })
        }
        InferenceResponseItem::Unknown { .. } => None,
    }
}

fn ui_status(kind: &AgentStateKind) -> UiAgentStatus {
    match kind {
        AgentStateKind::ApiStreaming { .. } => UiAgentStatus::Streaming,
        AgentStateKind::ToolCalling { waiting, .. } => UiAgentStatus::ToolCalling {
            waiting: waiting.as_ref().map(|wait| wait.until),
        },
        AgentStateKind::UnfinishedTurn {
            outstanding_calls, ..
        } => UiAgentStatus::UnfinishedTurn {
            outstanding_calls: outstanding_calls.len(),
        },
        AgentStateKind::Error(_) => UiAgentStatus::Error,
        AgentStateKind::Idle => UiAgentStatus::Idle,
    }
}

/// The in-flight tail of the block list: the response being streamed, or the
/// partial response plus an error notice after a permanent failure.
fn in_flight_blocks(kind: &AgentStateKind) -> Vec<UiBlock> {
    match kind {
        AgentStateKind::ApiStreaming {
            pending_response,
            previous_attempt,
        } => {
            let mut blocks = Vec::new();
            if let Some(failure) = previous_attempt {
                blocks.push(UiBlock::Notice {
                    text: format!(
                        "temporary inference error (attempt {}): {}; retrying",
                        failure.attempt_count, failure.error
                    ),
                });
            }
            blocks.extend(pending_response.items.iter().filter_map(streaming_block));
            blocks
        }
        AgentStateKind::Error(failure) => {
            let mut blocks = failure
                .partial_response
                .items
                .iter()
                .filter_map(streaming_block)
                .collect::<Vec<_>>();
            blocks.push(UiBlock::Notice {
                text: format!("agent error: {}", failure.error),
            });
            blocks
        }
        AgentStateKind::Idle
        | AgentStateKind::ToolCalling { .. }
        | AgentStateKind::UnfinishedTurn { .. } => Vec::new(),
    }
}

fn streaming_block(item: &StreamingContextItemState) -> Option<UiBlock> {
    match item {
        StreamingContextItemState::Pending(item) | StreamingContextItemState::Finished(item) => {
            block_from_streaming_item(item)
        }
        StreamingContextItemState::Empty => None,
    }
}

fn block_from_streaming_item(item: &StreamingContextItem) -> Option<UiBlock> {
    match item {
        StreamingContextItem::AssistantMessage { content, phase, .. } => {
            Some(UiBlock::AssistantMessage {
                text: content.iter().map(ToString::to_string).collect(),
                phase: phase.map(Into::into),
            })
        }
        StreamingContextItem::RawReasoning {
            content, summary, ..
        } => Some(UiBlock::Reasoning {
            text: reasoning_text(content, summary),
        }),
        StreamingContextItem::EncryptedReasoning { summary, .. } => {
            (!summary.is_empty()).then(|| UiBlock::Reasoning {
                text: summary
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            })
        }
        StreamingContextItem::ToolCall {
            id,
            name,
            arguments,
            ..
        } => Some(UiBlock::Tool(UiTool {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.to_string(),
            preview: None,
            status: UiToolStatus::Running,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            metadata: None,
        })),
        StreamingContextItem::Compaction { .. } => Some(UiBlock::Notice {
            text: "compacting context".to_owned(),
        }),
        StreamingContextItem::Unknown { .. } => None,
    }
}

fn reasoning_text(content: &impl ToString, summary: &[impl ToString]) -> String {
    if summary.is_empty() {
        content.to_string()
    } else {
        summary
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU64;

    use rho_agent::{FailedInferenceResponse, ToolPreview, ToolPreviewMetadata};
    // `register_senax_tagged!` names the trait and its registry entry type
    // unqualified, so both must be imported from the declaring crate.
    use rho_core::{
        __SenaxProviderSpecificDataEntry, AStr, ApplyPatchMetadata, ContentPart,
        PendingInferenceResponse, ProviderSpecificData, ToolCall, ToolCallId, ToolFileChange,
        ToolFileStatus, ToolName, ToolOutput, ToolOutputStatus, ToolResult, ToolType, UnixMs,
    };
    use rho_ui_proto::remote::{
        AgentRemoteEncoder, AgentRemoteFrame, UiAgentStatus, UiBlock, UiBlockDiff, UiBlockUpdate,
        UiBlocksDiff, UiTextDiff, UiTool, UiToolDiff, UiToolStatus,
    };
    use senax_encoder::{Decode, Encode};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    struct UiTestProviderSpecificData {
        item_id: String,
    }

    senax_encoder::register_senax_tagged!(
        trait = ProviderSpecificData,
        type = UiTestProviderSpecificData,
        tag = "rho-ui-proto-test.provider-data",
    );

    fn test_provider_specific_data() -> Box<dyn ProviderSpecificData> {
        Box::new(UiTestProviderSpecificData {
            item_id: "ui_item_1".to_owned(),
        })
    }

    #[test]
    fn cumulative_usage_is_streamed_as_agent_state() {
        let mut runtime = streaming_state("hello");
        runtime.usage_provider = rho_agent::db::AgentUsageModel::FABLE;
        let mut encoder = AgentRemoteEncoder::new();
        let mut receiver = project_agent_state(&runtime);
        let _ = encoder.encode(receiver.clone());

        runtime.total_usage.output_tokens = 42;
        let frame = encoder.encode(project_agent_state(&runtime));
        let AgentRemoteFrame::Diff { usage, .. } = &frame else {
            panic!("usage update should be a state diff");
        };
        assert_eq!(usage.as_ref().unwrap().provider, "fable");
        assert_eq!(usage.as_ref().unwrap().total.output_tokens, 42);

        frame.apply_diff(&mut receiver);
        assert_eq!(receiver.usage.total.output_tokens, 42);
    }

    fn streaming_state(text: &str) -> AgentState {
        AgentState {
            blocks: Vec::new(),
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::ApiStreaming {
                pending_response: PendingInferenceResponse {
                    items: vec![StreamingContextItemState::Pending(
                        StreamingContextItem::AssistantMessage {
                            provider_specific: test_provider_specific_data(),
                            content: vec![AStr::from(text)],
                            phase: None,
                        },
                    )],
                },
                previous_attempt: None,
            },
            context_used: None,
            total_usage: rho_agent::db::AgentUsageBucket::default(),
            usage_provider: rho_agent::db::AgentUsageModel::GPT,
        }
    }

    fn retry_streaming_state(text: &str) -> AgentState {
        let mut state = streaming_state(text);
        let AgentStateKind::ApiStreaming {
            previous_attempt, ..
        } = &mut state.kind
        else {
            unreachable!()
        };
        *previous_attempt = Some(FailedInferenceResponse {
            partial_response: PendingInferenceResponse::default(),
            attempt_count: NonZeroU64::MIN,
            error: Arc::new("temporary failure".to_owned()),
        });
        state
    }

    fn error_state(partial_text: &str, message: &str) -> AgentState {
        AgentState {
            blocks: Vec::new(),
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::Error(FailedInferenceResponse {
                partial_response: PendingInferenceResponse {
                    items: vec![StreamingContextItemState::Pending(
                        StreamingContextItem::AssistantMessage {
                            provider_specific: test_provider_specific_data(),
                            content: vec![AStr::from(partial_text)],
                            phase: None,
                        },
                    )],
                },
                attempt_count: NonZeroU64::MIN,
                error: Arc::new(message.to_owned()),
            }),
            context_used: None,
            total_usage: rho_agent::db::AgentUsageBucket::default(),
            usage_provider: rho_agent::db::AgentUsageModel::GPT,
        }
    }

    fn finished_state(text: &str) -> AgentState {
        AgentState {
            blocks: vec![Arc::new(ContextBlock::InferenceResponse {
                items: vec![InferenceResponseItem::AssistantMessage {
                    provider_specific: test_provider_specific_data(),
                    content: vec![ContentPart::Text {
                        text: text.to_owned(),
                    }],
                    phase: None,
                }],
                provider_response_id: None,
            })],
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
            total_usage: rho_agent::db::AgentUsageBucket::default(),
            usage_provider: rho_agent::db::AgentUsageModel::GPT,
        }
    }

    fn finished_tool_state() -> AgentState {
        let call_id = ToolCallId::try_from("call-1").unwrap();
        AgentState {
            blocks: vec![
                Arc::new(ContextBlock::InferenceResponse {
                    items: vec![InferenceResponseItem::ToolCall {
                        provider_specific: test_provider_specific_data(),
                        id: call_id.clone(),
                        name: ToolName::try_from("shell_command").unwrap(),
                        tool_type: ToolType::Function,
                        arguments: r#"{"command":"printf hi"}"#.to_owned(),
                    }],
                    provider_response_id: None,
                }),
                Arc::new(ContextBlock::ToolResults {
                    results: vec![ToolResult {
                        call_id,
                        tool_type: ToolType::Function,
                        body: ToolOutput {
                            images: std::sync::Arc::new(Vec::new()),
                            output: Arc::new("hi".to_owned()),
                            status: ToolOutputStatus::Success,
                        },
                        started_at: UnixMs(1),
                        finished_at: UnixMs(3),
                        metadata: None,
                    }],
                }),
            ],
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
            total_usage: rho_agent::db::AgentUsageBucket::default(),
            usage_provider: rho_agent::db::AgentUsageModel::GPT,
        }
    }

    fn tool_calling_state() -> AgentState {
        let call_id = ToolCallId::try_from("call-1").unwrap();
        let mut previews = BTreeMap::new();
        previews.insert(
            call_id.clone(),
            ToolPreview {
                call: ToolCall {
                    id: call_id,
                    name: ToolName::try_from("apply_patch").unwrap(),
                    tool_type: ToolType::Function,
                    arguments: "*** Begin Patch\n*** End Patch\n".to_owned(),
                },
                started_at: UnixMs(10),
                metadata: Some(ToolPreviewMetadata::ApplyPatch(ApplyPatchMetadata {
                    changes: vec![ToolFileChange {
                        path: "src/lib.rs".to_owned(),
                        status: ToolFileStatus::Modified,
                    }],
                })),
            },
        );
        AgentState {
            blocks: Vec::new(),
            queued_inputs: rho_agent::InputQueues::default(),
            kind: AgentStateKind::ToolCalling {
                previews,
                results: Vec::new(),
                waiting: None,
            },
            context_used: None,
            total_usage: rho_agent::db::AgentUsageBucket::default(),
            usage_provider: rho_agent::db::AgentUsageModel::GPT,
        }
    }

    fn tool_calling_state_with_call_block() -> AgentState {
        let mut state = tool_calling_state();
        let AgentStateKind::ToolCalling { previews, .. } = &state.kind else {
            unreachable!()
        };
        let preview = previews.values().next().unwrap();
        state.blocks.push(Arc::new(ContextBlock::InferenceResponse {
            items: vec![InferenceResponseItem::ToolCall {
                provider_specific: test_provider_specific_data(),
                id: preview.call.id.clone(),
                name: preview.call.name.clone(),
                tool_type: preview.call.tool_type,
                arguments: String::new(),
            }],
            provider_response_id: None,
        }));
        state
    }

    #[test]
    fn streaming_update_sends_text_suffix_diff() {
        let mut encoder = AgentRemoteEncoder::new();
        let AgentRemoteFrame::Snapshot(mut receiver) =
            encoder.encode(project_agent_state(&streaming_state("hel")))
        else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(project_agent_state(&streaming_state("hello")));
        let AgentRemoteFrame::Diff { blocks, status, .. } = &frame else {
            panic!("second frame should be a diff");
        };
        assert_eq!(*status, None);
        assert_eq!(
            blocks.updates,
            [UiBlockUpdate {
                index: 0,
                block: UiBlockDiff::AssistantText(UiTextDiff {
                    keep_bytes: 3,
                    value: "lo".to_owned(),
                }),
            }]
        );

        frame.apply_diff(&mut receiver);
        assert_eq!(receiver, project_agent_state(&streaming_state("hello")));
    }

    #[test]
    fn agent_message_sender_preserves_agent_id() {
        let sender = rho_core::AgentId::from_counter(2, &rho_core::AgentIdDomain(0)).unwrap();
        let mut state = finished_state("");
        state.blocks = vec![Arc::new(ContextBlock::UserMessage {
            sender: rho_core::MessageSender::Agent { id: sender },
            content: vec![ContentPart::Text {
                text: "hello".to_owned(),
            }],
        })];

        let state = project_agent_state(&state);

        assert_eq!(
            state.blocks,
            [UiBlock::AgentMessage {
                sender,
                text: "hello".to_owned(),
            }]
        );
    }

    #[test]
    fn tiny_streaming_update_has_small_wire_frame() {
        let mut encoder = AgentRemoteEncoder::new();
        let _ = encoder.encode(project_agent_state(&streaming_state("hel")));
        let frame = encoder.encode(project_agent_state(&streaming_state("hello")));
        let bytes = rho_ui_proto::protocol_frame_bytes(&rho_ui_proto::ServerMessage::Agent {
            agent_id: rho_core::AgentId::from_counter(1, &rho_core::AgentIdDomain(0)).unwrap(),
            frame,
        })
        .unwrap();
        assert!(bytes.len() < 56, "tiny frame was {} bytes", bytes.len());
    }

    #[test]
    fn finishing_a_streamed_response_sends_only_a_status_change() {
        let mut encoder = AgentRemoteEncoder::new();
        let AgentRemoteFrame::Snapshot(mut receiver) =
            encoder.encode(project_agent_state(&streaming_state("hello")))
        else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(project_agent_state(&finished_state("hello")));
        let AgentRemoteFrame::Diff { blocks, status, .. } = &frame else {
            panic!("second frame should be a diff");
        };
        assert_eq!(
            blocks,
            &UiBlocksDiff {
                truncate_to: None,
                updates: Vec::new(),
            }
        );
        assert_eq!(*status, Some(UiAgentStatus::Idle));

        let bytes = rho_ui_proto::protocol_frame_bytes(&rho_ui_proto::ServerMessage::Agent {
            agent_id: rho_core::AgentId::from_counter(1, &rho_core::AgentIdDomain(0)).unwrap(),
            frame: frame.clone(),
        })
        .unwrap();
        assert!(
            bytes.len() < 36,
            "finish frame resent too much data: {} bytes",
            bytes.len()
        );
        frame.apply_diff(&mut receiver);
        assert_eq!(receiver, project_agent_state(&finished_state("hello")));
    }

    #[test]
    fn snapshots_do_not_render_tool_result_placeholders() {
        let state = project_agent_state(&finished_tool_state());
        assert_eq!(state.blocks.len(), 1);
        assert!(matches!(
            &state.blocks[0],
            UiBlock::Tool(UiTool {
                name,
                arguments,
                status: UiToolStatus::Success,
                output: None,
                error: None,
                started_at: Some(UnixMs(1)),
                finished_at: Some(UnixMs(3)),
                metadata: None,
                ..
            }) if name == "shell_command" && arguments.contains("printf hi")
        ));
    }

    #[test]
    fn tool_calling_maps_to_plain_status() {
        let state = project_agent_state(&tool_calling_state());
        assert_eq!(state.status, UiAgentStatus::ToolCalling { waiting: None });
    }

    #[test]
    fn tool_calling_preview_updates_existing_tool_block() {
        let state = project_agent_state(&tool_calling_state_with_call_block());
        assert!(matches!(
            state.blocks.as_slice(),
            [UiBlock::Tool(UiTool {
                name,
                arguments,
                status: UiToolStatus::Running,
                started_at: Some(UnixMs(10)),
                finished_at: None,
                ..
            })] if name == "apply_patch"
                && arguments == "*** Begin Patch\n*** End Patch\n"
        ));
    }

    #[test]
    fn tool_calling_preview_timing_diffs_existing_tool_block() {
        let mut encoder = AgentRemoteEncoder::new();
        let mut initial = tool_calling_state_with_call_block();
        let AgentStateKind::ToolCalling { previews, .. } = &mut initial.kind else {
            unreachable!()
        };
        previews.clear();
        let AgentRemoteFrame::Snapshot(mut receiver) =
            encoder.encode(project_agent_state(&initial))
        else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(project_agent_state(&tool_calling_state_with_call_block()));
        let AgentRemoteFrame::Diff { blocks, .. } = &frame else {
            panic!("second frame should be a diff");
        };
        assert!(matches!(
            blocks.updates.as_slice(),
            [UiBlockUpdate {
                index: 0,
                block: UiBlockDiff::Tool(UiToolDiff {
                    arguments: Some(UiTextDiff {
                        keep_bytes: 0,
                        value,
                    }),
                    started_at: Some(Some(UnixMs(10))),
                    finished_at: None,
                    ..
                })
            }] if value == "*** Begin Patch\n*** End Patch\n"
        ));

        frame.apply_diff(&mut receiver);
        assert_eq!(
            receiver,
            project_agent_state(&tool_calling_state_with_call_block())
        );
    }

    #[test]
    fn tool_result_updates_existing_tool_block() {
        let mut encoder = AgentRemoteEncoder::new();
        let mut running = finished_tool_state();
        running.blocks.pop();
        let AgentRemoteFrame::Snapshot(mut receiver) =
            encoder.encode(project_agent_state(&running))
        else {
            panic!("first frame should be a snapshot");
        };

        let frame = encoder.encode(project_agent_state(&finished_tool_state()));
        let AgentRemoteFrame::Diff { blocks, .. } = &frame else {
            panic!("second frame should be a diff");
        };
        assert_eq!(blocks.truncate_to, None);
        assert!(matches!(
            blocks.updates.as_slice(),
            [UiBlockUpdate {
                index: 0,
                block: UiBlockDiff::Tool(UiToolDiff {
                    status: Some(UiToolStatus::Success),
                    output: None,
                    error: None,
                    started_at: Some(Some(UnixMs(1))),
                    finished_at: Some(Some(UnixMs(3))),
                    metadata: None,
                    ..
                })
            }]
        ));

        frame.apply_diff(&mut receiver);
        assert_eq!(receiver, project_agent_state(&finished_tool_state()));
    }

    #[test]
    fn retry_streaming_updates_still_use_text_diffs() {
        let mut encoder = AgentRemoteEncoder::new();
        let _ = encoder.encode(project_agent_state(&retry_streaming_state("hel")));
        let frame = encoder.encode(project_agent_state(&retry_streaming_state("hello")));
        let AgentRemoteFrame::Diff { blocks, .. } = frame else {
            panic!("second frame should be a diff");
        };
        assert_eq!(
            blocks.updates,
            [UiBlockUpdate {
                index: 1,
                block: UiBlockDiff::AssistantText(UiTextDiff {
                    keep_bytes: 3,
                    value: "lo".to_owned(),
                }),
            }]
        );
    }

    #[test]
    fn retry_streaming_displays_temporary_failure() {
        let state = project_agent_state(&retry_streaming_state("retry response"));
        assert_eq!(state.status, UiAgentStatus::Streaming);
        assert_eq!(
            state.blocks,
            [
                UiBlock::Notice {
                    text: "temporary inference error (attempt 1): temporary failure; retrying"
                        .to_owned(),
                },
                UiBlock::AssistantMessage {
                    text: "retry response".to_owned(),
                    phase: None,
                },
            ]
        );
    }

    #[test]
    fn error_state_keeps_partial_response_and_appends_notice() {
        let state = project_agent_state(&error_state("partial answer", "quota"));
        assert_eq!(state.status, UiAgentStatus::Error);
        assert_eq!(
            state.blocks,
            [
                UiBlock::AssistantMessage {
                    text: "partial answer".to_owned(),
                    phase: None,
                },
                UiBlock::Notice {
                    text: "agent error: quota".to_owned(),
                },
            ]
        );
    }
}
