use rho_core::{ToolCall, ToolCallId, ToolName, ToolOutputStatus, ToolType};

use super::*;

fn shell_call(arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from("call-1").unwrap(),
        name: ToolName::try_from(SHELL_COMMAND_TOOL_NAME).unwrap(),
        tool_type: ToolType::Function,
        arguments: arguments.to_string(),
    }
}

fn patch_call(arguments: impl Into<String>) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from("call-1").unwrap(),
        name: ToolName::try_from(APPLY_PATCH_TOOL_NAME).unwrap(),
        tool_type: ToolType::Custom,
        arguments: arguments.into(),
    }
}

#[tokio::test]
async fn runs_shell_call() {
    let tools = ShellTools::new(Duration::from_secs(2));
    let result = tools
        .call(shell_call(json!({"command": "printf hello"})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    let content: Value = serde_json::from_str(result.output.as_ref()).unwrap();
    assert_eq!(content["output"], "hello");
    assert_eq!(content["stdout"], "hello");
    assert_eq!(content["stderr"], "");
    assert_eq!(content["status"], 0);
}

#[tokio::test]
async fn nonzero_exit_is_structured_result_not_tool_error() {
    let tools = ShellTools::new(Duration::from_secs(2));
    let result = tools
        .call(shell_call(json!({"command": "printf nope; exit 3"})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    let content: Value = serde_json::from_str(result.output.as_ref()).unwrap();
    assert_eq!(content["status"], 3);
    assert_eq!(content["output"], "nope");
}

#[tokio::test]
async fn separates_stdout_and_stderr_while_preserving_combined_output() {
    let tools = ShellTools::new(Duration::from_secs(2));
    let result = tools
        .call(shell_call(json!({"command": "printf out; printf err >&2"})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    let content: Value = serde_json::from_str(result.output.as_ref()).unwrap();
    assert_eq!(content["stdout"], "out");
    assert_eq!(content["stderr"], "err");
    assert_eq!(content["output"], "out\nerr");
}

#[tokio::test]
async fn accepts_timeout_argument_name() {
    let tools = ShellTools::new(Duration::from_secs(30));
    let result = tools
        .call(shell_call(json!({"command": "sleep 2", "timeout": 1})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    let content: Value = serde_json::from_str(result.output.as_ref()).unwrap();
    assert_eq!(content["timed_out"], true);
    assert_eq!(content["termination_reason"], "timeout");
}

#[tokio::test]
async fn timeout_kills_shell_process() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker");
    let tools = ShellTools::new(Duration::from_secs(30));
    let result = tools
        .call(shell_call(json!({
            "command": format!("sleep 2; touch {}", marker.display()),
            "timeout": 1,
        })))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    let content: Value = serde_json::from_str(result.output.as_ref()).unwrap();
    assert_eq!(content["timed_out"], true);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!marker.exists());
}

#[test]
fn specs_expose_only_shell_command_and_apply_patch() {
    let tools = ShellTools::new(Duration::from_secs(2));
    let specs = tools.specs();

    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].name.as_str(), SHELL_COMMAND_TOOL_NAME);
    assert_eq!(specs[1].name.as_str(), APPLY_PATCH_TOOL_NAME);
    assert_eq!(specs[1].tool_type, ToolType::Custom);
    assert!(matches!(specs[1].format, Some(ToolFormat::Grammar { .. })));
}

#[tokio::test]
async fn apply_patch_custom_tool_applies_patch() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("hello.txt");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+hello\n*** End Patch",
        path.display()
    );
    let result = ShellTools::new(Duration::from_secs(2))
        .call(patch_call(patch))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(result.output.as_ref().contains("A "));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "hello\n");
}
