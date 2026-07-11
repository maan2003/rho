use rho_core::{ToolCall, ToolCallId, ToolName, ToolOutputStatus, ToolType};
use rho_workspaces::PathOverrides;

use super::*;

fn test_tools(timeout_secs: u64) -> ShellTools {
    ShellTools::in_directory(
        Duration::from_secs(timeout_secs),
        camino::Utf8PathBuf::try_from(std::env::temp_dir()).unwrap(),
        PathOverrides::default(),
    )
}

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
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(json!({"command": "printf hello"})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(serde_json::from_str::<Value>(result.output.as_ref()).is_err());
    assert!(result.output.as_ref().contains("Exit code: 0"));
    assert!(result.output.as_ref().contains("Output:\nhello"));
}

#[tokio::test]
async fn shell_command_stdin_is_null() {
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(
            json!({"command": "if read line; then printf got; else printf eof; fi"}),
        ))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(result.output.as_ref().contains("Output:\neof"));
}

#[tokio::test]
async fn shell_command_inherits_tool_environment() {
    let tools = test_tools(2).with_env("RHO_AGENT_ID", "agent-id");
    let result = tools
        .call(shell_call(
            json!({"command": "printf '%s' \"$RHO_AGENT_ID\""}),
        ))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(result.output.as_ref().contains("Output:\nagent-id"));
}

#[tokio::test]
async fn nonzero_exit_is_structured_result_not_tool_error() {
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(json!({"command": "printf nope; exit 3"})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(result.output.as_ref().contains("Exit code: 3"));
    assert!(result.output.as_ref().contains("Output:\nnope"));
}

#[tokio::test]
async fn interleaves_stdout_and_stderr_in_read_order() {
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(json!({
            "command": "printf out; sleep 0.05; printf err >&2; sleep 0.05; printf out2"
        })))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(result.output.as_ref().contains("Output:\nouterrout2"));
}

#[tokio::test]
async fn truncates_concatenated_output() {
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(json!({"command": "yes line | head -20000"})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(serde_json::from_str::<Value>(result.output.as_ref()).is_err());
    assert!(
        result
            .output
            .as_ref()
            .contains("Warning: truncated output (original token count:")
    );
    assert!(result.output.as_ref().contains("Total output lines: 20000"));
    assert!(result.output.as_ref().contains("tokens truncated"));
    assert!(result.output.len() < MAX_OUTPUT_BYTES + 2048);
}

#[tokio::test]
async fn accepts_timeout_argument_name() {
    let tools = test_tools(30);
    let result = tools
        .call(shell_call(json!({"command": "sleep 2", "timeout": 1})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(
        result
            .output
            .as_ref()
            .contains("Command timed out after 1 seconds")
    );
}

#[tokio::test]
async fn timeout_kills_shell_process() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker");
    let tools = test_tools(30);
    let result = tools
        .call(shell_call(json!({
            "command": format!("sleep 2; touch {}", marker.display()),
            "timeout": 1,
        })))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(
        result
            .output
            .as_ref()
            .contains("Command timed out after 1 seconds")
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!marker.exists());
}

#[test]
fn specs_expose_only_shell_command_and_apply_patch() {
    let tools = test_tools(2);
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
    let result = test_tools(2).call(patch_call(patch)).await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(result.output.as_ref().contains("A "));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "hello\n");
}

#[tokio::test]
async fn shell_commands_run_in_the_agents_working_directory() {
    let temp = tempfile::tempdir().unwrap();
    let tools = ShellTools::in_directory(
        Duration::from_secs(2),
        camino::Utf8PathBuf::try_from(temp.path().to_path_buf()).unwrap(),
        PathOverrides::default(),
    );
    let result = tools.call(shell_call(json!({"command": "pwd"}))).await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    let expected = temp.path().canonicalize().unwrap();
    assert!(
        result.output.as_ref().contains(expected.to_str().unwrap()),
        "expected pwd under {expected:?}, got: {}",
        result.output
    );
}

#[tokio::test]
async fn relative_model_cwd_resolves_against_working_directory() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("sub")).unwrap();
    let tools = ShellTools::in_directory(
        Duration::from_secs(2),
        camino::Utf8PathBuf::try_from(temp.path().to_path_buf()).unwrap(),
        PathOverrides::default(),
    );
    let result = tools
        .call(shell_call(json!({"command": "pwd", "cwd": "sub"})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(result.output.as_ref().contains("/sub"));
}

#[tokio::test]
async fn shell_path_overrides_prepend_and_append_entries() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let before = temp.path().join("before");
    let after = temp.path().join("after");
    std::fs::create_dir_all(&before).unwrap();
    std::fs::create_dir_all(&after).unwrap();

    let selected = before.join("rho-path-selected");
    std::fs::write(&selected, "#!/bin/sh\nprintf before\n").unwrap();
    std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o755)).unwrap();

    let shadowed = after.join("rho-path-selected");
    std::fs::write(&shadowed, "#!/bin/sh\nprintf after\n").unwrap();
    std::fs::set_permissions(&shadowed, std::fs::Permissions::from_mode(0o755)).unwrap();

    let after_only = after.join("rho-path-after-only");
    std::fs::write(&after_only, "#!/bin/sh\nprintf after-only\n").unwrap();
    std::fs::set_permissions(&after_only, std::fs::Permissions::from_mode(0o755)).unwrap();

    let tools = ShellTools::in_directory(
        Duration::from_secs(2),
        camino::Utf8PathBuf::try_from(temp.path().to_path_buf()).unwrap(),
        PathOverrides {
            before: vec![before],
            after: vec![after],
        },
    );
    let result = tools
        .call(shell_call(
            json!({"command": "rho-path-selected; printf ' '; rho-path-after-only"}),
        ))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(
        result
            .output
            .as_ref()
            .contains("Output:\nbefore after-only"),
        "{}",
        result.output
    );
}
