use std::sync::Arc;
use std::time::Duration;

use rho_core::{ToolCall, ToolCallId, ToolName, ToolOutputStatus, ToolType};
use rho_tool_shell::{EXEC_COMMAND_TOOL_NAME, ShellTools, WRITE_STDIN_TOOL_NAME};
use rho_workspaces::PathOverrides;
use serde_json::json;
use tokio::sync::Notify;

use crate::{ShellTool, SourceWaker, Tool, ToolHaste, ToolSession, tools};

fn shell() -> ShellTools {
    ShellTools::in_directory(
        Duration::from_secs(5),
        "/tmp".into(),
        PathOverrides::default(),
    )
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id).unwrap(),
        name: ToolName::try_from(name).unwrap(),
        tool_type: ToolType::Function,
        arguments: match arguments {
            serde_json::Value::String(source) => source,
            other => other.to_string(),
        },
    }
}

fn by_name<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> &'a Arc<dyn Tool> {
    tools
        .iter()
        .find(|tool| tool.spec().name.as_str() == name)
        .unwrap()
}

/// Waits, as the core would, for the session to report what `want` asks.
async fn until(
    wake: &Arc<Notify>,
    session: &dyn ToolSession,
    mut want: impl FnMut(ToolHaste) -> bool,
) -> ToolHaste {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let haste = session.haste();
        if want(haste) {
            return haste;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out at {haste:?}"
        );
        let _ = tokio::time::timeout(Duration::from_millis(200), wake.notified()).await;
    }
}

fn ended(haste: ToolHaste) -> bool {
    matches!(haste, ToolHaste::Ended { .. })
}

#[tokio::test]
async fn a_finished_command_is_answered_whole() {
    let tools = ShellTool::all(shell());
    let wake = Arc::new(Notify::new());
    let mut session = by_name(&tools, EXEC_COMMAND_TOOL_NAME).run(
        call(
            "c1",
            EXEC_COMMAND_TOOL_NAME,
            json!({"cmd": "sleep 0.2; echo hi; exit 3"}),
        ),
        SourceWaker::new(Arc::clone(&wake)),
    );
    assert_eq!(session.haste(), ToolHaste::None, "silent until it ends");
    until(&wake, &*session, ended).await;
    let output = session.first_output();
    assert!(
        output.output.contains("Process exited with code 3"),
        "{}",
        output.output
    );
    assert!(output.output.contains("Output:\nhi"), "{}", output.output);
    assert!(!output.output.contains("session ID"));
    assert!(session.done());
}

