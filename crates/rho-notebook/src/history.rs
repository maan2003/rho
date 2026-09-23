//! The conversation as notebook Python sees it: `transcript`, a lazy
//! sequence over a snapshot taken when the cell was admitted.
use std::sync::Arc;

use pyo3::IntoPyObjectExt;
use pyo3::exceptions::PyIndexError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};
use rho_core::{
    ContentPart, ContextBlock, InferenceResponseItem, MessageSender, ToolOutputStatus, ToolType,
};

#[derive(Default)]
pub(crate) struct HistorySnapshot {
    blocks: Arc<[Arc<ContextBlock>]>,
    locations: Vec<(usize, usize)>,
}

impl HistorySnapshot {
    pub(crate) fn new(blocks: Vec<Arc<ContextBlock>>) -> Self {
        let locations = blocks
            .iter()
            .enumerate()
            .flat_map(|(block, value)| {
                (0..history_block_len(value)).map(move |offset| (block, offset))
            })
            .collect();
        Self {
            blocks: blocks.into(),
            locations,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.locations.len()
    }

    /// One item as a plain tuple in `HistoryItem` field order; the kernel
    /// builds the named tuples.
    pub(crate) fn get<'py>(&self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyTuple>> {
        let &(block, offset) = self
            .locations
            .get(index)
            .ok_or_else(|| PyIndexError::new_err("transcript index out of range"))?;
        let item = history_item(&self.blocks[block], offset)
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;
        history_tuple(py, item)
    }
}

/// One transcript entry as the notebook's `transcript` shows it.
#[derive(Clone, Debug, Default)]
struct HistoryItem {
    kind: &'static str,
    role: Option<&'static str>,
    sender: Option<String>,
    text: Option<String>,
    content: Vec<HistoryContent>,
    name: Option<String>,
    call_id: Option<String>,
    summary: Vec<String>,
    images: Vec<HistoryImage>,
    provider: Option<HistoryProviderData>,
    status: Option<&'static str>,
    phase: Option<&'static str>,
    tool_type: Option<&'static str>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    at: Option<i64>,
    retain_from: Option<u64>,
    call_ids: Vec<String>,
    response_id: Option<String>,
    metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
struct HistoryContent {
    kind: &'static str,
    text: Option<String>,
    media_type: Option<String>,
    data: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct HistoryImage {
    media_type: String,
    data: Vec<u8>,
    detail: Option<&'static str>,
}

#[derive(Clone, Debug)]
struct HistoryProviderData {
    tag: String,
    data: Vec<u8>,
}

fn history_block_len(block: &ContextBlock) -> usize {
    match block {
        ContextBlock::ToolResults { results } => results.len(),
        ContextBlock::InferenceResponse { items, .. } => items.len(),
        _ => 1,
    }
}

fn history_content(content: &[ContentPart]) -> Vec<HistoryContent> {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => HistoryContent {
                kind: "text",
                text: Some(text.clone()),
                media_type: None,
                data: None,
            },
            ContentPart::Image { media_type, data } => HistoryContent {
                kind: "image",
                text: None,
                media_type: Some(media_type.clone()),
                data: Some(data.clone()),
            },
        })
        .collect()
}

fn history_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect()
}

fn history_images(images: &[rho_core::ImageContent]) -> Vec<HistoryImage> {
    images
        .iter()
        .map(|image| HistoryImage {
            media_type: image.media_type.clone(),
            data: image.data.clone(),
            detail: Some(match image.detail {
                rho_core::ImageDetail::High => "high",
                rho_core::ImageDetail::Original => "original",
            }),
        })
        .collect()
}

fn provider_data(
    provider: &dyn rho_core::ProviderSpecificData,
) -> Result<Option<HistoryProviderData>, String> {
    let encoded =
        senax_encoder::encode(&provider.clone_box()).map_err(|error| error.to_string())?;
    Ok(Some(HistoryProviderData {
        tag: provider.tag().to_owned(),
        data: encoded.to_vec(),
    }))
}

fn tool_type(value: ToolType) -> &'static str {
    match value {
        ToolType::Function => "function",
        ToolType::Custom => "custom",
    }
}

fn output_status(value: ToolOutputStatus) -> &'static str {
    match value {
        ToolOutputStatus::Success => "success",
        ToolOutputStatus::Error => "error",
        ToolOutputStatus::Cancelled => "cancelled",
    }
}

