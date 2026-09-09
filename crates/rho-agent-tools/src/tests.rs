use std::sync::Arc;
use std::time::Duration;

use rho_core::{ToolCall, ToolCallId, ToolName, ToolOutputStatus, ToolType};
use rho_tool_shell::{EXEC_COMMAND_TOOL_NAME, ShellTools, WRITE_STDIN_TOOL_NAME};
use rho_workspaces::PathOverrides;
use serde_json::json;
use tokio::sync::Notify;

use crate::{CodeModeTool, ShellTool, SourceWaker, Tool, ToolHaste, ToolSession, tools};

fn shell() -> ShellTools {
    ShellTools::in_directory(
        Duration::from_secs(5),
        "/tmp".into(),
        PathOverrides::default(),
    )
}

#[tokio::test]
async fn javascript_runtime_remains_selectable() {
    let tools = tools(shell(), Vec::new(), Some(crate::CodeMode::JavaScript)).unwrap();
    let wake = Arc::new(Notify::new());
    let mut cell = tools[0].run(
        call("js", "exec", json!("const answer = 6 * 7; text(answer);")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, ended).await;
    let output = cell.first_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("42"), "{output:?}");
}

#[tokio::test]
async fn python_tool_entries_use_the_callable_namespace() {
    let tool = crate::PythonTool::new(shell(), Vec::new()).unwrap();
    let description = tool.spec().description;
    assert!(!description.contains("apply_patch"));
    assert!(description.contains("web.run(search_query="));
    assert!(description.contains("same cell to run them concurrently"));
    assert!(description.contains("handle.cancel() requests cancellation"));
    assert!(description.contains("job.cancel()"));
    assert!(description.contains("await job"));
}

#[tokio::test]
async fn python_independent_commands_in_one_cell_run_concurrently() {
    let directory = tempfile::tempdir().unwrap();
    let tools = tools(
        ShellTools::in_directory(
            Duration::from_secs(5),
            directory.path().to_str().unwrap().into(),
            PathOverrides::default(),
        ),
        Vec::new(),
        Some(crate::CodeMode::Python),
    )
    .unwrap();
    let wake = Arc::new(Notify::new());
    // The first command cannot finish until the second starts. Serial execution
    // would deadlock, unlike merely placing sequential commands in one shell.
    let mut cell = tools[0].run(call("parallel", "exec", json!(
        "command('while [ ! -f ready ]; do sleep 0.01; done; echo first')\ncommand('touch ready; echo second')"
    )), SourceWaker::new(wake.clone()));
    until(&wake, &*cell, ended).await;
    let output = cell.first_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("first"), "{output:?}");
    assert!(output.output.contains("second"), "{output:?}");
    assert_eq!(output.output.matches("Process exited with code").count(), 2);
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
        let haste = test_haste(session);
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

// Legacy test predicates; production Python reports only Python-specific facts.
fn test_haste(session: &dyn ToolSession) -> ToolHaste {
    if let Some(exec) = session.python_exec() {
        let quiescent = exec.quiescent();
        let facts = exec.facts();
        if quiescent {
            return ToolHaste::Ended {
                at: facts.returned.unwrap(),
            };
        }
        if let Some(since) = facts.output.notification {
            return ToolHaste::Soon { since };
        }
        return facts
            .output
            .since
            .map_or(ToolHaste::None, |since| ToolHaste::Eventually { since });
    }
    match session.sources()[0].1 {
        crate::SourceFacts::Tool(haste) => haste,
        _ => unreachable!(),
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
    assert_eq!(
        test_haste(&*session),
        ToolHaste::None,
        "silent until it ends"
    );
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
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CodeModeTool::new(shell(), Vec::new()).unwrap())];
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
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CodeModeTool::new(shell(), Vec::new()).unwrap())];
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
    assert_eq!(test_haste(&*session), ToolHaste::None, "drained");

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
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CodeModeTool::new(shell(), Vec::new()).unwrap())];
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

