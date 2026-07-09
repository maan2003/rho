//! The JS runtime thread: one V8 isolate per session, cells evaluated in
//! REPL mode over an in-process inspector session (the same mechanism Deno's
//! REPL and Jupyter kernel use). `JsRuntime` is not `Send`, so everything V8
//! lives on this thread; the session handle communicates through commands,
//! shared cell state, and the thread-safe isolate handle.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deno_core::{
    InspectorMsg, InspectorMsgKind, InspectorSessionKind, JsRuntime, JsRuntimeInspector, OpState,
    PollEventLoopOptions, RuntimeOptions, op2, v8,
};
use rho_core::{ToolCall, ToolCallId, ToolName, ToolOutputStatus, ToolType};
use serde_json::{Value as JsonValue, json};
use tokio::sync::mpsc;

use crate::cell::{CellShared, CellStatus};
use crate::description::NestedTool;
use crate::session::ToolDispatcher;

/// Milliseconds since an arbitrary epoch; stale means the thread is stuck in
/// a synchronous JS segment (the busy-loop case).
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const HEARTBEAT_STALE: Duration = Duration::from_millis(400);

pub(crate) struct Command {
    pub(crate) cell: Arc<CellShared>,
    pub(crate) source: String,
}

pub(crate) struct RuntimeHandle {
    pub(crate) isolate: v8::IsolateHandle,
    pub(crate) heartbeat: Arc<AtomicU64>,
}

struct OpCtx {
    dispatcher: Arc<dyn ToolDispatcher>,
    cells: Arc<Mutex<HashMap<u32, Arc<CellShared>>>>,
    tool_types: HashMap<String, ToolType>,
    next_tool_call: std::cell::Cell<u64>,
}

impl OpCtx {
    fn cell(&self, id: u32) -> Option<Arc<CellShared>> {
        self.cells.lock().unwrap().get(&id).cloned()
    }

    fn running_cell(&self, id: u32) -> Result<Arc<CellShared>, deno_error::JsErrorBox> {
        match self.cell(id) {
            Some(cell) if cell.is_running() => Ok(cell),
            _ => Err(deno_error::JsErrorBox::generic(format!(
                "exec cell {id} is not running"
            ))),
        }
    }
}

fn op_ctx(state: &Rc<RefCell<OpState>>) -> Rc<OpCtx> {
    state.borrow().borrow::<Rc<OpCtx>>().clone()
}

#[op2]
#[string]
async fn op_call_tool(
    state: Rc<RefCell<OpState>>,
    #[smi] cell_id: u32,
    #[string] name: String,
    #[string] arguments: String,
) -> Result<String, deno_error::JsErrorBox> {
    let ctx = op_ctx(&state);
    let cell = ctx.running_cell(cell_id)?;
    let tool_type = *ctx
        .tool_types
        .get(&name)
        .ok_or_else(|| deno_error::JsErrorBox::generic(format!("unknown tool `{name}`")))?;
    let tool_name =
        ToolName::try_from(name).map_err(|err| deno_error::JsErrorBox::generic(err.to_string()))?;
    let call_seq = ctx.next_tool_call.get();
    ctx.next_tool_call.set(call_seq + 1);
    let call = ToolCall {
        id: ToolCallId::try_from(format!("code-mode-{cell_id}-{call_seq}"))
            .map_err(|err| deno_error::JsErrorBox::generic(err.to_string()))?,
        name: tool_name,
        tool_type,
        arguments,
    };
    let dispatch = ctx.dispatcher.call_tool(call);
    tokio::select! {
        biased;
        _ = cell.cancel.cancelled() => {
            Err(deno_error::JsErrorBox::generic("exec cell terminated"))
        }
        output = dispatch => match output.status {
            ToolOutputStatus::Success => Ok(output.output.as_ref().clone()),
            ToolOutputStatus::Error | ToolOutputStatus::Cancelled => {
                Err(deno_error::JsErrorBox::generic(output.output.as_ref().clone()))
            }
        },
    }
}

#[op2(fast)]
fn op_text(state: Rc<RefCell<OpState>>, #[smi] cell_id: u32, #[string] text: String) {
    let ctx = op_ctx(&state);
    if let Some(cell) = ctx.cell(cell_id)
        && cell.is_running()
    {
        cell.push_output(text);
    }
}

