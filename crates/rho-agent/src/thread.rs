//! Append-only agent conversation history.
//!
//! The agent owns this shape so providers receive stable `ItemBlock` snapshots
//! without being responsible for local queue/tool-result policy.

use rho::{Item, ItemBlock, ProviderRequest, ToolSpec};

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentThread {
    blocks: Vec<ItemBlock>,
    tools: Vec<ToolSpec>,
}

impl AgentThread {
    pub(crate) fn from_blocks(blocks: Vec<ItemBlock>) -> Self {
        Self {
            blocks,
            tools: Vec::new(),
        }
    }

    pub(crate) fn append_block(&mut self, block: ItemBlock) {
        self.blocks.push(block);
    }

    pub(crate) fn replace_tools(&mut self, tools: Vec<ToolSpec>) {
        self.tools = tools;
    }

    pub(crate) fn blocks(&self) -> &[ItemBlock] {
        &self.blocks
    }

    pub(crate) fn items(&self) -> Vec<Item> {
        self.blocks.iter().flat_map(block_items).cloned().collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub(crate) fn next_item_index(&self) -> u64 {
        self.blocks
            .iter()
            .map(|block| block_items(block).len())
            .sum::<usize>() as u64
    }

    pub(crate) fn provider_request(&self) -> ProviderRequest {
        ProviderRequest {
            input: self.blocks.clone(),
            tools: self.tools.clone(),
        }
    }
}

fn block_items(block: &ItemBlock) -> &[Item] {
    match block {
        ItemBlock::Local { items } | ItemBlock::ProviderResponse { items, .. } => items,
    }
}

#[cfg(test)]
mod tests {
    use rho::{Item, ItemBlock, Role, ToolSpec, ToolType};
    use serde_json::json;

    use super::*;

    #[test]
    fn keeps_blocks_append_only_and_ordered() {
        let mut thread = AgentThread::default();

        thread.append_block(ItemBlock::Local {
            items: vec![Item::message("item-0", Role::User, "first")],
        });
        thread.append_block(ItemBlock::ProviderResponse {
            provider_response_id: Some("resp_1".to_owned()),
            items: vec![Item::message("item-1", Role::Assistant, "done")],
        });

        assert_eq!(thread.blocks().len(), 2);
        assert!(
            matches!(&thread.blocks()[0], ItemBlock::Local { items } if items[0].id.0 == "item-0")
        );
        assert!(
            matches!(&thread.blocks()[1], ItemBlock::ProviderResponse { items, .. } if items[0].id.0 == "item-1")
        );
        assert_eq!(thread.next_item_index(), 2);
    }

    #[test]
    fn provider_request_is_a_snapshot_of_blocks_and_tools() {
        let mut thread = AgentThread::default();
        thread.replace_tools(vec![ToolSpec {
            name: "shell_command".to_owned(),
            tool_type: ToolType::Function,
            description: "Run a shell command.".to_owned(),
            input_schema: json!({ "type": "object" }),
            format: None,
        }]);
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