#[tokio::test]
async fn a_running_command_gets_a_session_and_its_later_output_arrives_as_updates() {
    let tools = ShellTool::all(shell());
    let wake = Arc::new(Notify::new());
    let mut session = by_name(&tools, EXEC_COMMAND_TOOL_NAME).run(
        call(
            "c1",
            EXEC_COMMAND_TOOL_NAME,
            json!({"cmd": "echo start; sleep 0.6; echo later"}),
        ),
        SourceWaker::new(Arc::clone(&wake)),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The core answers the call now because something else made a request.
    let first = session.first_output();
    assert!(
        first.output.contains("Process running with session ID 1"),
        "{}",
        first.output
    );
    assert!(first.output.contains("Output:\nstart"), "{}", first.output);
    assert!(!session.done());
    assert!(session.more_output().is_none(), "nothing new yet");

    until(&wake, &*session, ended).await;
    let update = session.more_output().unwrap();
    assert!(
        update.output.contains("Process exited with code 0"),
        "{}",
        update.output
    );
    assert!(update.output.contains("later"), "{}", update.output);
    assert!(session.done());
    assert!(session.more_output().is_none());
}

#[tokio::test]
async fn write_stdin_types_into_a_session_and_the_reply_lands_on_the_exec_call() {
    let tools = ShellTool::all(shell());
    let wake = Arc::new(Notify::new());
    let mut exec = by_name(&tools, EXEC_COMMAND_TOOL_NAME).run(
        call(
            "c1",
            EXEC_COMMAND_TOOL_NAME,
            json!({"cmd": "read line; echo got:$line"}),
        ),
        SourceWaker::new(Arc::clone(&wake)),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let first = exec.first_output();
    assert!(first.output.contains("session ID 1"), "{}", first.output);

    let mut write = by_name(&tools, WRITE_STDIN_TOOL_NAME).run(
        call(
            "c2",
            WRITE_STDIN_TOOL_NAME,
            json!({"session_id": 1, "chars": "hello\n"}),
        ),
        SourceWaker::new(Arc::clone(&wake)),
    );
    until(&wake, &*write, ended).await;
    let wrote = write.first_output();
    assert_eq!(wrote.status, ToolOutputStatus::Success, "{}", wrote.output);
    assert!(wrote.output.contains("Wrote 6 bytes"), "{}", wrote.output);

    until(&wake, &*exec, ended).await;
    let update = exec.more_output().unwrap();
    assert!(update.output.contains("got:hello"), "{}", update.output);

    let mut stale = by_name(&tools, WRITE_STDIN_TOOL_NAME).run(
        call(
            "c3",
            WRITE_STDIN_TOOL_NAME,
            json!({"session_id": 1, "chars": "x"}),
        ),
        SourceWaker::new(Arc::clone(&wake)),
    );
    assert_eq!(
        stale.first_output().status,
        ToolOutputStatus::Error,
        "session is gone"
    );
}

#[tokio::test]
async fn cancel_kills_the_process_and_says_so() {
    let tools = ShellTool::all(shell());
    let wake = Arc::new(Notify::new());
    let mut session = by_name(&tools, EXEC_COMMAND_TOOL_NAME).run(
        call("c1", EXEC_COMMAND_TOOL_NAME, json!({"cmd": "sleep 30"})),
        SourceWaker::new(Arc::clone(&wake)),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    session.cancel();
    until(&wake, &*session, ended).await;
    let output = session.first_output();
    assert!(
        output.output.contains("Process terminated"),
        "{}",
        output.output
    );
    assert!(session.done());
}

#[tokio::test]
async fn a_crash_line_makes_the_output_stand_on_its_own() {
    let tools = ShellTool::all(shell());
    let wake = Arc::new(Notify::new());
    let session = by_name(&tools, EXEC_COMMAND_TOOL_NAME).run(
        call(
            "c1",
            EXEC_COMMAND_TOOL_NAME,
            json!({"cmd": "echo 'thread main panicked at src/x.rs'; sleep 5"}),
        ),
        SourceWaker::new(Arc::clone(&wake)),
    );
    let haste = until(&wake, &*session, |haste| haste != ToolHaste::None).await;
    assert!(matches!(haste, ToolHaste::Soon { .. }), "{haste:?}");
}

#[tokio::test]
async fn a_script_runs_nested_tools_and_ends_with_its_output() {
    let tools = tools(shell(), Vec::new(), true).unwrap();
    assert_eq!(tools.len(), 1);
    let wake = Arc::new(Notify::new());
    let mut session = tools[0].run(
        call(
            "c1",
            "exec",
            json!("text('a'); const r = await tools.exec_command({cmd: 'echo hi'}); text(r.output.trim()); text(String(r.exit_code));"),
        ),
        SourceWaker::new(Arc::clone(&wake)),
    );
    until(&wake, &*session, ended).await;
    let output = session.first_output();
    assert_eq!(
        output.status,
        ToolOutputStatus::Success,
        "{}",
        output.output
    );
    assert!(
        output.output.starts_with("Script completed"),
        "{}",
        output.output
    );
    assert!(
        output.output.contains("Output:\na\nhi\n0"),
        "{}",
        output.output
    );
    assert!(session.done());
}

#[tokio::test]
async fn a_script_notify_is_urgent_and_its_end_arrives_as_an_update() {
    let tools = tools(shell(), Vec::new(), true).unwrap();
    let wake = Arc::new(Notify::new());
    let mut session = tools[0].run(
        call(
            "c1",
            "exec",
            json!("notify('halfway'); await new Promise(r => setTimeout(r, 700)); text('x'.repeat(50000));"),
        ),
        SourceWaker::new(Arc::clone(&wake)),
    );
    let haste = until(&wake, &*session, |haste| haste != ToolHaste::None).await;
    assert!(matches!(haste, ToolHaste::Soon { .. }), "{haste:?}");
    let first = session.first_output();
    assert!(
        first.output.contains("Script running with cell ID"),
        "{}",
        first.output
    );
    assert!(first.output.contains("halfway"), "{}", first.output);
    assert_eq!(session.haste(), ToolHaste::None, "drained");

    until(&wake, &*session, ended).await;
    let update = session.more_output().unwrap();
    assert!(
        update.output.starts_with("Script completed"),
        "{}",
        update.output
    );
    assert!(update.output.contains("truncated"), "{}", update.output);
    assert_eq!(update.recorded_output(), "x".repeat(50_000));
    assert!(session.done());
}

#[tokio::test]
async fn a_script_that_fails_says_so() {
    let tools = tools(shell(), Vec::new(), true).unwrap();
    let wake = Arc::new(Notify::new());
    let mut session = tools[0].run(
        call("c1", "exec", json!("throw new Error('boom')")),
        SourceWaker::new(Arc::clone(&wake)),
    );
    until(&wake, &*session, ended).await;
    let output = session.first_output();
    assert_eq!(output.status, ToolOutputStatus::Error);
    assert!(output.output.contains("boom"), "{}", output.output);
}