#[op2(fast)]
fn op_notify(state: Rc<RefCell<OpState>>, #[smi] cell_id: u32, #[string] text: String) {
    let ctx = op_ctx(&state);
    if let Some(cell) = ctx.cell(cell_id)
        && cell.is_running()
        && !text.trim().is_empty()
    {
        ctx.dispatcher.notify(text);
    }
}

#[op2(fast)]
fn op_yield(state: Rc<RefCell<OpState>>, #[smi] cell_id: u32) {
    let ctx = op_ctx(&state);
    if let Some(cell) = ctx.cell(cell_id) {
        cell.yield_requested.store(true, Ordering::Release);
        cell.notify.notify_waiters();
    }
}

#[op2(fast)]
fn op_exit(state: Rc<RefCell<OpState>>, #[smi] cell_id: u32) {
    let ctx = op_ctx(&state);
    if let Some(cell) = ctx.cell(cell_id) {
        cell.exit_requested.store(true, Ordering::Release);
    }
}

/// Returns false when the sleep was cancelled because the cell ended.
#[op2]
async fn op_sleep(state: Rc<RefCell<OpState>>, #[smi] cell_id: u32, ms: f64) -> bool {
    let ctx = op_ctx(&state);
    let Some(cell) = ctx.cell(cell_id) else {
        return false;
    };
    let sleep = tokio::time::sleep(Duration::from_millis(ms.max(0.0) as u64));
    tokio::select! {
        biased;
        _ = cell.cancel.cancelled() => false,
        _ = sleep => cell.is_running(),
    }
}

deno_core::extension!(
    rho_code_mode,
    ops = [
        op_call_tool,
        op_text,
        op_notify,
        op_yield,
        op_exit,
        op_sleep
    ],
);

pub(crate) fn spawn_runtime_thread(
    tools: Vec<NestedTool>,
    dispatcher: Arc<dyn ToolDispatcher>,
    cells: Arc<Mutex<HashMap<u32, Arc<CellShared>>>>,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    ready_tx: std::sync::mpsc::SyncSender<Result<RuntimeHandle, String>>,
) {
    std::thread::Builder::new()
        .name("rho-code-mode".to_string())
        .spawn(move || {
            let tokio_runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(tokio_runtime) => tokio_runtime,
                Err(err) => {
                    let _ = ready_tx.send(Err(format!("failed to build tokio runtime: {err}")));
                    return;
                }
            };
            tokio_runtime.block_on(run(tools, dispatcher, cells, cmd_rx, ready_tx));
        })
        .expect("spawn rho-code-mode thread");
}

