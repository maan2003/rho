//! The agent's invariant-guarded state.
//!
//! This type exists to hold the things that must obey an invariant for the rest
//! of the agent to stay correct, and to make those invariants impossible to
//! violate from the outside. Each field documents the invariant it upholds and
//! how it is enforced. If you have a value that "must always be X", it belongs
//! here behind an API that guarantees X.
//!
//! Together these fields are the persistent inputs to an inference request
//! (`inference_request`): the conversation so far plus the tools available.

use rho_core::{InferenceRequest, ItemBlock, ToolSpec};
use tokio::sync::broadcast;

use crate::observable::Observable;

#[derive(Clone, Debug)]
pub(crate) struct AgentInvariantsEnforcer {
    /// Invariant: append-only. Blocks are only ever pushed — never removed,
    /// replaced, or reordered. Enforced by `Observable`, which exposes `update`
    /// (used here only to append) and no remove/set-by-index.
    blocks: Observable<Vec<ItemBlock>, ItemBlock>,
    /// Invariant: immutable. Set once at construction and never changed for the
    /// life of the agent. Enforced by exposing no mutator.
    tool_specs: Vec<ToolSpec>,
}

impl AgentInvariantsEnforcer {
    pub(crate) fn new(tool_specs: Vec<ToolSpec>, blocks: Vec<ItemBlock>) -> Self {
        Self {
            blocks: Observable::new(blocks),
            tool_specs,
        }
    }

    pub(crate) fn append_block(&self, block: ItemBlock) {
        self.blocks.update(|blocks| {
            blocks.push(block.clone());
            block
        });
    }

    pub(crate) fn snapshot(&self) -> Vec<ItemBlock> {
        self.blocks.snapshot()
    }

    /// Current history plus a receiver for every later appended block, taken
    /// atomically so a follower misses nothing and double-counts nothing.
    pub(crate) fn subscribe(&self) -> (Vec<ItemBlock>, broadcast::Receiver<ItemBlock>) {
        self.blocks.subscribe()
    }

    pub(crate) fn inference_request(&self) -> InferenceRequest {
        InferenceRequest {
            input: self.blocks.snapshot(),
            tools: self.tool_specs.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_core::{Item, ItemBlock, Role, ToolSpec, ToolType};
    use serde_json::json;

    use super::*;

    #[test]
    fn keeps_blocks_append_only_and_ordered() {
        let enforcer = AgentInvariantsEnforcer::new(Vec::new(), Vec::new());

        enforcer.append_block(ItemBlock::Local {
            items: vec![Item::message("item-0", Role::User, "first")],
        });
        enforcer.append_block(ItemBlock::InferenceResponse {
            provider_response_id: Some("resp_1".to_owned()),
            items: vec![Item::message("item-1", Role::Assistant, "done")],
        });

        let blocks = enforcer.snapshot();
        assert!(matches!(&blocks[0], ItemBlock::Local { items } if items[0].id.0 == "item-0"));
        assert!(
            matches!(&blocks[1], ItemBlock::InferenceResponse { items, .. } if items[0].id.0 == "item-1")
        );
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn inference_request_is_a_snapshot_of_blocks_and_tools() {
        let enforcer = AgentInvariantsEnforcer::new(
            vec![ToolSpec {
                name: "shell_command".to_owned(),
                tool_type: ToolType::Function,
                description: "Run a shell command.".to_owned(),
                input_schema: json!({ "type": "object" }),
                format: None,
            }],
            Vec::new(),
        );
        enforcer.append_block(ItemBlock::Local {
            items: vec![Item::message("item-0", Role::User, "hello")],
        });

        let request = enforcer.inference_request();
        enforcer.append_block(ItemBlock::Local {
            items: vec![Item::message("item-1", Role::User, "later")],
        });

        assert_eq!(request.input.len(), 1);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(enforcer.snapshot().len(), 2);
    }
}
