use rho_agent_types::ToolOutputStatus;
use rho_agent_types::transcript::{ExecId, ToolCall, ToolName, ToolType};
use rho_fs_view::PathOverrides;

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
        id: ExecId::try_from("call-1").unwrap(),
        name: ToolName::try_from(EXEC_COMMAND_TOOL_NAME).unwrap(),
        tool_type: ToolType::Function,
        arguments: arguments.to_string(),
    }
}

fn patch_call(arguments: impl Into<String>) -> ToolCall {
    ToolCall {
        id: ExecId::try_from("call-1").unwrap(),
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
    assert!(
        result
            .output
            .as_ref()
            .contains("Process exited with code 0"),
        "{}",
        result.output
    );
    assert!(result.output.as_ref().contains("Output:\nhello"));
}

#[tokio::test]
async fn a_pipeline_fails_when_any_stage_does() {
    let result = test_tools(2)
        .call_code_mode(shell_call(
            json!({"cmd": "sh -c 'echo partial; exit 3' | cat"}),
        ))
        .await
        .unwrap();
    assert_eq!(result["exit_code"], 3, "pipefail reports the failing stage");
    assert_eq!(result["output"], "partial\n");
}

#[tokio::test]
async fn code_mode_receives_structured_exec_output() {
    let result = test_tools(2)
        .call_code_mode(shell_call(json!({"cmd": "printf hello"})))
        .await
        .unwrap();
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["output"], "hello");
    assert!(result["chunk_id"].is_string());
    assert!(result.get("session_id").is_none());
}

#[tokio::test]
async fn write_stdin_continues_a_running_process() {
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(
            json!({"cmd": "read line; printf 'got:%s' \"$line\"", "yield_time_ms": 1}),
        ))
        .await;
    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(
        result
            .output
            .as_ref()
            .contains("Process running with session ID 1"),
        "{}",
        result.output
    );
    let result = tools
        .call(ToolCall {
            id: ExecId::try_from("call-2").unwrap(),
            name: ToolName::try_from(WRITE_STDIN_TOOL_NAME).unwrap(),
            tool_type: ToolType::Function,
            arguments: json!({"session_id": 1, "chars": "hello\n"}).to_string(),
        })
        .await;
    assert!(
        result
            .output
            .as_ref()
            .contains("Process exited with code 0"),
        "{}",
        result.output
    );
    assert!(result.output.as_ref().contains("Output:\ngot:hello"));
}

#[tokio::test]
async fn exec_command_waits_for_the_command_by_default() {
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(json!({"cmd": "sleep 0.4; printf done"})))
        .await;
    assert!(
        result
            .output
            .as_ref()
            .contains("Process exited with code 0"),
        "{}",
        result.output
    );
    assert!(result.output.as_ref().contains("Output:\ndone"));
}

#[tokio::test]
async fn an_empty_poll_returns_as_soon_as_the_process_says_something() {
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(json!({
            "cmd": "sleep 0.4; echo first; sleep 0.4; echo second; sleep 5",
            "yield_time_ms": 1
        })))
        .await;
    assert!(result.output.as_ref().contains("Process running"));
    for expected in ["first", "second"] {
        let started = std::time::Instant::now();
        let result = tools
            .call(ToolCall {
                id: ExecId::try_from(format!("poll-{expected}").as_str()).unwrap(),
                name: ToolName::try_from(WRITE_STDIN_TOOL_NAME).unwrap(),
                tool_type: ToolType::Function,
                // A tiny yield is raised to the poll floor, so this returns on
                // output rather than after 1 ms with nothing.
                arguments: json!({"session_id": 1, "yield_time_ms": 1}).to_string(),
            })
            .await;
        assert!(
            result.output.as_ref().contains(expected),
            "{}",
            result.output
        );
        assert!(!result.output.as_ref().contains("Process exited"));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "poll did not return on output"
        );
    }
}

#[tokio::test]
async fn reaps_a_session_that_exits_without_another_poll() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("pid");
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(json!({
            "cmd": format!("printf '%s' $$ > {}; sleep 0.5", pid_file.display()),
            "yield_time_ms": 1
        })))
        .await;
    assert!(result.output.as_ref().contains("Process running"));

    tokio::time::sleep(Duration::from_millis(900)).await;
    let pid = std::fs::read_to_string(pid_file).unwrap();
    assert!(
        !std::path::Path::new("/proc").join(pid).exists(),
        "unpolled shell session was not reaped"
    );
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
async fn shell_inherits_process_environment_and_explicit_override_wins() {
    let inherited_home = std::env::var("HOME").unwrap();
    let inherited = test_tools(2)
        .call_code_mode(shell_call(json!({"cmd": "printf '%s' \"$HOME\""})))
        .await
        .unwrap();
    assert_eq!(inherited["exit_code"], 0);
    assert_eq!(inherited["output"], inherited_home);

    let overridden = test_tools(2)
        .with_env("HOME", "/explicit-home")
        .call_code_mode(shell_call(json!({"cmd": "printf '%s' \"$HOME\""})))
        .await
        .unwrap();
    assert_eq!(overridden["exit_code"], 0);
    assert_eq!(overridden["output"], "/explicit-home");
}

