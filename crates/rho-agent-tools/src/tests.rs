use std::sync::Arc;
use std::time::Duration;

use rho_core::{ExecCall, ToolCall, ToolCallId, ToolName, ToolOutputStatus, ToolType};
use rho_fs_view::PathOverrides;
use rho_tool_shell::ShellTools;
use serde_json::json;
use tokio::sync::Notify;

use crate::{JobFacts, PythonCell, PythonNotebook, SourceFacts, SourceWaker};

fn shell() -> ShellTools {
    ShellTools::in_directory(
        Duration::from_secs(5),
        "/tmp".into(),
        PathOverrides::default(),
    )
}

fn shell_in(directory: &tempfile::TempDir) -> ShellTools {
    ShellTools::in_directory(
        Duration::from_secs(5),
        directory.path().to_str().unwrap().into(),
        PathOverrides::default(),
    )
}

fn python(shell: ShellTools, others: Vec<Arc<dyn crate::FutureTool>>) -> Arc<PythonNotebook> {
    Arc::new(PythonNotebook::new(shell, others).unwrap())
}

fn call(id: &str, arguments: serde_json::Value) -> ExecCall {
    ExecCall {
        id: ToolCallId::try_from(id).unwrap(),
        source: match arguments {
            serde_json::Value::String(source) => source,
            other => other.to_string(),
        },
    }
}

/// What a cell's own facts say, in the order the scheduler cares about them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    None,
    Output,
    Notified,
    Ended,
}

fn signal(session: &PythonCell) -> Signal {
    let exec = session.execution();
    if exec.quiescent() {
        return Signal::Ended;
    }
    let facts = exec.facts();
    if facts.notified_at.is_some() {
        Signal::Notified
    } else if facts.output_since.is_some() {
        Signal::Output
    } else {
        Signal::None
    }
}

/// Waits, as the core would, for the session to report what `want` asks.
async fn until(wake: &Arc<Notify>, session: &PythonCell, want: Signal) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let now = signal(session);
        if now == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out at {now:?} waiting for {want:?}"
        );
        let _ = tokio::time::timeout(Duration::from_millis(200), wake.notified()).await;
    }
}

