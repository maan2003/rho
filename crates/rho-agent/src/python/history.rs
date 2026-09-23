//! The conversation as notebook Python sees it: `transcript`, a lazy
//! sequence over a snapshot taken when the cell was admitted.
use std::sync::Arc;

use pyo3::exceptions::{PyIndexError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyModule, PyTuple};
use rho_agent_host_proto::{ContentPart, ToolOutputStatus};
use rho_inference::types::{
    ContextBlock, InferenceResponseItem, MessageSender, ProviderSpecificData, ToolType,
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

    /// One item as the kernel's `HistoryItem`.
    pub(crate) fn get<'py>(&self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyAny>> {
        let &(block, offset) = self
            .locations
            .get(index)
            .ok_or_else(|| PyIndexError::new_err("transcript index out of range"))?;
        history_item(py, &self.blocks[block], offset)
    }
}

fn history_block_len(block: &ContextBlock) -> usize {
    match block {
        ContextBlock::ToolResults { results } => results.len(),
        ContextBlock::InferenceResponse { items, .. } => items.len(),
        _ => 1,
    }
}

/// A transcript item under construction: the fields it has, by name. The
/// kernel's named tuples supply the rest as defaults.
struct Item<'py> {
    kernel: Bound<'py, PyModule>,
    fields: Bound<'py, PyDict>,
}

impl<'py> Item<'py> {
    fn new(py: Python<'py>, kind: &str) -> PyResult<Self> {
        let item = Self {
            kernel: crate::python::interpreter::kernel(py)?.clone(),
            fields: PyDict::new(py),
        };
        item.set("kind", kind)
    }

    fn set(self, name: &str, value: impl IntoPyObject<'py>) -> PyResult<Self> {
        self.fields.set_item(name, value)?;
        Ok(self)
    }

    fn py(&self) -> Python<'py> {
        self.fields.py()
    }

    fn content(self, content: &[ContentPart]) -> PyResult<Self> {
        let text: String = content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                ContentPart::Image { .. } => None,
            })
            .collect();
        let make = self.kernel.getattr("HistoryContent")?;
        let parts = content
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => make.call1(("text", text)),
                ContentPart::Image { media_type, data } => make.call1((
                    "image",
                    None::<&str>,
                    media_type,
                    PyBytes::new(self.py(), data),
                )),
            })
            .collect::<PyResult<Vec<_>>>()?;
        let parts = PyTuple::new(self.py(), parts)?;
        self.set("text", text)?.set("content", parts)
    }

    fn images(self, images: &[rho_inference::types::ImageContent]) -> PyResult<Self> {
        let make = self.kernel.getattr("HistoryImage")?;
        let images = images
            .iter()
            .map(|image| {
                let detail = match image.detail {
                    rho_inference::types::ImageDetail::High => "high",
                    rho_inference::types::ImageDetail::Original => "original",
                };
                make.call1((
                    &image.media_type,
                    PyBytes::new(self.py(), &image.data),
                    detail,
                ))
            })
            .collect::<PyResult<Vec<_>>>()?;
        let images = PyTuple::new(self.py(), images)?;
        self.set("images", images)
    }

    fn strings<'a>(self, name: &str, values: impl IntoIterator<Item = &'a str>) -> PyResult<Self> {
        let values = PyTuple::new(self.py(), values.into_iter().collect::<Vec<_>>())?;
        self.set(name, values)
    }

    fn provider(self, provider: &dyn ProviderSpecificData) -> PyResult<Self> {
        let encoded = senax_encoder::encode(&provider.clone_box())
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        let data = self
            .kernel
            .getattr("HistoryProviderData")?
            .call1((provider.tag(), PyBytes::new(self.py(), &encoded)))?;
        self.set("provider", data)
    }

    fn build(self) -> PyResult<Bound<'py, PyAny>> {
        self.kernel
            .getattr("HistoryItem")?
            .call((), Some(&self.fields))
    }
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

