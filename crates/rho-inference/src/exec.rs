//! Native model action contract. Wire tool names and shapes do not leak into
//! the notebook's execution API.
use rho_agent_types::{ExecCall, InferenceResponseItem, ToolFormat, ToolName, ToolSpec, ToolType};

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from("exec").unwrap(),
        tool_type: ToolType::Custom,
        description: "Execute Python in the persistent notebook.".into(),
        input_schema: serde_json::Value::Null,
        format: Some(ToolFormat::Text),
    }
}

/// One response may contain prose/opaque provider content and at most one exec.
pub fn call(items: &[InferenceResponseItem]) -> Result<Option<ExecCall>, &'static str> {
    let mut exec = None;
    for item in items {
        if let InferenceResponseItem::ToolCall {
            id,
            name,
            tool_type,
            arguments,
            ..
        } = item
        {
            if exec.is_some() || name.as_str() != "exec" || *tool_type != ToolType::Custom {
                return Err(
                    "Native responses permit at most one custom Python exec; no additional calls were executed",
                );
            }
            exec = Some(ExecCall {
                id: id.clone(),
                source: arguments.clone(),
            });
        }
    }
    Ok(exec)
}

/// A validated streamed execution plus opaque replay evidence. Item completion
/// here does not close Python input; the runtime separately validates response
/// EOF.
pub fn stream(
    pending: &rho_agent_types::PendingInferenceResponse,
) -> Result<Option<(usize, InferenceResponseItem, ExecCall)>, String> {
    use rho_agent_types::{StreamingContextItem, StreamingContextItemState};
    let mut selected = None;
    for (index, state) in pending.items.iter().enumerate() {
        if let StreamingContextItemState::Pending(item) | StreamingContextItemState::Finished(item) =
            state
            && matches!(item, StreamingContextItem::ToolCall { .. })
        {
            if selected.is_some() {
                return Err("Python streaming permits only one exec call; previously admitted code is not undone".into());
            }
            let item = item.to_context_item().map_err(|error| error.to_string())?;
            let exec = call(std::slice::from_ref(&item))?.expect("selected a call");
            selected = Some((index, item, exec));
        }
    }
    Ok(selected)
}

/// Restore the accepted source in validated opaque replay evidence.
pub fn set_source(item: &mut InferenceResponseItem, source: String) {
    let InferenceResponseItem::ToolCall { arguments, .. } = item else {
        unreachable!()
    };
    *arguments = source;
}

pub fn output(output: &rho_agent_types::ExecOutput) -> rho_agent_types::ContextBlock {
    use rho_agent_types::{ContextBlock, ExecOutput, ToolResult, ToolUpdate};
    match output {
        ExecOutput::Reply {
            id,
            body,
            first_block_at,
            at,
        } => ContextBlock::ToolResults {
            results: vec![ToolResult {
                call_id: id.clone(),
                tool_type: ToolType::Custom,
                body: body.clone(),
                started_at: *first_block_at,
                finished_at: *at,
                metadata: None,
            }],
        },
        ExecOutput::Report { id, body, at } => ContextBlock::ToolUpdate(ToolUpdate {
            status: Some(body.status),
            call_id: id.clone(),
            tool_type: ToolType::Custom,
            output: body.output.clone(),
            full_output: body.full_output.clone(),
            images: body.images.clone(),
            at: *at,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(name: &str, kind: ToolType) -> InferenceResponseItem {
        InferenceResponseItem::ToolCall {
            provider_specific: Box::new(crate::OpenAiResponsesProviderData::CustomToolCall {
                item_id: "item".try_into().unwrap(),
            }),
            id: "exec-1".try_into().unwrap(),
            name: name.try_into().unwrap(),
            tool_type: kind,
            arguments: "pass".into(),
        }
    }

    #[test]
    fn one_exec_or_prose_and_no_generic_tool_dispatch() {
        assert_eq!(call(&[]), Ok(None));
        let exec = invocation("exec", ToolType::Custom);
        assert_eq!(
            call(std::slice::from_ref(&exec)),
            Ok(Some(ExecCall {
                id: "exec-1".try_into().unwrap(),
                source: "pass".into(),
            }))
        );
        assert!(call(&[exec.clone(), exec]).is_err());
        assert!(call(&[invocation("shell", ToolType::Custom)]).is_err());
        assert!(call(&[invocation("exec", ToolType::Function)]).is_err());
        assert_eq!(spec().tool_type, ToolType::Custom);
    }
}
