//! Native model action contract. Wire tool names and shapes do not leak into
//! the notebook's execution API.
use crate::types::{ToolFormat, ToolName, ToolSpec, ToolType};

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from("exec").unwrap(),
        tool_type: ToolType::Custom,
        description: "Execute Python in the persistent notebook.".into(),
        input_schema: serde_json::Value::Null,
        format: Some(ToolFormat::Text),
    }
}
