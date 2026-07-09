//! End-to-end tests for the code-mode session: REPL persistence, nested tool
//! dispatch, yield/wait/terminate, and the model-facing response format.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use rho_code_mode::{CodeModeSession, NestedTool, ToolDispatcher, WaitArgs};
use rho_core::{ToolCall, ToolOutput, ToolOutputStatus, ToolSpec, ToolType};
use serde_json::json;

struct FakeDispatcher {
    notifications: Mutex<Vec<String>>,
}

impl FakeDispatcher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            notifications: Mutex::new(Vec::new()),
        })
    }
}

impl ToolDispatcher for FakeDispatcher {
    fn call_tool(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            match call.name.as_str() {
                "echo" => ToolOutput {
                    output: Arc::new(format!("echo:{}", call.arguments)),
                    status: ToolOutputStatus::Success,
                },
                "slow_echo" => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    ToolOutput {
                        output: Arc::new("slow done".to_string()),
                        status: ToolOutputStatus::Success,
                    }
                }
                "hang" => {
                    // Completes only through cell cancellation.
                    futures::future::pending::<()>().await;
                    unreachable!()
                }
                "fail" => ToolOutput {
                    output: Arc::new("tool exploded".to_string()),
                    status: ToolOutputStatus::Error,
                },
                other => ToolOutput {
                    output: Arc::new(format!("unknown tool {other}")),
                    status: ToolOutputStatus::Error,
                },
            }
        })
    }

    fn notify(&self, text: String) {
        self.notifications.lock().unwrap().push(text);
    }
}

fn nested_tools() -> Vec<NestedTool> {
    ["echo", "slow_echo", "hang", "fail"]
        .into_iter()
        .map(|name| {
            NestedTool::from_spec(&ToolSpec {
                name: name.try_into().unwrap(),
                tool_type: ToolType::Function,
                description: format!("test tool {name}"),
                input_schema: json!({ "type": "object", "properties": {} }),
                format: None,
            })
        })
        .collect()
}

fn session() -> (CodeModeSession, Arc<FakeDispatcher>) {
    let dispatcher = FakeDispatcher::new();
    let session = CodeModeSession::new(nested_tools(), dispatcher.clone()).unwrap();
    (session, dispatcher)
}