fn history_item(block: &ContextBlock, offset: usize) -> Result<HistoryItem, String> {
    let item = match block {
        ContextBlock::UserMessage { sender, content } => {
            let (role, sender) = match sender {
                MessageSender::User => ("user", None),
                MessageSender::Agent { id } => ("agent", Some(id.encoded().to_owned())),
            };
            HistoryItem {
                kind: "message",
                role: Some(role),
                sender,
                text: Some(history_text(content)),
                content: history_content(content),
                ..HistoryItem::default()
            }
        }
        ContextBlock::DeveloperMessage { text } => HistoryItem {
            kind: "message",
            role: Some("developer"),
            text: Some(text.clone()),
            ..HistoryItem::default()
        },
        ContextBlock::ToolResults { results } => {
            let result = &results[offset];
            HistoryItem {
                kind: "tool_result",
                call_id: Some(result.call_id.as_str().to_owned()),
                tool_type: Some(tool_type(result.tool_type)),
                text: Some(result.body.output.as_str().to_owned()),
                images: history_images(&result.body.images),
                status: Some(output_status(result.body.status)),
                started_at: Some(result.started_at.0 as i64),
                finished_at: Some(result.finished_at.0 as i64),
                metadata: result
                    .metadata
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|error| error.to_string())?,
                ..HistoryItem::default()
            }
        }
        ContextBlock::ToolUpdate(update) => HistoryItem {
            kind: "tool_update",
            call_id: Some(update.call_id.as_str().to_owned()),
            tool_type: Some(tool_type(update.tool_type)),
            text: Some(update.output.as_str().to_owned()),
            images: history_images(&update.images),
            status: update.status.map(output_status),
            at: Some(update.at.0 as i64),
            ..HistoryItem::default()
        },
        ContextBlock::InferenceResponse {
            items,
            provider_response_id,
        } => {
            let response_id = provider_response_id
                .as_ref()
                .map(|id| id.as_str().to_owned());
            match &items[offset] {
                InferenceResponseItem::AssistantMessage {
                    provider_specific,
                    content,
                    phase,
                } => HistoryItem {
                    kind: "message",
                    role: Some("assistant"),
                    text: Some(history_text(content)),
                    content: history_content(content),
                    phase: phase.map(|phase| match phase {
                        rho_core::MessagePhase::Commentary => "commentary",
                        rho_core::MessagePhase::FinalAnswer => "final_answer",
                    }),
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::ToolCall {
                    provider_specific,
                    id,
                    name,
                    tool_type: kind,
                    arguments,
                } => HistoryItem {
                    kind: "tool_call",
                    name: Some(name.as_str().to_owned()),
                    text: Some(arguments.clone()),
                    call_id: Some(id.as_str().to_owned()),
                    tool_type: Some(tool_type(*kind)),
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::EncryptedReasoning {
                    provider_specific,
                    summary,
                } => HistoryItem {
                    kind: "encrypted_reasoning",
                    summary: summary.clone(),
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::RawReasoning {
                    provider_specific,
                    content,
                    summary,
                } => HistoryItem {
                    kind: "reasoning",
                    text: Some(content.clone()),
                    summary: summary.clone(),
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::Compaction { provider_specific } => HistoryItem {
                    kind: "compaction",
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
                InferenceResponseItem::Unknown { provider_specific } => HistoryItem {
                    kind: "unknown",
                    provider: provider_data(provider_specific.as_ref())?,
                    response_id,
                    ..HistoryItem::default()
                },
            }
        }
        ContextBlock::CompactionTrigger => HistoryItem {
            kind: "compaction_trigger",
            ..HistoryItem::default()
        },
        ContextBlock::ContextRotation { retain_from } => HistoryItem {
            kind: "context_rotation",
            retain_from: Some(*retain_from),
            ..HistoryItem::default()
        },
        ContextBlock::ToolHistoryEvicted { call_ids } => HistoryItem {
            kind: "tool_history_evicted",
            call_ids: call_ids.iter().map(|id| id.as_str().to_owned()).collect(),
            ..HistoryItem::default()
        },
    };
    Ok(item)
}

fn history_tuple(py: Python<'_>, item: HistoryItem) -> PyResult<Bound<'_, PyTuple>> {
    let bytes = |data: &[u8]| PyBytes::new(py, data).into_any();
    let content = item
        .content
        .into_iter()
        .map(|part| {
            let data = part.data.as_deref().map(bytes);
            (part.kind, part.text, part.media_type, data).into_bound_py_any(py)
        })
        .collect::<PyResult<Vec<_>>>()?;
    let images = item
        .images
        .into_iter()
        .map(|image| (image.media_type, bytes(&image.data), image.detail).into_bound_py_any(py))
        .collect::<PyResult<Vec<_>>>()?;
    let provider = item
        .provider
        .map(|provider| (provider.tag, bytes(&provider.data)).into_bound_py_any(py))
        .transpose()?;
    let metadata = item.metadata.as_ref().map(serde_json::Value::to_string);
    PyTuple::new(
        py,
        [
            item.kind.into_bound_py_any(py)?,
            item.role.into_bound_py_any(py)?,
            item.sender.into_bound_py_any(py)?,
            item.text.into_bound_py_any(py)?,
            PyTuple::new(py, content)?.into_any(),
            item.name.into_bound_py_any(py)?,
            item.call_id.into_bound_py_any(py)?,
            PyTuple::new(py, item.summary)?.into_any(),
            PyTuple::new(py, images)?.into_any(),
            provider.into_bound_py_any(py)?,
            item.status.into_bound_py_any(py)?,
            item.phase.into_bound_py_any(py)?,
            item.tool_type.into_bound_py_any(py)?,
            item.started_at.into_bound_py_any(py)?,
            item.finished_at.into_bound_py_any(py)?,
            item.at.into_bound_py_any(py)?,
            item.retain_from.into_bound_py_any(py)?,
            PyTuple::new(py, item.call_ids)?.into_any(),
            item.response_id.into_bound_py_any(py)?,
            metadata.into_bound_py_any(py)?,
        ],
    )
}
