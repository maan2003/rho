//! Append-only agent conversation history.
//!
//! The agent owns this shape so providers receive stable `ItemBlock` snapshots
//! without being responsible for local queue/tool-result policy.

use rho::{ItemBlock, ProviderRequest, ToolSpec};

#[derive(Clone, Debug)]
pub(crate) struct AgentThread {
    blocks: Vec<ItemBlock>,
    tool_specs: Vec<ToolSpec>,
}

impl AgentThread {
    pub(crate) fn new(tool_specs: Vec<ToolSpec>, blocks: Vec<ItemBlock>) -> Self {
        Self { blocks, tool_specs }
    }

    pub(crate) fn append_block(&mut self, block: ItemBlock) {
        self.blocks.push(block);
    }

    pub(crate) fn blocks(&self) -> &[ItemBlock] {
        &self.blocks
    }

    pub(crate) fn provider_request(&self) -> ProviderRequest {
        ProviderRequest {
            input: self.blocks.clone(),
            tools: self.tool_specs.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use rho::{Item, ItemBlock, Role, ToolSpec, ToolType};
    use serde_json::json;

    use super::*;

    #[test]
    fn keeps_blocks_append_only_and_ordered() {
        let mut thread = AgentThread::new(Vec::new(), Vec::new());

        thread.append_block(ItemBlock::Local {
            items: vec![Item::message("item-0", Role::User, "first")],
        });
        thread.append_block(ItemBlock::ProviderResponse {
            provider_response_id: Some("resp_1".to_owned()),
            items: vec![Item::message("item-1", Role::Assistant, "done")],
        });

        assert!(
            matches!(&thread.blocks()[0], ItemBlock::Local { items } if items[0].id.0 == "item-0")
        );
        assert!(
            matches!(&thread.blocks()[1], ItemBlock::ProviderResponse { items, .. } if items[0].id.0 == "item-1")
        );
        assert_eq!(thread.blocks().len(), 2);
    }

    #[test]
    fn provider_request_is_a_snapshot_of_blocks_and_tools() {
        let mut thread = AgentThread::new(
            vec![ToolSpec {
                name: "shell_command".to_owned(),
                tool_type: ToolType::Function,
                description: "Run a shell command.".to_owned(),
                input_schema: json!({ "type": "object" }),
                format: None,
            }],
            Vec::new(),
        );
        thread.append_block(ItemBlock::Local {
            items: vec![Item::message("item-0", Role::User, "hello")],
        });

        let request = thread.provider_request();
        thread.append_block(ItemBlock::Local {
            items: vec![Item::message("item-1", Role::User, "later")],
        });

        assert_eq!(request.input.len(), 1);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(thread.blocks().len(), 2);
    }
}