fn cell_id(output: &str) -> String {
    let marker = "Script running with cell ID ";
    let start = output.find(marker).expect("yielded response") + marker.len();
    output[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect()
}

#[tokio::test]
async fn repl_scope_persists_across_cells() {
    let (session, _) = session();

    let first = session.execute("let counter = 41; text('set')").await;
    assert_eq!(first.status, ToolOutputStatus::Success, "{}", first.output);
    assert!(
        first.output.starts_with("Script completed"),
        "{}",
        first.output
    );

    let second = session.execute("counter += 1; text(String(counter))").await;
    assert!(second.output.contains("42"), "{}", second.output);

    // Redeclaration is legal in REPL mode.
    let third = session
        .execute("let counter = 5; text(String(counter))")
        .await;
    assert!(third.output.contains("5"), "{}", third.output);
}

#[tokio::test]
async fn nested_tool_calls_round_trip() {
    let (session, _) = session();
    let result = session
        .execute("const r = await tools.echo({ v: 1 }); text(r)")
        .await;
    assert_eq!(
        result.status,
        ToolOutputStatus::Success,
        "{}",
        result.output
    );
    assert!(
        result.output.contains(r#"echo:{"v":1}"#),
        "{}",
        result.output
    );
}

#[tokio::test]
async fn failing_tool_rejects_and_is_catchable() {
    let (session, _) = session();
    let uncaught = session.execute("await tools.fail({})").await;
    assert_eq!(
        uncaught.status,
        ToolOutputStatus::Error,
        "{}",
        uncaught.output
    );
    assert!(
        uncaught.output.contains("Script failed"),
        "{}",
        uncaught.output
    );
    assert!(
        uncaught.output.contains("tool exploded"),
        "{}",
        uncaught.output
    );

    let caught = session
        .execute("try { await tools.fail({}) } catch (e) { text('caught ' + e.message) }")
        .await;
    assert_eq!(
        caught.status,
        ToolOutputStatus::Success,
        "{}",
        caught.output
    );
    assert!(caught.output.contains("caught"), "{}", caught.output);
}

#[tokio::test]
async fn slow_cell_yields_then_wait_collects_completion() {
    let (session, _) = session();
    let first = session
        .execute("// @exec: {\"yield_time_ms\": 50}\nconst r = await tools.slow_echo({}); text(r)")
        .await;
    assert!(
        first.output.starts_with("Script running with cell ID"),
        "{}",
        first.output
    );

    let result = session
        .wait(WaitArgs {
            cell_id: cell_id(&first.output),
            yield_time_ms: 5_000,
            max_tokens: None,
            terminate: false,
        })
        .await;
    assert!(
        result.output.starts_with("Script completed"),
        "{}",
        result.output
    );
    assert!(result.output.contains("slow done"), "{}", result.output);

    // The cell is closed after delivering its final result.
    let missing = session
        .wait(WaitArgs {
            cell_id: cell_id(&first.output),
            yield_time_ms: 100,
            max_tokens: None,
            terminate: false,
        })
        .await;
    assert!(missing.output.contains("not found"), "{}", missing.output);
}

#[tokio::test]
async fn concurrent_cells_interleave() {
    let (session, _) = session();
    let slow = session
        .execute(
            "// @exec: {\"yield_time_ms\": 50}\nglobalThis.slowOut = await tools.slow_echo({});",
        )
        .await;
    assert!(slow.output.starts_with("Script running"), "{}", slow.output);

    // A second cell runs to completion while the first is parked.
    let fast = session.execute("text('fast ' + (1 + 1))").await;
    assert!(fast.output.contains("fast 2"), "{}", fast.output);

    let done = session
        .wait(WaitArgs {
            cell_id: cell_id(&slow.output),
            yield_time_ms: 5_000,
            max_tokens: None,
            terminate: false,
        })
        .await;
    assert!(
        done.output.starts_with("Script completed"),
        "{}",
        done.output
    );
}

#[tokio::test]
async fn terminate_parked_cell_rejects_its_tool_call() {
    let (session, _) = session();
    let parked = session
        .execute("// @exec: {\"yield_time_ms\": 50}\nawait tools.hang({}); text('unreachable')")
        .await;
    assert!(
        parked.output.starts_with("Script running"),
        "{}",
        parked.output
    );

    let terminated = session
        .wait(WaitArgs {
            cell_id: cell_id(&parked.output),
            yield_time_ms: 1_000,
            max_tokens: None,
            terminate: true,
        })
        .await;
    assert!(
        terminated.output.starts_with("Script terminated"),
        "{}",
        terminated.output
    );

    // The session stays healthy afterwards.
    let after = session.execute("text('alive')").await;
    assert!(after.output.contains("alive"), "{}", after.output);
}

#[tokio::test]
async fn terminate_busy_loop_preserves_session_state() {
    let (session, _) = session();
    let seeded = session.execute("let keep = 'kept'; text('seeded')").await;
    assert!(
        seeded.output.starts_with("Script completed"),
        "{}",
        seeded.output
    );

    let spin = session
        .execute("// @exec: {\"yield_time_ms\": 50}\nglobalThis.spun = 0; while (true) { globalThis.spun++; }")
        .await;
    assert!(spin.output.starts_with("Script running"), "{}", spin.output);

    let terminated = session
        .wait(WaitArgs {
            cell_id: cell_id(&spin.output),
            yield_time_ms: 1_000,
            max_tokens: None,
            terminate: true,
        })
        .await;
    assert!(
        terminated.output.starts_with("Script terminated"),
        "{}",
        terminated.output
    );

    let after = session
        .execute("text(keep + ' ' + String(globalThis.spun > 0))")
        .await;
    assert!(after.output.contains("kept true"), "{}", after.output);
}

#[tokio::test]
async fn exit_ends_script_successfully() {
    let (session, _) = session();
    let result = session
        .execute("text('before'); exit(); text('after')")
        .await;
    assert_eq!(
        result.status,
        ToolOutputStatus::Success,
        "{}",
        result.output
    );
    assert!(
        result.output.starts_with("Script completed"),
        "{}",
        result.output
    );
    assert!(result.output.contains("before"), "{}", result.output);
    assert!(!result.output.contains("after"), "{}", result.output);
}

#[tokio::test]
async fn notify_reaches_dispatcher() {
    let (session, dispatcher) = session();
    let result = session
        .execute("notify('progress note'); text('done')")
        .await;
    assert!(result.output.contains("done"), "{}", result.output);
    assert_eq!(
        dispatcher.notifications.lock().unwrap().as_slice(),
        ["progress note"]
    );
}

#[tokio::test]
async fn yield_control_returns_early_while_script_continues() {
    let (session, _) = session();
    let first = session
        .execute("text('early'); yield_control(); await tools.slow_echo({}); text('late')")
        .await;
    assert!(
        first.output.starts_with("Script running"),
        "{}",
        first.output
    );
    assert!(first.output.contains("early"), "{}", first.output);
    assert!(!first.output.contains("late"), "{}", first.output);

    let rest = session
        .wait(WaitArgs {
            cell_id: cell_id(&first.output),
            yield_time_ms: 5_000,
            max_tokens: None,
            terminate: false,
        })
        .await;
    assert!(rest.output.contains("late"), "{}", rest.output);
    assert!(!rest.output.contains("early"), "{}", rest.output);
}

#[tokio::test]
async fn script_errors_are_reported() {
    let (session, _) = session();
    let result = session.execute("throw new Error('boom')").await;
    assert_eq!(result.status, ToolOutputStatus::Error, "{}", result.output);
    assert!(
        result.output.starts_with("Script failed"),
        "{}",
        result.output
    );
    assert!(result.output.contains("boom"), "{}", result.output);
}

#[tokio::test]
async fn wait_on_unknown_cell_reports_not_found() {
    let (session, _) = session();
    let result = session
        .wait(WaitArgs {
            cell_id: "999".to_string(),
            yield_time_ms: 100,
            max_tokens: None,
            terminate: false,
        })
        .await;
    assert_eq!(result.status, ToolOutputStatus::Error, "{}", result.output);
    assert!(
        result.output.contains("exec cell 999 not found"),
        "{}",
        result.output
    );
}

#[tokio::test]
async fn output_is_truncated_to_budget() {
    let (session, _) = session();
    let result = session
        .execute("// @exec: {\"max_output_tokens\": 10}\ntext('x'.repeat(10000))")
        .await;
    assert!(result.output.contains("truncated"), "{}", result.output);
    assert!(result.output.len() < 2_000, "{}", result.output.len());
}

#[tokio::test]
async fn set_timeout_runs_and_clear_timeout_cancels() {
    let (session, _) = session();
    let result = session
        .execute(
            "let fired = [];\n\
             const keep = setTimeout(() => fired.push('keep'), 10);\n\
             const gone = setTimeout(() => fired.push('gone'), 10);\n\
             clearTimeout(gone);\n\
             await new Promise((resolve) => setTimeout(resolve, 100));\n\
             text(fired.join(','))",
        )
        .await;
    assert!(
        result.output.starts_with("Script completed"),
        "{}",
        result.output
    );
    assert!(result.output.contains("keep"), "{}", result.output);
    assert!(!result.output.contains("gone"), "{}", result.output);
}