fn jobs(session: &PythonCell) -> Vec<JobFacts> {
    session
        .sources()
        .into_iter()
        .filter_map(|(_, facts)| match facts {
            SourceFacts::Job(job) => Some(job),
            SourceFacts::Cell(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn python_tool_entries_use_the_callable_namespace() {
    let description = crate::python_instructions(&[]);
    assert!(!description.contains("apply_patch"));
    assert!(description.contains("session IDs from 1000 through 9999"));
    assert!(description.contains("web.run(search_query="));
    assert!(description.contains("same cell to run them concurrently"));
    assert!(description.contains("handle.cancel() requests cancellation"));
    assert!(description.contains("job.cancel()"));
    assert!(description.contains("await job"));
}

#[tokio::test]
async fn python_independent_commands_in_one_cell_run_concurrently() {
    let directory = tempfile::tempdir().unwrap();
    let tool = python(shell_in(&directory), Vec::new());
    let wake = Arc::new(Notify::new());
    // The first command cannot finish until the second starts. Serial execution
    // would deadlock, unlike merely placing sequential commands in one shell.
    let mut cell = tool.exec(call("parallel", json!(
        "command('while [ ! -f ready ]; do sleep 0.01; done; echo first')\ncommand('touch ready; echo second')"
    )), SourceWaker::new(wake.clone()));
    until(&wake, &*cell, Signal::Ended).await;
    let output = cell.first_output();
    cell.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("first"), "{output:?}");
    assert!(output.output.contains("second"), "{output:?}");
    assert_eq!(
        output.output.matches("Process exited with code 0").count(),
        2,
        "{output:?}"
    );
}

#[tokio::test]
async fn a_finished_job_reports_its_exit_code_and_output_without_an_id() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call("exit", json!("command('echo hi; exit 3')")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, Signal::Ended).await;
    let job = &jobs(&*cell)[0];
    assert_eq!(job.cell, cell.execution().facts().cell);
    assert!(job.finished.unwrap().failed, "exit 3 is a failure");
    let output = cell.first_output();
    cell.acknowledge_output();
    // The job failed; the cell did not.
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(
        output
            .output
            .contains("Process exited with code 3\nOutput:\nhi"),
        "{output:?}"
    );
    assert!(
        !output.output.contains("Session ID"),
        "it ended before any reply: {output:?}"
    );
    assert!(cell.done());
    assert!(jobs(&*cell).is_empty(), "delivered jobs are forgotten");
}

#[tokio::test]
async fn a_running_job_is_announced_once_and_its_end_names_the_same_id() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call("pieces", json!("command('echo a; sleep 0.5; echo b')")),
        SourceWaker::new(wake.clone()),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while jobs(&*cell)
        .first()
        .is_none_or(|job| job.output_since.is_none())
    {
        assert!(tokio::time::Instant::now() < deadline);
        let _ = tokio::time::timeout(Duration::from_millis(100), wake.notified()).await;
    }
    let first = cell.first_output().output;
    cell.acknowledge_output();
    let id = first
        .strip_prefix("Command running in background with session ID ")
        .and_then(|rest| rest.split('\n').next())
        .unwrap_or_else(|| panic!("{first}"))
        .to_owned();
    assert!((1_000..10_000).contains(&id.parse::<u32>().unwrap()));
    assert!(first.ends_with("\nOutput:\na\n"), "{first}");
    assert!(!first.contains("Command:"), "the cell is current: {first}");
    until(&wake, &*cell, Signal::Ended).await;
    let rest = cell.more_output().unwrap().output;
    cell.acknowledge_output();
    assert_eq!(
        rest.as_str(),
        format!("Session ID: {id}\nProcess exited with code 0\nOutput:\nb\n")
    );
}

#[tokio::test]
async fn a_raising_cell_is_a_failed_cell() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call("raise", json!("raise RuntimeError('boom')")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, Signal::Ended).await;
    let facts = cell.execution().facts();
    assert!(facts.failed);
    assert!(
        facts.notified_at.is_none(),
        "an error is not something the model asked to be told"
    );
    let output = cell.first_output();
    cell.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Error, "{output:?}");
    assert!(output.output.contains("boom"), "{output:?}");
}