async fn run(
    tools: Vec<NestedTool>,
    dispatcher: Arc<dyn ToolDispatcher>,
    cells: Arc<Mutex<HashMap<u32, Arc<CellShared>>>>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    ready_tx: std::sync::mpsc::SyncSender<Result<RuntimeHandle, String>>,
) {
    let mut runtime = JsRuntime::new(RuntimeOptions {
        inspector: true,
        extensions: vec![rho_code_mode::init()],
        ..Default::default()
    });

    let tool_types = tools
        .iter()
        .map(|tool| (tool.name.as_str().to_string(), tool.tool_type))
        .collect();
    let op_ctx = Rc::new(OpCtx {
        dispatcher,
        cells: Arc::clone(&cells),
        tool_types,
        next_tool_call: std::cell::Cell::new(1),
    });
    runtime.op_state().borrow_mut().put(op_ctx);

    // Responses are correlated back to cells by CDP message id. The callback
    // runs on this thread during dispatch/microtask work.
    let pending_evals: Rc<RefCell<HashMap<i32, Arc<CellShared>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let callback_pending = Rc::clone(&pending_evals);
    let callback = Box::new(move |msg: InspectorMsg| {
        let InspectorMsgKind::Message(eval_id) = msg.kind else {
            return;
        };
        let Some(cell) = callback_pending.borrow_mut().remove(&eval_id) else {
            return;
        };
        let response: JsonValue = match serde_json::from_str(&msg.content) {
            Ok(response) => response,
            Err(_) => json!({}),
        };
        cell.finish(evaluation_outcome(&cell, &response));
    });
    let mut session = JsRuntimeInspector::create_local_session(
        runtime.inspector(),
        callback,
        InspectorSessionKind::NonBlocking {
            wait_for_disconnect: false,
        },
    );

    if let Err(err) = install_prelude(&mut runtime, &tools) {
        let _ = ready_tx.send(Err(err));
        return;
    }

    let heartbeat = Arc::new(AtomicU64::new(now_millis()));
    let _ = ready_tx.send(Ok(RuntimeHandle {
        isolate: runtime.v8_isolate().thread_safe_handle(),
        heartbeat: Arc::clone(&heartbeat),
    }));

    let mut next_eval_id: i32 = 1;
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        heartbeat.store(now_millis(), Ordering::Release);
        let command = tokio::select! {
            biased;
            command = cmd_rx.recv() => Some(command),
            _ = tick.tick() => None,
            poll = runtime.run_event_loop(PollEventLoopOptions::default()) => {
                if let Err(err) = poll {
                    tracing::warn!("code-mode event loop error: {err}");
                }
                // Idle: block until there is new work rather than spinning.
                tokio::select! {
                    command = cmd_rx.recv() => Some(command),
                    _ = tick.tick() => None,
                }
            }
        };
        match command {
            None => continue,
            Some(None) => break,
            Some(Some(Command { cell, source })) => {
                let eval_id = next_eval_id;
                next_eval_id += 1;
                pending_evals
                    .borrow_mut()
                    .insert(eval_id, Arc::clone(&cell));
                let begin = format!("__rhoBeginCell({});", cell.id);
                if runtime.execute_script("rho:begin-cell", begin).is_err() {
                    pending_evals.borrow_mut().remove(&eval_id);
                    cell.finish(CellStatus::Completed {
                        error: Some("failed to enter cell context".to_string()),
                    });
                    continue;
                }
                // Runs the cell's synchronous prefix inline; a busy loop
                // blocks here until terminated via the isolate handle.
                session.post_message(
                    eval_id,
                    "Runtime.evaluate",
                    Some(json!({
                        "expression": source,
                        "replMode": true,
                        "awaitPromise": true,
                    })),
                );
                let _ = runtime.execute_script("rho:end-cell", "__rhoBeginCell(0);");
            }
        }
    }

    // Session dropped: cancel whatever is still alive.
    let cells = cells.lock().unwrap().drain().collect::<Vec<_>>();
    for (_, cell) in cells {
        cell.finish(CellStatus::Terminated);
    }
}

fn evaluation_outcome(cell: &CellShared, response: &JsonValue) -> CellStatus {
    let terminated = cell.terminate_requested.load(Ordering::Acquire);
    let exited = cell.exit_requested.load(Ordering::Acquire);

    if let Some(error) = response.get("error") {
        // "Promise was collected" is how REPL-mode evaluations that settle to
        // `undefined` can report; the evaluation itself succeeded.
        let message = error
            .get("message")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        if message == "Promise was collected" {
            return CellStatus::Completed { error: None };
        }
        if terminated {
            return CellStatus::Terminated;
        }
        return CellStatus::Completed {
            error: Some(message.to_string()),
        };
    }

    if let Some(details) = response.pointer("/result/exceptionDetails") {
        if exited {
            return CellStatus::Completed { error: None };
        }
        if terminated {
            return CellStatus::Terminated;
        }
        let error = details
            .pointer("/exception/description")
            .or_else(|| details.get("text"))
            .and_then(JsonValue::as_str)
            .unwrap_or("script failed")
            .to_string();
        return CellStatus::Completed { error: Some(error) };
    }

    CellStatus::Completed { error: None }
}

fn install_prelude(runtime: &mut JsRuntime, tools: &[NestedTool]) -> Result<(), String> {
    runtime
        .execute_script("rho:prelude", include_str!("prelude.js"))
        .map_err(|err| format!("failed to install code-mode prelude: {err}"))?;

    let metadata = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name.as_str(),
                "global_name": tool.global_name(),
                "description": tool.description,
                "kind": match tool.tool_type {
                    ToolType::Function => "function",
                    ToolType::Custom => "freeform",
                },
            })
        })
        .collect::<Vec<_>>();
    let metadata = serde_json::to_string(&JsonValue::Array(metadata))
        .map_err(|err| format!("failed to encode tool metadata: {err}"))?
        // JSON is not quite a JS subset: escape the separators that are valid
        // in JSON strings but terminate lines in JS source.
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    runtime
        .execute_script("rho:init", format!("__rhoInit({metadata});"))
        .map_err(|err| format!("failed to initialize code-mode tools: {err}"))?;
    Ok(())
}

pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