#[tokio::test]
async fn requested_workdir_does_not_change_the_process_or_other_commands_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let left = temp.path().join("left");
    let right = temp.path().join("right");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    let process_cwd = std::env::current_dir().unwrap();
    let tools = ShellTools::in_directory(
        Duration::from_secs(2),
        camino::Utf8PathBuf::try_from(left.clone()).unwrap(),
        PathOverrides::default(),
    );
    let moved = tools
        .call_code_mode(shell_call(json!({"cmd": "pwd", "workdir": right})))
        .await
        .unwrap();
    let default = tools
        .call_code_mode(shell_call(json!({"cmd": "pwd"})))
        .await
        .unwrap();
    assert_eq!(moved["exit_code"], 0);
    assert_eq!(moved["output"], format!("{}\n", right.display()));
    assert_eq!(default["exit_code"], 0);
    assert_eq!(default["output"], format!("{}\n", left.display()));
    assert_eq!(std::env::current_dir().unwrap(), process_cwd);
}

#[tokio::test]
async fn nonzero_exit_is_structured_result_not_tool_error() {
    let tools = test_tools(2);
    let result = tools
        .call(shell_call(json!({"command": "printf nope; exit 3"})))
        .await;

    assert_eq!(result.status, ToolOutputStatus::Success);
    assert!(
        result
            .output
            .as_ref()
            .contains("Process exited with code 3")
    );
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
    assert!(result.output.as_ref().contains("tokens truncated"));
    assert!(result.output.len() < MAX_OUTPUT_BYTES + 2048);
}

#[test]
fn specs_expose_unified_exec_and_apply_patch() {
    let tools = test_tools(2);
    let specs = tools.specs();

    assert_eq!(specs.len(), 3);
    assert_eq!(specs[0].name.as_str(), EXEC_COMMAND_TOOL_NAME);
    assert_eq!(specs[1].name.as_str(), WRITE_STDIN_TOOL_NAME);
    assert_eq!(specs[2].name.as_str(), APPLY_PATCH_TOOL_NAME);
    assert_eq!(specs[2].tool_type, ToolType::Custom);
    assert!(matches!(specs[2].format, Some(ToolFormat::Grammar { .. })));
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

#[test]
fn interpreter_cwd_is_private_to_its_thread() {
    let temp = tempfile::tempdir().unwrap();
    let process_cwd = std::env::current_dir().unwrap();
    let target = temp.path().to_owned();
    let tools = ShellTools::in_directory(
        Duration::from_secs(2),
        camino::Utf8PathBuf::try_from(target.clone()).unwrap(),
        PathOverrides::default(),
    );
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            unsafe { tools.enter_interpreter_thread().await }.unwrap();
            assert_eq!(std::env::current_dir().unwrap(), target);
        });
    })
    .join()
    .unwrap();
    assert_eq!(std::env::current_dir().unwrap(), process_cwd);
}

#[tokio::test]
async fn patch_paths_are_absolute_or_relative_to_the_tools_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    std::fs::create_dir(&root).unwrap();
    let tools = ShellTools::in_directory(
        Duration::from_secs(2),
        camino::Utf8PathBuf::try_from(root.clone()).unwrap(),
        PathOverrides::default(),
    );
    let absolute = temp.path().join("absolute.txt");
    let patch = format!(
        "*** Begin Patch\n*** Add File: relative.txt\n+relative\n*** Add File: {}\n+absolute\n*** End Patch",
        absolute.display()
    );
    let result = tools.call(patch_call(patch)).await;
    assert_eq!(
        result.status,
        ToolOutputStatus::Success,
        "{}",
        result.output
    );
    assert_eq!(
        std::fs::read_to_string(root.join("relative.txt")).unwrap(),
        "relative\n"
    );
    assert_eq!(std::fs::read_to_string(absolute).unwrap(), "absolute\n");
    assert!(!temp.path().join("relative.txt").exists());
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