fn history_item<'py>(
    py: Python<'py>,
    block: &ContextBlock,
    offset: usize,
) -> PyResult<Bound<'py, PyAny>> {
    let item = |kind| Item::new(py, kind);
    match block {
        ContextBlock::UserMessage { sender, content } => {
            let (role, sender) = match sender {
                MessageSender::User => ("user", None),
                MessageSender::Agent { id } => ("agent", Some(id.encoded())),
            };
            item("message")?
                .set("role", role)?
                .set("sender", sender)?
                .content(content)?
                .build()
        }
        ContextBlock::DeveloperMessage { text } => item("message")?
            .set("role", "developer")?
            .set("text", text)?
            .build(),
        ContextBlock::ToolResults { results } => {
            let result = &results[offset];
            let metadata = match &result.metadata {
                Some(metadata) => {
                    let json = serde_json::to_string(metadata)
                        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
                    Some(
                        crate::python::interpreter::kernel(py)?
                            .call_method1("frozen_json", (json,))?,
                    )
                }
                None => None,
            };
            item("tool_result")?
                .set("call_id", result.call_id.as_str())?
                .set("tool_type", tool_type(result.tool_type))?
                .set("text", result.body.output.as_str())?
                .images(&result.body.images)?
                .set("status", output_status(result.body.status))?
                .set("started_at", result.started_at.0)?
                .set("finished_at", result.finished_at.0)?
                .set("metadata", metadata)?
                .build()
        }
        ContextBlock::ToolUpdate(update) => item("tool_update")?
            .set("call_id", update.call_id.as_str())?
            .set("tool_type", tool_type(update.tool_type))?
            .set("text", update.output.as_str())?
            .images(&update.images)?
            .set("status", update.status.map(output_status))?
            .set("at", update.at.0)?
            .build(),
        ContextBlock::InferenceResponse {
            items,
            provider_response_id,
        } => {
            let response = |kind, provider: &dyn ProviderSpecificData| {
                item(kind)?.provider(provider)?.set(
                    "response_id",
                    provider_response_id.as_ref().map(|id| id.as_str()),
                )
            };
            match &items[offset] {
                InferenceResponseItem::AssistantMessage {
                    provider_specific,
                    content,
                    phase,
                } => response("message", provider_specific.as_ref())?
                    .set("role", "assistant")?
                    .content(content)?
                    .set(
                        "phase",
                        phase.map(|phase| match phase {
                            rho_agent_host_proto::MessagePhase::Commentary => "commentary",
                            rho_agent_host_proto::MessagePhase::FinalAnswer => "final_answer",
                        }),
                    )?
                    .build(),
                InferenceResponseItem::ToolCall {
                    provider_specific,
                    id,
                    name,
                    tool_type: kind,
                    arguments,
                } => response("tool_call", provider_specific.as_ref())?
                    .set("name", name.as_str())?
                    .set("text", arguments)?
                    .set("call_id", id.as_str())?
                    .set("tool_type", tool_type(*kind))?
                    .build(),
                InferenceResponseItem::EncryptedReasoning {
                    provider_specific,
                    summary,
                } => response("encrypted_reasoning", provider_specific.as_ref())?
                    .strings("summary", summary.iter().map(String::as_str))?
                    .build(),
                InferenceResponseItem::RawReasoning {
                    provider_specific,
                    content,
                    summary,
                } => response("reasoning", provider_specific.as_ref())?
                    .set("text", content)?
                    .strings("summary", summary.iter().map(String::as_str))?
                    .build(),
                InferenceResponseItem::Compaction { provider_specific } => {
                    response("compaction", provider_specific.as_ref())?.build()
                }
                InferenceResponseItem::Unknown { provider_specific } => {
                    response("unknown", provider_specific.as_ref())?.build()
                }
            }
        }
        ContextBlock::CompactionTrigger => item("compaction_trigger")?.build(),
        ContextBlock::ContextRotation { retain_from } => item("context_rotation")?
            .set("retain_from", *retain_from)?
            .build(),
        ContextBlock::ToolHistoryEvicted { call_ids } => item("tool_history_evicted")?
            .strings("call_ids", call_ids.iter().map(|id| id.as_str()))?
            .build(),
    }
}
