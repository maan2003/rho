//! A persisted response as `Detail` hands it back: one `Item` per item
//! that has a face. The same shape the live tail streams, so a client
//! draws both the same way.

use rho_core::{InferenceResponseItem, text_content};
use rho_ui_proto::mirror::Item;

pub(crate) fn item(item: &InferenceResponseItem) -> Option<Item> {
    Some(match item {
        InferenceResponseItem::AssistantMessage { content, phase, .. } => Item::Text {
            text: text_content(content),
            phase: phase.map(rho_agent::live::text_phase),
        },
        InferenceResponseItem::RawReasoning {
            content, summary, ..
        } => Item::Reasoning {
            text: if summary.is_empty() {
                content.clone()
            } else {
                summary.join("\n")
            },
        },
        InferenceResponseItem::EncryptedReasoning { summary, .. } => {
            if summary.is_empty() {
                return None;
            }
            Item::Reasoning {
                text: summary.join("\n"),
            }
        }
        InferenceResponseItem::ToolCall {
            id,
            name,
            arguments,
            ..
        } => Item::ToolCall {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.clone(),
        },
        InferenceResponseItem::Compaction { .. } | InferenceResponseItem::Unknown { .. } => {
            return None;
        }
    })
}