#[tokio::test]
async fn only_notify_marks_a_cell_notified() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut plain = tool.exec(
        call(
            "plain",
            json!("text('just output')\nawait asyncio.sleep(0.3)"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*plain, Signal::Output).await;
    assert!(plain.execution().facts().notified_at.is_none());
    plain.first_output();
    plain.acknowledge_output();
    assert_eq!(signal(&*plain), Signal::None, "drained");
    until(&wake, &*plain, Signal::Ended).await;
    let mut loud = tool.exec(
        call("loud", json!("notify('look')\nawait asyncio.sleep(0.3)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*loud, Signal::Notified).await;
    assert!(loud.first_output().output.contains("look"));
    loud.acknowledge_output();
    assert_eq!(signal(&*loud), Signal::None, "drained");
}

#[tokio::test]
async fn the_foreground_moves_only_when_a_cell_registers_work() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut worker = tool.exec(
        call("worker", json!("command('sleep 0.2')")),
        SourceWaker::new(wake.clone()),
    );
    let worker_id = worker.execution().facts().cell;
    until(&wake, &*worker, Signal::Ended).await;
    worker.first_output();
    worker.acknowledge_output();
    let looker = tool.exec(
        call("looker", json!("text(1)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*looker, Signal::Ended).await;
    let looking = looker.execution().facts();
    assert!(looking.cell > worker_id);
    assert_eq!(
        looking.foreground_cell, worker_id,
        "a cell that only looks does not move the foreground"
    );
    let next = tool.exec(
        call("next", json!("command('true')")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*next, Signal::Ended).await;
    let facts = next.execution().facts();
    assert_eq!(facts.foreground_cell, facts.cell);
    assert_eq!(
        looker.execution().facts().foreground_cell,
        facts.cell,
        "the foreground is shared by every cell"
    );
}

#[tokio::test]
async fn python_commands_outlive_cells_and_retain_truncated_output() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call(
            "p1",
            json!("job = command(\"sleep 0.1; printf abcdefghijklmnopqrstuvwxyz\", max_tokens=1)"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, Signal::Ended).await;
    let result = cell.first_output();
    cell.acknowledge_output();
    assert!(
        result.output.contains("Process exited with code 0"),
        "{}",
        result.output
    );
    assert!(!result.output.contains("Session ID:"));
    assert!(!result.output.contains("Command:"));
    assert!(!result.output.contains("\nabcdefghijklmnopqrstuvwxyz"));
    assert!(!result.output.contains("Session ID"));
    assert!(!result.output.contains("session ID"));
    assert!(!result.output.contains("Retained"));
    assert!(!result.output.contains("beyond retention limit"));
    assert!(cell.done());
    drop(cell);
    let mut read = tool.exec(
        call("p2", json!("write_stdin(job, max_tokens=100)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*read, Signal::Ended).await;
    assert!(
        read.first_output()
            .output
            .contains("abcdefghijklmnopqrstuvwxyz")
    );
    read.acknowledge_output();
}

#[tokio::test]
async fn python_monitor_remains_inspectable_and_notifies_on_its_original_call() {
    let directory = tempfile::tempdir().unwrap();
    let tool = python(shell_in(&directory), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut monitor = tool.exec(call("monitor", json!(
        "progress = {'checks': 0}\ntext('monitor started')\nwhile not Path('results.json').exists():\n    progress['checks'] += 1\n    await asyncio.sleep(0.01)\nnotify(Path('results.json').read_text())"
    )), SourceWaker::new(wake.clone()));
    until(&wake, &*monitor, Signal::Output).await;
    assert_eq!(monitor.first_output().output.trim(), "monitor started");
    monitor.acknowledge_output();
    assert!(!monitor.done());

    let mut inspect = tool.exec(
        call(
            "inspect",
            json!("assert progress['checks'] > 0\ntext(progress)"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*inspect, Signal::Ended).await;
    let output = inspect.first_output();
    inspect.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("checks"));
    assert!(!monitor.done());

    let mut idle = tool.exec(
        call("idle", json!("set_checkin(300)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*idle, Signal::Ended).await;
    assert_eq!(
        idle.execution()
            .facts()
            .checkin
            .map(|checkin| checkin.after),
        Some(Duration::from_secs(300))
    );
    assert_eq!(idle.first_output().output.as_str(), "No output.");
    idle.acknowledge_output();

    // No assignment or await: Rust owns the command and its source attachment.
    let mut writer = tool.exec(
        call(
            "writer",
            json!("command(\"printf 'ready' > results.json\")"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*writer, Signal::Ended).await;
    let output = writer.first_output();
    writer.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(
        output.output.contains("Process exited with code 0"),
        "{output:?}"
    );
    until(&wake, &*monitor, Signal::Ended).await;
    let output = monitor.more_output().unwrap();
    monitor.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("ready"));
    assert!(!output.output.contains("Process exited"), "{output:?}");
    assert!(monitor.done());
}

#[tokio::test]
async fn python_immediate_stdin_and_checkin_controls() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call(
            "p1",
            json!("job = command('read line; echo $line')\nwrite_stdin(job, 'hello\\n')"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, Signal::Ended).await;
    let result = cell.first_output();
    cell.acknowledge_output();
    assert_eq!(
        result.status,
        ToolOutputStatus::Success,
        "{}",
        result.output
    );
    assert!(result.output.contains("hello"), "{}", result.output);
    let mut control = tool.exec(
        call("p2", json!("set_checkin(after_seconds=300)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*control, Signal::Ended).await;
    assert_eq!(
        control
            .execution()
            .facts()
            .checkin
            .map(|checkin| checkin.after),
        Some(Duration::from_secs(300))
    );
    assert_eq!(control.first_output().status, ToolOutputStatus::Success);
    control.acknowledge_output();
}

#[tokio::test]
async fn python_asyncio_timeout_does_not_own_the_managed_command() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(call("timeout", json!(
        "job = command('sleep 0.1; echo completed')\ntry:\n    await asyncio.wait_for(job, 0.01)\nexcept TimeoutError:\n    pass\nelse:\n    raise AssertionError('expected timeout')\nassert (await job)['exit_code'] == 0"
    )), SourceWaker::new(wake.clone()));
    until(&wake, &*cell, Signal::Ended).await;
    let output = cell.first_output();
    cell.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("completed"), "{output:?}");
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
    let tool = python(shell(), vec![Arc::new(Echo)]);
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(call("echo", json!(
        "echo('unawaited output')\nresult = await echo('awaited output')\nassert isinstance(result, str)\nassert result == 'awaited output'"
    )), SourceWaker::new(wake.clone()));
    until(&wake, &*cell, Signal::Ended).await;
    let output = cell.first_output();
    cell.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("unawaited output"), "{output:?}");
    assert!(output.output.contains("awaited output"), "{output:?}");
    assert!(!output.output.contains("Session ID"));
    assert!(!output.output.contains("session ID"));
}

#[tokio::test]
async fn python_registration_settles_startup_failure_and_preserves_exit_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let tool = python(shell_in(&directory), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut failed = tool.exec(
        call(
            "startup",
            json!("job = command('true', workdir='missing-directory')\nwrite_stdin(job, 'hello')"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*failed, Signal::Ended).await;
    assert!(
        jobs(&*failed)
            .iter()
            .all(|job| job.finished.unwrap().failed),
        "a spawn failure and a write into it both fail"
    );
    let output = failed.first_output();
    failed.acknowledge_output();
    assert!(output.output.contains("Command failed:"), "{output:?}");
    assert!(
        output.output.contains("Command stdin not ready or closed"),
        "{output:?}"
    );
    assert!(failed.done());

    let mut exit = tool.exec(
        call(
            "exit",
            json!("result = await command('exit 7')\nassert result['exit_code'] == 7"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*exit, Signal::Ended).await;
    let output = exit.first_output();
    exit.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Success);
    assert!(
        output.output.contains("Process exited with code 7"),
        "{output:?}"
    );
}

#[tokio::test]
async fn python_registration_cancels_an_unawaited_command_and_blocked_stdin_together() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call(
            "blocked",
            json!("job = command('sleep 60')\nwrite_stdin(job, 'x' * 262144)\ntext('registered')"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, Signal::Output).await;
    assert!(cell.first_output().output.contains("registered"));
    cell.acknowledge_output();
    cell.cancel();
    until(&wake, &*cell, Signal::Ended).await;
    let output = cell.more_output().unwrap();
    cell.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Cancelled, "{output:?}");
    assert!(output.output.contains("Command failed:"), "{output:?}");
    assert!(cell.done());
}

#[tokio::test]
async fn old_execution_keeps_its_own_checkin_without_touching_new_execution() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut old = tool.exec(
        call(
            "old",
            json!("notify('waiting')\nawait asyncio.sleep(0.2)\nset_checkin(3600, wake_on_tools=False)"),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*old, Signal::Notified).await;
    old.first_output();
    old.acknowledge_output();
    let new = tool.exec(
        call("new", json!("set_checkin(300)")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*new, Signal::Ended).await;
    until(&wake, &*old, Signal::Ended).await;
    let old_checkin = old.execution().facts().checkin.unwrap();
    let new_checkin = new.execution().facts().checkin.unwrap();
    assert_eq!(old_checkin.after, Duration::from_secs(3600));
    assert_eq!(new_checkin.after, Duration::from_secs(300));
    assert!(!old_checkin.wake_on_tools);
    assert!(new_checkin.wake_on_tools);
    assert!(
        old.more_output().is_none(),
        "a cell that ends with nothing to say sends nothing"
    );
    old.acknowledge_output();
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
    let tool = python(shell(), vec![Arc::new(PendingTool(count.clone()))]);
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call("p1", json!("notify('starting'); await pending()")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, Signal::Notified).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cell.cancel();
    until(&wake, &*cell, Signal::Ended).await;
    assert_eq!(cell.first_output().status, ToolOutputStatus::Cancelled);
    cell.acknowledge_output();
    assert!(cell.done());
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);

    let mut second = tool.exec(
        call("p2", json!("await pending()")),
        SourceWaker::new(wake.clone()),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tool);
    until(&wake, &*second, Signal::Ended).await;
    assert!(second.execution().facts().failed);
    second.first_output();
    second.acknowledge_output();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn python_retained_pages_do_not_split_unicode_characters() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(call("p1", json!("job = command(\"printf '☃☃'\")\nawait job\na = await write_stdin(job, max_tokens=1)\nb = await write_stdin(job, max_tokens=1)\nassert a['output'] + b['output'] == '☃☃'")), SourceWaker::new(wake.clone()));
    until(&wake, &*cell, Signal::Ended).await;
    let result = cell.first_output();
    cell.acknowledge_output();
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
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let source = format!(
        "set_checkin(300)\ncommand('echo registered')\nweb.run(unknown=True)\nPath({:?}).touch()\nawait asyncio.sleep(0.1)",
        marker.to_str().unwrap()
    );
    let mut cell = tool.exec(call("sync", json!(source)), SourceWaker::new(wake.clone()));
    assert!(cell.execution().facts().returned.is_none());
    // Don't let the Tokio executor run. Python's native callbacks must commit
    // registration and the check-in before the subsequent filesystem write.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let exec = cell.execution();
    assert!(exec.facts().started);
    assert_eq!(
        exec.facts().checkin.map(|checkin| checkin.after),
        Some(Duration::from_secs(300))
    );
    assert_eq!(
        cell.sources().len(),
        3,
        "exec and two independent operations"
    );
    assert_eq!(exec.facts().foreground_cell, exec.facts().cell);
    cell.cancel();
    until(&wake, &*cell, Signal::Ended).await;
    cell.first_output();
    cell.acknowledge_output();
    drop(cell);
    // State remains readable without a transcript session; no drained setter.
    assert_eq!(
        exec.facts().checkin.map(|checkin| checkin.after),
        Some(Duration::from_secs(300))
    );
}

#[tokio::test]
async fn top_level_return_does_not_finish_detached_python_activity() {
    let directory = tempfile::tempdir().unwrap();
    let release = directory.path().join("release");
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let source = format!(
        "async def monitor():\n    while not Path({:?}).exists():\n        await asyncio.sleep(0.01)\n    notify('detached finished')\nasyncio.create_task(monitor())",
        release.to_str().unwrap()
    );
    let mut cell = tool.exec(
        call("detached", json!(source)),
        SourceWaker::new(wake.clone()),
    );
    let exec = cell.execution();
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
    cell.acknowledge_output();
    assert!(!cell.done());
    std::fs::write(release, "").unwrap();
    until(&wake, &*cell, Signal::Ended).await;
    assert!(
        cell.more_output()
            .unwrap()
            .output
            .contains("detached finished")
    );
    cell.acknowledge_output();
    assert!(cell.done());
}

#[tokio::test]
async fn a_silent_cell_says_nothing_when_an_older_cell_speaks_in_the_same_reply() {
    let tool = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut older = tool.exec(
        call("older", json!("command('echo late')")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*older, Signal::Ended).await;
    // The older cell's finished job is unreported when the newer cell is
    // first answered, so "No output yet" would misdescribe the reply.
    let mut newer = tool.exec(
        call("newer", json!("await asyncio.sleep(0.2)")),
        SourceWaker::new(wake.clone()),
    );
    assert_eq!(newer.first_output().output.as_str(), "");
    newer.acknowledge_output();
    assert!(older.first_output().output.contains("late"));
    older.acknowledge_output();
    assert!(older.done());
    // With nothing older left to say, silence is the whole reply.
    let mut newest = tool.exec(
        call("newest", json!("await asyncio.sleep(0.2)")),
        SourceWaker::new(wake.clone()),
    );
    assert_eq!(
        newest.first_output().output.as_str(),
        "No output yet. Output and completion arrive automatically."
    );
    newest.acknowledge_output();
    until(&wake, &*newer, Signal::Ended).await;
    until(&wake, &*newest, Signal::Ended).await;
    assert!(newer.more_output().is_none());
    newer.acknowledge_output();
    assert!(newest.more_output().is_none());
    newest.acknowledge_output();
}

#[tokio::test]
async fn operation_output_does_not_change_exec_return_facts() {
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
    let tool = python(
        shell(),
        vec![
            Arc::new(ReleasedTool(release.clone())),
            Arc::new(PendingTool(Default::default())),
        ],
    );
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call("return-facts", json!("released()\npending()")),
        SourceWaker::new(wake.clone()),
    );
    let exec = cell.execution();
    tokio::time::timeout(Duration::from_secs(10), async {
        while exec.facts().returned.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    let returned = exec.facts();
    assert!(returned.output_since.is_none());
    assert!(!returned.failed);
    assert!(
        jobs(&*cell).iter().all(|job| job.finished.is_none()),
        "both operations are still running"
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !jobs(&*cell).iter().any(|job| job.finished.is_some()) {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    let finished = jobs(&*cell)
        .into_iter()
        .find(|job| job.finished.is_some())
        .unwrap();
    assert!(!finished.finished.unwrap().failed);
    assert_eq!(exec.facts(), returned, "an operation's end is its own fact");
    let output = cell.first_output().output;
    cell.acknowledge_output();
    assert!(
        output.contains("Operation released completed\nOutput:\nnested result"),
        "{output}"
    );
    cell.cancel();
    until(&wake, &*cell, Signal::Ended).await;
    let output = cell.more_output().unwrap().output;
    cell.acknowledge_output();
    assert!(output.contains("Operation pending completed"), "{output}");
    assert_eq!(exec.facts().returned, returned.returned);
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
    let others: Vec<Arc<dyn crate::FutureTool>> = [
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
    let specs = others.iter().map(|tool| tool.spec()).collect::<Vec<_>>();
    let tool = PythonNotebook::new(shell(), others).unwrap();
    let description = crate::python_instructions(&specs);
    assert!(description.contains("display(agents.delegate_engineer)"));
    assert!(description.contains("agents.spawn_new_advisor: ask_advisor full documentation"));
    assert!(!description.contains("tools.ask_advisor"));
    assert!(!description.contains("spawn_engineer full documentation"));
    assert!(!description.contains("encoding="));
    assert!(description.contains("web.run: standard OpenAI web run"));
    assert!(!description.contains("web__run full documentation"));
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call(
            "agents-api",
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
    until(&wake, &*cell, Signal::Ended).await;
    let output = cell.first_output();
    cell.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("background review"), "{output:?}");
    assert!(output.output.contains("interrupt_engineer:"), "{output:?}");
}

#[tokio::test]
async fn python_announces_sources_once_in_registration_order_including_late_sources() {
    let directory = tempfile::tempdir().unwrap();
    let release = directory.path().join("release");
    let registered = directory.path().join("registered");
    let tool = python(shell(), vec![Arc::new(PendingTool(Default::default()))]);
    let wake = Arc::new(Notify::new());
    let source = format!(
        "command('sleep 600')\npending()\ncommand('sleep 601')\nasync def later():\n    while not Path({:?}).exists():\n        await asyncio.sleep(0.01)\n    pending()\n    Path({:?}).touch()\nasyncio.create_task(later())",
        release.to_str().unwrap(),
        registered.to_str().unwrap()
    );
    let mut cell = tool.exec(
        call("source-order", json!(source)),
        SourceWaker::new(wake.clone()),
    );
    let exec = cell.execution();
    tokio::time::timeout(Duration::from_secs(10), async {
        while exec.facts().returned.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(jobs(&*cell).len(), 3);
    assert!(jobs(&*cell).iter().all(|job| job.finished.is_none()));
    let first = cell.first_output().output;
    cell.acknowledge_output();
    let lines = first.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "{first}");
    assert!(lines[0].starts_with("Command running in background with session ID "));
    assert!(lines[1].starts_with("Operation pending running in background with session ID "));
    assert!(lines[2].starts_with("Command running in background with session ID "));
    let mut ids = lines
        .iter()
        .map(|line| line.rsplit(' ').next().unwrap().parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.iter().all(|id| (1_000..10_000).contains(id)));
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );
    assert!(cell.more_output().is_none(), "announced once, then silent");
    cell.acknowledge_output();
    std::fs::write(release, "").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !registered.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        jobs(&*cell).len(),
        4,
        "a late source is a source like any other"
    );
    let late = cell.more_output().unwrap().output;
    cell.acknowledge_output();
    assert!(late.starts_with("Operation pending running in background with session ID "));
    let late_id = late
        .rsplit(' ')
        .next()
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!((1_000..10_000).contains(&late_id));
    assert!(!ids.contains(&late_id));
    ids.push(late_id);
    assert!(cell.more_output().is_none());
    cell.acknowledge_output();
    cell.cancel();
    until(&wake, &*cell, Signal::Ended).await;
    assert!(jobs(&*cell).iter().all(|job| job.finished.unwrap().failed));
    let finished = cell.more_output().unwrap().output;
    cell.acknowledge_output();
    let positions = ids
        .iter()
        .map(|id| finished.find(&format!("Session ID: {id}")).unwrap())
        .collect::<Vec<_>>();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{finished}"
    );
    assert!(jobs(&*cell).is_empty());
}

#[tokio::test]
async fn checkin_policy_is_validated_and_does_not_discard_output() {
    let tool = PythonNotebook::new(shell(), Vec::new()).unwrap();
    assert!(crate::python_instructions(&[]).contains("wake_on_tools=False"));
    let wake = Arc::new(Notify::new());
    let mut cell = tool.exec(
        call(
            "checkin",
            json!(
                r#"
assert "set_patience" not in globals()
for args in [
    dict(after_seconds=True), dict(after_seconds=0), dict(after_seconds=3601),
    dict(after_seconds=1.5), dict(wake_on_tools="false"), dict(wake_on_tools=0), dict(seconds=300),
]:
    try:
        set_checkin(**args)
    except (TypeError, ValueError):
        pass
    else:
        raise AssertionError(args)
set_checkin(after_seconds=300, wake_on_tools=False)
notify("buffered notification")
await command("printf buffered-command")
"#
            ),
        ),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &*cell, Signal::Ended).await;
    assert_eq!(
        cell.execution().facts().checkin,
        Some(crate::PythonCheckin {
            after: Duration::from_secs(300),
            wake_on_tools: false,
        })
    );
    let output = cell.first_output();
    cell.acknowledge_output();
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(
        output.output.contains("buffered notification"),
        "{output:?}"
    );
    assert!(output.output.contains("buffered-command"), "{output:?}");
}

#[tokio::test]
async fn output_is_leased_until_its_owner_acknowledges_it() {
    let notebook = python(shell(), Vec::new());
    let wake = Arc::new(Notify::new());
    let mut cell = notebook.exec(
        call("lease", json!("print('retained')")),
        SourceWaker::new(wake.clone()),
    );
    until(&wake, &cell, Signal::Ended).await;
    let first = cell.first_output();
    assert_eq!(first.output.as_str(), "retained\n");
    assert_eq!(cell.first_output(), first);
    assert_eq!(cell.more_output(), Some(first));
    assert!(!cell.done(), "unacknowledged output prevents reaping");
    assert!(cell.acknowledge_output());
    assert!(!cell.acknowledge_output());
    assert!(cell.done());
    assert_eq!(cell.more_output(), None);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shutdown_reaps_owned_commands_and_stops_host_calls_before_returning() {
    let directory = tempfile::tempdir().unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tool = python(
        shell_in(&directory),
        vec![Arc::new(PendingTool(count.clone()))],
    );
    let wake = Arc::new(Notify::new());
    let _cell = tool.exec(
        call(
            "shutdown",
            json!("job = command('echo $$ > owned-pid; exec sleep 60')\npending()"),
        ),
        SourceWaker::new(wake.clone()),
    );
    let path = directory.path().join("owned-pid");
    let pid: u32 = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if count.load(std::sync::atomic::Ordering::SeqCst) == 1
                && let Ok(pid) = std::fs::read_to_string(&path)
                && let Ok(pid) = pid.trim().parse()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), tool.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "owned child was not reaped"
    );
    let mut rejected = tool.exec(
        call(
            "after-shutdown",
            json!("Path('wrong').write_text('must not run')"),
        ),
        SourceWaker::new(wake),
    );
    assert!(
        rejected
            .first_output()
            .output
            .contains("Python notebook closed")
    );
    assert!(!directory.path().join("wrong").exists());
    tool.shutdown().await.unwrap();
}

/// Local code submission through real Bash completion and result rendering.
/// Uses the workset's Tokio worker count; excludes daemon/provider transport.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual release-mode awaited command latency benchmark"]
async fn benchmark_awaited_command() {
    tokio::spawn(async {
        let cwd = camino::Utf8PathBuf::try_from(std::env::current_dir().unwrap()).unwrap();
        let notebook = python(
            ShellTools::in_directory(Duration::from_secs(30), cwd, PathOverrides::default()),
            Vec::new(),
        );
        // Optional untimed Python setup supports same-binary profiling/A-B runs.
        if let Ok(source) = std::env::var("RHO_BENCH_SETUP") {
            let wake = Arc::new(Notify::new());
            let mut cell = notebook.exec(
                call("benchmark-setup", json!(source)),
                SourceWaker::new(wake.clone()),
            );
            until(&wake, &cell, Signal::Ended).await;
            let output = cell.first_output();
            cell.acknowledge_output();
            assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
            assert!(cell.done());
        }
        let mut samples = Vec::new();
        for n in 0..1010 {
            let wake = Arc::new(Notify::new());
            let call = call(&format!("bench-{n}"), json!("await command(\"true\")"));
            let start = std::time::Instant::now();
            let mut cell = notebook.exec(call, SourceWaker::new(wake.clone()));
            until(&wake, &cell, Signal::Ended).await;
            let output = cell.first_output();
            cell.acknowledge_output();
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
            assert!(
                output.output.contains("Process exited with code 0"),
                "{output:?}"
            );
            assert!(cell.done());
            if n >= 10 {
                samples.push(elapsed);
            }
        }
        samples.sort_by(f64::total_cmp);
        eprintln!(
            "await command(true), 1000 warm samples: mean_ms={:.3} median_ms={:.3} p95_ms={:.3}",
            samples.iter().sum::<f64>() / samples.len() as f64,
            (samples[499] + samples[500]) / 2.0,
            samples[949],
        );
        notebook.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}