#[tokio::test]
async fn python_commands_outlive_cells_and_retain_truncated_output() {
    let tools = tools(shell(), Vec::new(), Some(crate::CodeMode::Python)).unwrap();
    let wake = Arc::new(Notify::new());
    let mut cell = tools[0].run(
        call(
            "p1",
            "exec",
            json!("job = command(\"sleep 0.1; printf abcdefghijklmnopqrstuvwxyz\", max_tokens=1)"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, ended).await;
    let result = cell.first_output();
    assert!(
        result.output.contains("Process exited with code"),
        "{}",
        result.output
    );
    assert!(!result.output.contains("\nabcdefghijklmnopqrstuvwxyz"));
    assert!(!result.output.contains("Session ID:"));
    assert!(!result.output.contains("session ID"));
    assert!(!result.output.contains("Command:"));
    assert!(!result.output.contains("Retained"));
    assert!(!result.output.contains("beyond retention limit"));
    assert!(cell.done());
    drop(cell);
    let mut read = tools[0].run(
        call("p2", "exec", json!("write_stdin(job, max_tokens=100)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*read, ended).await;
    assert!(
        read.first_output()
            .output
            .contains("abcdefghijklmnopqrstuvwxyz")
    );
}

#[tokio::test]
async fn python_monitor_remains_inspectable_and_notifies_on_its_original_call() {
    let directory = tempfile::tempdir().unwrap();
    let tools = tools(
        ShellTools::in_directory(
            Duration::from_secs(5),
            directory.path().to_str().unwrap().into(),
            PathOverrides::default(),
        ),
        Vec::new(),
        Some(crate::CodeMode::Python),
    )
    .unwrap();
    let wake = Arc::new(Notify::new());
    let mut monitor = tools[0].run(call("monitor", "exec", json!(
        "progress = {'checks': 0}\ntext('monitor started')\nwhile not Path('results.json').exists():\n    progress['checks'] += 1\n    await asyncio.sleep(0.01)\nnotify(Path('results.json').read_text())"
    )), SourceWaker::new(wake.clone()));
    until(&wake, &*monitor, |h| {
        matches!(h, ToolHaste::Eventually { .. })
    })
    .await;
    assert_eq!(monitor.first_output().output.trim(), "monitor started");
    assert!(!monitor.done());

    let mut inspect = tools[0].run(
        call(
            "inspect",
            "exec",
            json!("assert progress['checks'] > 0\ntext(progress)"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*inspect, ended).await;
    let output = inspect.first_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("checks"));
    assert!(!monitor.done());

    let mut idle = tools[0].run(
        call("idle", "exec", json!("set_patience(300)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*idle, ended).await;
    assert_eq!(
        idle.python_exec().unwrap().facts().patience,
        Some(Duration::from_secs(300))
    );
    assert!(
        idle.python_exec()
            .unwrap()
            .facts()
            .completion
            .unwrap()
            .set_patience
    );
    idle.first_output();

    // No assignment or await: Rust owns the command and its source attachment.
    let mut writer = tools[0].run(
        call(
            "writer",
            "exec",
            json!("command(\"printf 'ready' > results.json\")"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*writer, ended).await;
    let output = writer.first_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("Process exited with code"));
    until(&wake, &*monitor, ended).await;
    let output = monitor.more_output().unwrap();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("ready"));
    assert!(!output.output.contains("Process exited with code"));
    assert!(monitor.done());
}

#[tokio::test]
async fn python_immediate_stdin_and_patience_controls() {
    let tools = tools(shell(), Vec::new(), Some(crate::CodeMode::Python)).unwrap();
    let wake = Arc::new(Notify::new());
    let mut cell = tools[0].run(
        call(
            "p1",
            "exec",
            json!("job = command('read line; echo $line')\nwrite_stdin(job, 'hello\\n')"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, ended).await;
    let result = cell.first_output();
    assert_eq!(
        result.status,
        ToolOutputStatus::Success,
        "{}",
        result.output
    );
    assert!(result.output.contains("hello"), "{}", result.output);
    let mut control = tools[0].run(
        call("p2", "exec", json!("set_patience(seconds=300)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*control, ended).await;
    assert_eq!(
        control.python_exec().unwrap().facts().patience,
        Some(Duration::from_secs(300))
    );
    assert!(
        control
            .python_exec()
            .unwrap()
            .facts()
            .completion
            .unwrap()
            .set_patience
    );
    assert_eq!(control.first_output().status, ToolOutputStatus::Success);
}

#[tokio::test]
async fn python_asyncio_timeout_does_not_own_the_managed_command() {
    let tools = tools(shell(), Vec::new(), Some(crate::CodeMode::Python)).unwrap();
    let wake = Arc::new(Notify::new());
    let mut cell = tools[0].run(call("timeout", "exec", json!(
        "job = command('sleep 0.1; echo completed')\ntry:\n    await asyncio.wait_for(job, 0.01)\nexcept TimeoutError:\n    pass\nelse:\n    raise AssertionError('expected timeout')\nassert (await job)['exit_code'] == 0"
    )), SourceWaker::new(wake.clone()));
    until(&wake, &*cell, ended).await;
    let output = cell.first_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("completed"), "{output:?}");
}

#[tokio::test]
async fn only_python_omits_apply_patch() {
    let python = tools(shell(), Vec::new(), Some(crate::CodeMode::Python)).unwrap();
    assert!(!python[0].spec().description.contains("apply_patch"));
    let javascript = tools(shell(), Vec::new(), Some(crate::CodeMode::JavaScript)).unwrap();
    assert!(javascript[0].spec().description.contains("apply_patch"));
    assert!(
        tools(shell(), Vec::new(), None)
            .unwrap()
            .iter()
            .any(|tool| tool.spec().name.as_str() == "apply_patch")
    );
}

#[tokio::test]
async fn python_nested_tools_deliver_unawaited_output_and_keep_native_results() {
    struct Echo;
    impl crate::FutureTool for Echo {
        fn spec(&self) -> rho_core::ToolSpec {
            let mut spec = PendingTool(Default::default()).spec();
            spec.name = ToolName::try_from("echo").unwrap();
            spec.tool_type = ToolType::Custom;
            spec
        }
        fn call(
            &self,
            call: ToolCall,
        ) -> futures::future::BoxFuture<'static, rho_core::ToolOutput> {
            Box::pin(async move { crate::output(call.arguments, ToolOutputStatus::Success) })
        }
    }
    let tools = tools(shell(), vec![Arc::new(Echo)], Some(crate::CodeMode::Python)).unwrap();
    let wake = Arc::new(Notify::new());
    let mut cell = tools[0].run(call("echo", "exec", json!(
        "echo('unawaited output')\nresult = await echo('awaited output')\nassert isinstance(result, str)\nassert result == 'awaited output'"
    )), SourceWaker::new(wake.clone()));
    until(&wake, &*cell, ended).await;
    let output = cell.first_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("unawaited output"), "{output:?}");
    assert!(output.output.contains("awaited output"), "{output:?}");
    assert!(!output.output.contains("Session ID:"));
    assert!(!output.output.contains("session ID"));
}

#[tokio::test]
async fn python_registration_settles_startup_failure_and_preserves_exit_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let tools = tools(
        ShellTools::in_directory(
            Duration::from_secs(5),
            directory.path().to_str().unwrap().into(),
            PathOverrides::default(),
        ),
        Vec::new(),
        Some(crate::CodeMode::Python),
    )
    .unwrap();
    let wake = Arc::new(Notify::new());
    let mut failed = tools[0].run(
        call(
            "startup",
            "exec",
            json!("job = command('true', workdir='missing-directory')\nwrite_stdin(job, 'hello')"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*failed, ended).await;
    let output = failed.first_output();
    assert!(output.output.contains("Command failed:"), "{output:?}");
    assert!(
        output.output.contains("Command stdin not ready or closed"),
        "{output:?}"
    );
    assert!(failed.done());

    let mut exit = tools[0].run(
        call(
            "exit",
            "exec",
            json!("result = await command('exit 7')\nassert result['exit_code'] == 7"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*exit, ended).await;
    assert_eq!(exit.first_output().status, ToolOutputStatus::Success);
}

#[tokio::test]
async fn python_registration_cancels_an_unawaited_command_and_blocked_stdin_together() {
    let tools = tools(shell(), Vec::new(), Some(crate::CodeMode::Python)).unwrap();
    let wake = Arc::new(Notify::new());
    let mut cell = tools[0].run(
        call(
            "blocked",
            "exec",
            json!("job = command('sleep 60')\nwrite_stdin(job, 'x' * 262144)\ntext('registered')"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, |h| matches!(h, ToolHaste::Eventually { .. })).await;
    assert!(cell.first_output().output.contains("registered"));
    cell.cancel();
    until(&wake, &*cell, ended).await;
    let output = cell.more_output().unwrap();
    assert_eq!(output.status, ToolOutputStatus::Cancelled, "{output:?}");
    assert!(output.output.contains("Command failed:"), "{output:?}");
    assert!(cell.done());
}

#[tokio::test]
async fn old_execution_keeps_its_own_patience_without_touching_new_execution() {
    let tools = tools(shell(), Vec::new(), Some(crate::CodeMode::Python)).unwrap();
    let wake = Arc::new(Notify::new());
    let mut old = tools[0].run(
        call(
            "old",
            "exec",
            json!("notify('waiting')\nawait asyncio.sleep(0.2)\nset_patience(3600)"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*old, |h| matches!(h, ToolHaste::Soon { .. })).await;
    old.first_output();
    let new = tools[0].run(
        call("new", "exec", json!("set_patience(300)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*new, ended).await;
    until(&wake, &*old, ended).await;
    assert_eq!(
        old.python_exec().unwrap().facts().patience,
        Some(Duration::from_secs(3600))
    );
    assert_eq!(
        new.python_exec().unwrap().facts().patience,
        Some(Duration::from_secs(300))
    );
    assert!(!old.more_output().unwrap().output.contains("ignored"));
}

struct PendingTool(Arc<std::sync::atomic::AtomicUsize>);
impl crate::FutureTool for PendingTool {
    fn spec(&self) -> rho_core::ToolSpec {
        rho_core::ToolSpec {
            name: ToolName::try_from("pending").unwrap(),
            tool_type: ToolType::Function,
            description: "test".into(),
            input_schema: json!({}),
            format: None,
        }
    }
    fn call(&self, _: ToolCall) -> futures::future::BoxFuture<'static, rho_core::ToolOutput> {
        struct Guard(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let count = self.0.clone();
        Box::pin(async move {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _guard = Guard(count);
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn python_cancellation_owns_pending_host_calls() {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tools = tools(
        shell(),
        vec![Arc::new(PendingTool(count.clone()))],
        Some(crate::CodeMode::Python),
    )
    .unwrap();
    let wake = Arc::new(Notify::new());
    let mut cell = tools[0].run(
        call("p1", "exec", json!("notify('starting'); await pending()")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, |h| matches!(h, ToolHaste::Soon { .. })).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cell.cancel();
    until(&wake, &*cell, ended).await;
    assert_eq!(cell.first_output().status, ToolOutputStatus::Cancelled);
    assert!(cell.done());
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);

    let mut second = tools[0].run(
        call("p2", "exec", json!("await pending()")),
        SourceWaker::new(wake.clone()),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tools);
    until(&wake, &*second, ended).await;
    second.first_output();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn python_retained_pages_do_not_split_unicode_characters() {
    let tools = tools(shell(), Vec::new(), Some(crate::CodeMode::Python)).unwrap();
    let wake = Arc::new(Notify::new());
    let mut cell = tools[0].run(call("p1", "exec", json!("job = command(\"printf '☃☃'\")\nawait job\na = await write_stdin(job, max_tokens=1)\nb = await write_stdin(job, max_tokens=1)\nassert a['output'] + b['output'] == '☃☃'")), SourceWaker::new(wake.clone()));
    until(&wake, &*cell, ended).await;
    let result = cell.first_output();
    assert_eq!(
        result.status,
        ToolOutputStatus::Success,
        "{}",
        result.output
    );
}

#[tokio::test(flavor = "current_thread")]
async fn python_host_state_is_committed_before_return_without_agent_polling() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("registered");
    let tools = tools(shell(), Vec::new(), Some(crate::CodeMode::Python)).unwrap();
    let wake = Arc::new(Notify::new());
    let source = format!(
        "set_patience(300)\ncommand('echo registered')\nweb.run(unknown=True)\nPath({:?}).touch()\nawait asyncio.sleep(0.1)",
        marker.to_str().unwrap()
    );
    let mut cell = tools[0].run(
        call("sync", "exec", json!(source)),
        SourceWaker::new(wake.clone()),
    );
    assert!(cell.python_exec().unwrap().facts().returned.is_none());
    // Don't let the Tokio executor run. Python's native callbacks must commit
    // registration and patience before the subsequent filesystem write.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let exec = cell.python_exec().unwrap();
    assert!(exec.facts().started);
    assert_eq!(exec.facts().patience, Some(Duration::from_secs(300)));
    assert_eq!(
        cell.sources().len(),
        3,
        "exec and two independent operations"
    );
    cell.cancel();
    until(&wake, &*cell, ended).await;
    cell.first_output();
    drop(cell);
    // State remains readable without a transcript session; no drained setter.
    assert_eq!(exec.facts().patience, Some(Duration::from_secs(300)));
}

#[tokio::test]
async fn top_level_return_does_not_finish_detached_python_activity() {
    let directory = tempfile::tempdir().unwrap();
    let release = directory.path().join("release");
    let tools = tools(shell(), Vec::new(), Some(crate::CodeMode::Python)).unwrap();
    let wake = Arc::new(Notify::new());
    let source = format!(
        "async def monitor():\n    while not Path({:?}).exists():\n        await asyncio.sleep(0.01)\n    notify('detached finished')\nasyncio.create_task(monitor())",
        release.to_str().unwrap()
    );
    let mut cell = tools[0].run(
        call("detached", "exec", json!(source)),
        SourceWaker::new(wake.clone()),
    );
    let exec = cell.python_exec().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while exec.facts().returned.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert!(!exec.quiescent());
    assert_eq!(
        cell.first_output().output.as_str(),
        "No output yet. Output and completion arrive automatically."
    );
    assert!(!cell.done());
    std::fs::write(release, "").unwrap();
    until(&wake, &*cell, ended).await;
    assert!(
        cell.more_output()
            .unwrap()
            .output
            .contains("detached finished")
    );
    assert!(cell.done());
}

#[tokio::test]
async fn operation_output_does_not_retroactively_change_exec_return_facts() {
    struct ReleasedTool(Arc<Notify>);
    impl crate::FutureTool for ReleasedTool {
        fn spec(&self) -> rho_core::ToolSpec {
            let mut spec = PendingTool(Default::default()).spec();
            spec.name = ToolName::try_from("released").unwrap();
            spec
        }
        fn call(&self, _: ToolCall) -> futures::future::BoxFuture<'static, rho_core::ToolOutput> {
            let release = self.0.clone();
            Box::pin(async move {
                release.notified().await;
                crate::output("nested result", ToolOutputStatus::Success)
            })
        }
    }
    let release = Arc::new(Notify::new());
    let tools = tools(
        shell(),
        vec![
            Arc::new(ReleasedTool(release.clone())),
            Arc::new(PendingTool(Default::default())),
        ],
        Some(crate::CodeMode::Python),
    )
    .unwrap();
    let wake = Arc::new(Notify::new());
    let mut cell = tools[0].run(
        call("return-facts", "exec", json!("released()\npending()")),
        SourceWaker::new(wake.clone()),
    );
    let exec = cell.python_exec().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while exec.facts().returned.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    let completion = exec.facts().completion.unwrap();
    assert!(completion.dispatched);
    assert!(!completion.produced_output);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !cell.sources().iter().any(|(_, facts)| {
            matches!(
                facts,
                crate::SourceFacts::PythonOperation(crate::PythonOperationFacts {
                    finished: Some(_),
                    ..
                })
            )
        }) {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(exec.facts().completion, Some(completion));
    assert!(exec.facts().output.since.is_none());
    assert!(cell.first_output().output.contains("nested result"));
    assert!(exec.facts().completion.is_none());
    cell.cancel();
    until(&wake, &*cell, ended).await;
    cell.more_output();
    assert!(
        exec.facts().completion.is_none(),
        "quiescence is not a second top-level return"
    );
}

#[tokio::test]
async fn python_agents_api_exposes_docs_and_runs_advisor_without_await() {
    struct EchoTool(rho_core::ToolSpec);
    impl crate::FutureTool for EchoTool {
        fn spec(&self) -> rho_core::ToolSpec {
            self.0.clone()
        }
        fn call(
            &self,
            call: ToolCall,
        ) -> futures::future::BoxFuture<'static, rho_core::ToolOutput> {
            Box::pin(async move {
                crate::output(
                    format!("{}:{}", call.name.as_str(), call.arguments),
                    ToolOutputStatus::Success,
                )
            })
        }
    }
    let others = [
        "spawn_engineer",
        "interrupt_engineer",
        "message_agent",
        "ask_advisor",
        "web__run",
        "view_image",
    ]
    .into_iter()
    .map(|name| {
        let mut spec = crate::FutureTool::spec(&PendingTool(Default::default()));
        spec.name = ToolName::try_from(name).unwrap();
        spec.description = format!("{name} full documentation");
        Arc::new(EchoTool(spec)) as Arc<dyn crate::FutureTool>
    })
    .collect();
    let tool = crate::PythonTool::new(shell(), others).unwrap();
    let description = tool.spec().description;
    assert!(description.contains("display(agents.delegate_engineer)"));
    assert!(description.contains("agents.spawn_new_advisor: ask_advisor full documentation"));
    assert!(!description.contains("tools.ask_advisor"));
    assert!(!description.contains("spawn_engineer full documentation"));
    assert!(!description.contains("encoding="));
    assert!(description.contains("web.run: standard OpenAI web run"));
    assert!(!description.contains("web__run full documentation"));
    let wake = Arc::new(Notify::new());
    let mut cell = tool.run(
        call(
            "agents-api",
            "exec",
            json!(
                r#"
import types
assert isinstance(agents, types.ModuleType)
docs = display(agents.delegate_engineer)
assert "spawn_engineer full documentation" in docs
assert "agents.delegate_engineer(" in docs
assert "task_name" in docs and "prompt" in docs
assert "tools" not in globals()
assert "spawn_engineer" not in globals()
assert "ask_advisor" not in globals()
assert "message_agent" not in globals()
assert "interrupt_engineer" not in globals()
result = await agents.delegate_engineer(task_name="test", prompt="work")
assert result.startswith("spawn_engineer:")
assert (await agents.message(agent_id="eng-test", message="hello")).startswith("message_agent:")
agents.cancel(engineer_id="eng-test")
agents.spawn_new_advisor("background review")
assert (await web.run(search_query=[])).startswith("web__run:")
assert (await view_image(path="test.png")).startswith("view_image:")
"#
            ),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, ended).await;
    let output = cell.first_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("background review"), "{output:?}");
    assert!(output.output.contains("interrupt_engineer:"), "{output:?}");
}

#[tokio::test]
async fn python_announces_sources_once_in_registration_order_including_late_sources() {
    let directory = tempfile::tempdir().unwrap();
    let release = directory.path().join("release");
    let registered = directory.path().join("registered");
    let tools = tools(
        shell(),
        vec![Arc::new(PendingTool(Default::default()))],
        Some(crate::CodeMode::Python),
    )
    .unwrap();
    let wake = Arc::new(Notify::new());
    let source = format!(
        "command('sleep 600')\npending()\ncommand('sleep 601')\nasync def later():\n    while not Path({:?}).exists():\n        await asyncio.sleep(0.01)\n    pending()\n    Path({:?}).touch()\nasyncio.create_task(later())",
        release.to_str().unwrap(),
        registered.to_str().unwrap()
    );
    let mut cell = tools[0].run(
        call("source-order", "exec", json!(source)),
        SourceWaker::new(wake.clone()),
    );
    let exec = cell.python_exec().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while exec.facts().returned.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    let first = cell.first_output().output;
    let lines = first.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "{first}");
    assert!(lines[0].starts_with("Command running with session ID "));
    assert!(lines[1].starts_with("Operation pending running with session ID "));
    assert!(lines[2].starts_with("Command running with session ID "));
    let mut ids = lines
        .iter()
        .map(|line| line.rsplit(' ').next().unwrap().parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.iter().all(|id| (1_000..10_000).contains(id)));
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );
    assert!(cell.more_output().is_none());
    std::fs::write(release, "").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !registered.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let late = cell.more_output().unwrap().output;
    assert!(late.starts_with("Operation pending running with session ID "));
    let late_id = late.rsplit(' ').next().unwrap().parse::<u32>().unwrap();
    assert!((1_000..10_000).contains(&late_id));
    assert!(!ids.contains(&late_id));
    ids.push(late_id);
    assert!(cell.more_output().is_none());
    cell.cancel();
    until(&wake, &*cell, ended).await;
    let finished = cell.more_output().unwrap().output;
    let positions = ids
        .iter()
        .map(|id| finished.find(&format!("Session ID: {id}")).unwrap())
        .collect::<Vec<_>>();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{finished}"
    );
}
