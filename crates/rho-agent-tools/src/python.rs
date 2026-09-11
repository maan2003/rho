//! Python notebook adapter. Rho owns subprocesses, retained output, and source
//! policy.
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rho_core::{ToolCall, ToolName, ToolOutput, ToolOutputStatus, ToolSpec, ToolType, UnixMs};
use rho_python::{Event, Input, Sender, Session};
use rho_tool_shell::{BoundedOutput, ProcessEvent, ShellTools, decode_output_lossy};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::{Finished, FutureTool, SourceWaker, Tool, ToolSession, output, stands_on_its_own};

const LOG_LIMIT: usize = 8 * 1024 * 1024;
const JOB_LIMIT: usize = 64;

pub struct PythonTool {
    session: Session,
    shared: Arc<Shared>,
    spec: ToolSpec,
    runtime: tokio::runtime::Handle,
}
struct Shared {
    next_cell: AtomicU64,
    shell: ShellTools,
    others: HashMap<String, Arc<dyn FutureTool>>,
    cells: Mutex<HashMap<u64, Arc<Mutex<ExecState>>>>,
    jobs: Mutex<BTreeMap<u64, Arc<Job>>>,
    sequence: AtomicU64,
    images: Mutex<BTreeMap<u64, rho_core::ImageContent>>,
}
// Each modular shift is reversible by subtraction. Together they permute all
// 9,000 slots without a lookup table; public labels repeat every 9,000 internal
// requests. Handles and output ordering always use the original internal ID.
fn session_id(internal_id: u64) -> u32 {
    let x = internal_id % 9_000;
    let (mut left, mut right) = (x / 100, x % 100);
    left = (left + right * right + 17 * right + 43) % 90;
    right = (right + left * left + 29 * left + 71) % 100;
    left = (left + right * right + 53 * right + 19) % 90;
    right = (right + left * left + 11 * left + 37) % 100;
    (1_000 + 100 * left + right) as u32
}

#[derive(Clone, Debug, Default)]
pub struct PythonStreamProgress {
    pub returned: bool,
    pub ready: Option<usize>,
    pub settled: Option<(usize, Option<String>)>,
}

struct ExecState {
    stream: PythonStreamProgress,
    waker: SourceWaker,
    output: BoundedOutput,
    since: Option<UnixMs>,
    important: Option<UnixMs>,
    finished: Option<UnixMs>,
    started: bool,
    returned: Option<UnixMs>,
    returned_error: Option<String>,
    completion: Option<crate::PythonCompletion>,
    operations: Vec<Arc<Mutex<Operation>>>,
    dispatched: bool,
    error: bool,
    delivered: bool,
    meaningful: bool,
    checkin: Option<crate::PythonCheckin>,
    jobs: Vec<Arc<Job>>,
    pending: usize,
    cancelled: tokio::sync::watch::Sender<bool>,
    images: Vec<rho_core::ImageContent>,
}
impl ExecState {
    fn say(&mut self, text: &str, important: bool) {
        self.write(text, important);
        self.output.push(b"\n");
    }
    fn write(&mut self, text: &str, important: bool) {
        self.output.push(text.as_bytes());
        self.meaningful = true;
        self.since.get_or_insert_with(UnixMs::now);
        if important {
            self.important.get_or_insert_with(UnixMs::now);
        }
        self.waker.wake();
    }
    fn closed(&self) -> bool {
        self.finished.is_some()
            && self.pending == 0
            && self
                .jobs
                .iter()
                .all(|j| j.state.lock().unwrap().finished.is_some())
    }
}
struct Operation {
    id: u64,
    name: String,
    output: BoundedOutput,
    announced: bool,
    finished: Option<UnixMs>,
}
struct Job {
    id: u64,
    name: String,
    state: Mutex<JobState>,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    cancel: Notify,
    budget: usize,
    ready: tokio::sync::watch::Sender<bool>,
}
struct JobState {
    announced: bool,
    file: std::fs::File,
    len: usize,
    dropped: usize,
    cursor: usize,
    unsent: BoundedOutput,
    since: Option<UnixMs>,
    important: Option<UnixMs>,
    finished: Option<(UnixMs, Value)>,
    delivered: bool,
}
/// When an `exec` call comes back to the model, which changes what the
/// guidance about waiting has to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecReturn {
    /// Rho's native runtime: the call is answered at the next request
    /// boundary, so the model ends its turn to wait.
    AtTurnEnd,
    /// A host that holds the call open, such as Claude Code's MCP tool call:
    /// the call returns when that same boundary would have answered it.
    Blocking,
}

/// System-prompt guidance for the Python runtime and its available host
/// functions, for the native runtime.
pub fn python_instructions(specs: &[ToolSpec]) -> String {
    python_instructions_for(specs, ExecReturn::AtTurnEnd)
}

/// System-prompt guidance for the Python runtime and its available host
/// functions.
pub fn python_instructions_for(specs: &[ToolSpec], returns: ExecReturn) -> String {
    let (opening, after_checkin) = match returns {
        ExecReturn::AtTurnEnd => (
            "`exec` is your only top-level tool. Issue at most one exec call per response.",
            "Then end the model turn. No separate exec is needed; this does not sleep or block Python.",
        ),
        ExecReturn::Blocking => (
            "`exec` (`mcp__py__exec`) is your only tool. Make one exec call at a time.",
            "The exec call then returns when the check-in fires, or sooner if something worth \
reporting happens; this does not sleep or block Python.",
        ),
    };
    let mut description = format!("## Python Code Mode\n\n{opening}\n");
    description.push_str(
        "It runs a persistent Python notebook with top-level await. Globals are shared;
live cells interleave at await. Use shell commands to inspect files and Python
to manipulate their data.
The Python standard library, PyYAML (`yaml`), and HTTPX (`httpx`) are available through ordinary \
imports.

Work registers immediately; output arrives automatically. Put independent calls in the same cell \
to run them concurrently; await only when later Python statements depend on completion.
- command(cmd, workdir=None, max_tokens=2000) returns a managed handle. Assignment and await are \
optional. Awaiting it returns completion metadata (id, exit_code), not stdout.
- write_stdin(handle, chars='', max_tokens=2000) registers an input write and retained-output \
read. It waits for stdin readiness, not subsequent output; an empty page is valid. Await only \
if Python needs the returned page or write completion. display(handle) also reads a page.
- handle.cancel() requests cancellation of a running command. Awaiting handle.cancel() waits only \
for the cancellation request to be handled; await handle to wait for the command to finish.
- agents.spawn_new_advisor(\"...\") starts an advisor consultation immediately. Include the \
question, relevant file paths, scope, and desired output in the string. Output arrives \
automatically; await only when code needs its result. Full guidance, examples, and argument \
schema are below when available.
- Before delegating, call display(agents.delegate_engineer) to read its full guidance and \
arguments. display(callable) shows its documentation.
- Host functions register work immediately. Await only for Python dependencies; text results are \
strings, JSON results are parsed values.
- Pending commands and operations expose session IDs from 1000 through 9999 when first reported as \
running. These are display labels derived from internal IDs and repeat every 9000 internal \
requests; use Python handles to await, inspect, or cancel commands. Commands finishing before \
their first reply do not expose an ID. Later output uses the same identity. A reply does not \
mean the source finished. Latest-cell sources are reported first, then older cells; sources \
within each cell follow registration order, without waiting for earlier sources to finish.

Python output: print(...) and text(value, max_tokens=2000) are ordinary output; notify(value, \
max_tokens=2000) is meaningful output that wakes sooner unless tool wakeups are disabled. \
image((await view_image(path=...))[\"content\"][0]) displays a returned image reference.

Examples

Run independent inspections concurrently in one exec—no gather, await, or result-printing:
```python
command(\"git diff --stat\")
command(\"rg -n 'TODO' src\")
```

Search the web without awaiting or reprinting the result:
```python
web.run(search_query=[{\"q\": \"Python asyncio TaskGroup documentation\"}])
```
Use the returned references in a later cell for web.run(open=[...]).

After editing a file, await only the dependencies:
```python
Path(\"config.toml\").write_text(updated_config)
check = await command(\"cargo check\")
if check[\"exit_code\"] == 0:
    command(\"cargo test\")
```

Use Python directly to manipulate file data:
```python
config = Path(\"config.toml\")
config.write_text(config.read_text().replace(\"retries = 2\", \"retries = 3\"))
```

Send stdin without blocking the notebook:
```python
job = command(\"python3 -c 'print(input())'\", max_tokens=100)
write_stdin(job, \"hello\\n\")
```
After automatic completion, a later cell can expand retained output:
```python
write_stdin(job, max_tokens=6000)
```

Keep a handle if you may want to stop a command:
```python
job = command(\"sleep 600\")
```
Cancel it from a later cell; completion arrives automatically:
```python
job.cancel()
# Only if subsequent code needs the command to have stopped:
await job
```

A live monitoring cell:
```python
progress = {\"checks\": 0}
while not Path(\"results.json\").exists():
    progress[\"checks\"] += 1
    await asyncio.sleep(5)
notify(\"Results are ready\")
```
Inspect its globals in a later cell without stopping it:
```python
text(progress)
```
The default check-in interval is 120 seconds. Omit set_checkin unless you want
a different interval or want to suppress tool wakeups. Set it alongside the work:
```python
command(\"git diff --check\")
command(\"cargo test\")
set_checkin(after_seconds=300)
```
",
    );
    description.push_str(after_checkin);
    description.push_str(
        "
If the cell also awaits a dependency, call set_checkin before that await; a suspended cell
may resume after its originating model turn has ended.

Wait for an advisor answer without tool events waking you early:
```python
set_checkin(after_seconds=300, wake_on_tools=False)
agents.spawn_new_advisor(\"Review the current diff for correctness. Report concrete blockers \
only.\")
```
The advisor's mail or a user message can wake you before the timer. Other work keeps running.

Details and limits
- Explicit output reads have a separate cursor starting at byte 0, so they can repeat automatic \
previews. Wait for command completion only if Python needs a complete final read.
- Budgets are capped at 10000 tokens. Each command retains its first 8 MiB with explicit overflow \
counts. Up to 64 handles are retained; oldest completed, delivered handles may be evicted. Up \
to 32 image references are retained.
- set_checkin(after_seconds=300, wake_on_tools=True) accepts 1..3600 seconds and overrides only \
this turn's default 120-second interval. By default, tool output and completion can wake \
earlier.
- set_checkin(after_seconds=300, wake_on_tools=False) waits for the timer, user messages, or agent \
mail (such as an advisor answer). No tool event wakes early: this includes current and older \
commands, host operations, errors, notify(), and exec completion. Work continues and buffered \
output is delivered on the next wake. Old cells cannot change a newer turn's policy.
- Python is in-process, not a sandbox. Cwd is private to the notebook; other process-global APIs \
retain normal semantics. Native extension packages are unsupported.

Available tools:
",
    );
    for spec in specs {
        let name = spec.name.as_str();
        if name == "spawn_engineer" {
            continue;
        }
        if name == "web__run" {
            description.push_str("web.run: standard OpenAI web run\n");
            continue;
        }
        let callable = match name {
            "ask_advisor" => "agents.spawn_new_advisor",
            "message_agent" => "agents.message",
            "interrupt_engineer" => "agents.cancel",
            _ => name,
        };
        let mut schema = spec.input_schema.clone();
        if name == "ask_advisor" {
            if let Some(message) = schema["properties"]
                .as_object_mut()
                .and_then(|properties| properties.remove("message"))
            {
                schema["properties"]["msg"] = message;
            }
            schema["required"] = json!(["msg"]);
        }
        description.push_str(&format!(
            "{}: {}\nArguments: {}\n",
            callable, spec.description, schema
        ));
    }
    description.push('\n');
    description
}

impl PythonTool {
    pub fn new(shell: ShellTools, others: Vec<Arc<dyn FutureTool>>) -> Result<Self, String> {
        let specs = others.iter().map(|tool| tool.spec()).collect::<Vec<_>>();
        let shared = Arc::new(Shared {
            next_cell: AtomicU64::new(1),
            shell,
            others: others
                .into_iter()
                .map(|t| (t.spec().name.as_str().to_owned(), t))
                .collect(),
            cells: Mutex::new(HashMap::new()),
            jobs: Mutex::new(BTreeMap::new()),
            sequence: AtomicU64::new(1),
            images: Mutex::new(BTreeMap::new()),
        });
        let shell = shared.shell.clone();
        let runtime = tokio::runtime::Handle::current();
        let setup_runtime = runtime.clone();
        let session = Session::new(
            move || {
                unsafe { setup_runtime.block_on(shell.enter_interpreter_thread()) }
                    .map_err(|error| error.to_string())
            },
            json!(specs),
        )?;
        Ok(Self {
            session,
            shared,
            spec: ToolSpec {
                name: ToolName::try_from("exec").unwrap(),
                tool_type: ToolType::Custom,
                description: "Execute Python in the persistent notebook.".into(),
                input_schema: Value::Null,
                format: Some(rho_core::ToolFormat::Text),
            },
            runtime,
        })
    }
}
impl Drop for PythonTool {
    fn drop(&mut self) {
        for cell in self.shared.cells.lock().unwrap().values() {
            let mut cell = cell.lock().unwrap();
            cell.cancelled.send_replace(true);
            cell.say("Python notebook closed", true);
        }
        for job in self.shared.jobs.lock().unwrap().values() {
            job.cancel.notify_one();
        }
    }
}
impl Tool for PythonTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn run(&self, call: ToolCall, waker: SourceWaker) -> Box<dyn ToolSession> {
        self.start(Some(call.arguments), waker)
    }
    fn start_stream(&self, waker: SourceWaker) -> Option<Box<dyn ToolSession>> {
        Some(self.start(None, waker))
    }
}
impl PythonTool {
    fn start(&self, source: Option<String>, waker: SourceWaker) -> Box<dyn ToolSession> {
        let cell = self.shared.next_cell.fetch_add(1, Ordering::Relaxed);
        let link = Arc::new(Mutex::new(ExecState {
            stream: PythonStreamProgress::default(),
            waker,
            output: BoundedOutput::for_tokens(Some(10000)),
            since: None,
            important: None,
            finished: None,
            started: false,
            returned: None,
            returned_error: None,
            completion: None,
            operations: Vec::new(),
            dispatched: false,
            error: false,
            delivered: false,
            meaningful: false,
            checkin: None,
            jobs: Vec::new(),
            pending: 0,
            cancelled: tokio::sync::watch::channel(false).0,
            images: Vec::new(),
        }));
        self.shared
            .cells
            .lock()
            .unwrap()
            .insert(cell, Arc::clone(&link));
        let exec = Arc::new(PythonExec {
            cell,
            link,
            shared: Arc::clone(&self.shared),
            sender: self.session.sender(),
            runtime: self.runtime.clone(),
        });
        let sender = self.session.sender();
        let result = match source {
            Some(source) => sender.execute(cell, source, exec.clone()),
            None => sender.stream(cell, exec.clone()),
        };
        if let Err(error) = result {
            self.shared.cells.lock().unwrap().remove(&cell);
            return Box::new(Finished::error(error));
        }
        Box::new(PythonCell(exec))
    }
}
async fn resolve(sender: &Sender, request: u64, result: Result<Value, String>) {
    let (value, error) = match result {
        Ok(value) => (value, None),
        Err(error) => (Value::Null, Some(error)),
    };
    if let Err(error) = sender
        .send_async(Input::Resolve {
            request,
            value,
            error,
        })
        .await
    {
        // A large result must fail its await rather than strand the cell.
        let _ = sender
            .send_async(Input::Resolve {
                request,
                value: Value::Null,
                error: Some(error),
            })
            .await;
    }
}
// Every Python host call registers ownership before the driver accepts the next
// event. Awaiting its future is optional and never controls the Rust lifetime.
fn register_call(
    shared: &Arc<Shared>,
    sender: &Sender,
    request: u64,
    name: String,
    args: Value,
    link: Arc<Mutex<ExecState>>,
    runtime: &tokio::runtime::Handle,
) {
    // Publish command handles synchronously: write_stdin in the same cell can
    // refer to a command whose process has not started yet.
    let job = if name == "command" {
        (|| -> Result<Arc<Job>, String> {
            let mut jobs = shared.jobs.lock().unwrap();
            if jobs.len() >= JOB_LIMIT {
                let old = jobs
                    .iter()
                    .find(|(_, j)| j.state.lock().unwrap().delivered)
                    .map(|(id, _)| *id);
                if let Some(id) = old {
                    jobs.remove(&id);
                } else {
                    return Err("64 command handles are still active or awaiting delivery".into());
                }
            }
            let budget = args["max_tokens"].as_u64().unwrap_or(2000).clamp(1, 10000) as usize;
            let job = Arc::new(Job {
                id: request,
                name: {
                    let mut name = args["cmd"]
                        .as_str()
                        .unwrap_or("command")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    if name.len() > 20 {
                        let mut end = 17;
                        while !name.is_char_boundary(end) {
                            end -= 1;
                        }
                        name.truncate(end);
                        name.push_str("...");
                    }
                    name
                },
                state: Mutex::new(JobState {
                    announced: false,
                    file: tempfile::tempfile().map_err(|e| e.to_string())?,
                    len: 0,
                    dropped: 0,
                    cursor: 0,
                    unsent: BoundedOutput::for_tokens(Some(budget)),
                    since: None,
                    important: None,
                    finished: None,
                    delivered: false,
                }),
                stdin: tokio::sync::Mutex::new(None),
                cancel: Notify::new(),
                budget,
                ready: tokio::sync::watch::channel(false).0,
            });
            jobs.insert(request, Arc::clone(&job));
            Ok(job)
        })()
        .map(Some)
    } else {
        Ok(None)
    };
    let operation = (name != "command").then(|| {
        Arc::new(Mutex::new(Operation {
            id: request,
            name: match name.as_str() {
                "web__run" => "web.run",
                "spawn_engineer" => "agents.delegate_engineer",
                "ask_advisor" => "agents.spawn_new_advisor",
                "message_agent" => "agents.message",
                "interrupt_engineer" => "agents.cancel",
                other => other,
            }
            .to_owned(),
            output: BoundedOutput::for_tokens(Some(10000)),
            announced: false,
            finished: None,
        }))
    });
    {
        let mut cell = link.lock().unwrap();
        cell.dispatched = true;
        if let Some(operation) = &operation {
            cell.operations.push(operation.clone());
        }
        cell.waker.wake();
        cell.pending += 1;
        if let Ok(Some(job)) = &job {
            cell.jobs.push(Arc::clone(job));
        }
    }
    let shared = Arc::clone(shared);
    let sender = sender.clone();
    runtime.spawn(async move {
        let mut cancelled = link.lock().unwrap().cancelled.subscribe();
        let work = async {
            match &job {
                Ok(Some(job)) => run_command(&shared.shell, job, &args, &link).await,
                Ok(None) => {
                    host_call(&shared, &name, args, &link, operation.as_ref().unwrap()).await
                }
                Err(error) => Err(error.clone()),
            }
        };
        let result = tokio::select! {
            biased;
            _ = cancelled.wait_for(|cancelled| *cancelled) => Err("Tool call cancelled".into()),
            result = work => result,
        };
        if let Ok(Some(job)) = &job {
            *job.stdin.lock().await = None;
            job.ready.send_replace(true);
            let summary = match &result {
                Ok(value) => value.clone(),
                Err(error) => json!({"id":request,"error":error}),
            };
            job.state.lock().unwrap().finished = Some((UnixMs::now(), summary));
        } else if let Err(error) = &result {
            operation
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .output
                .push(error.as_bytes());
            link.lock().unwrap().error = true;
        }
        {
            let mut cell = link.lock().unwrap();
            if let Some(operation) = operation {
                operation.lock().unwrap().finished = Some(UnixMs::now());
            }
            cell.pending -= 1;
            cell.waker.wake();
        }
        resolve(&sender, request, result).await;
    });
}

async fn run_command(
    shell: &ShellTools,
    job: &Job,
    args: &Value,
    link: &Arc<Mutex<ExecState>>,
) -> Result<Value, String> {
    let work = async {
        let cmd = args["cmd"].as_str().ok_or("command requires cmd")?;
        let mut process = shell
            .spawn(cmd, args["workdir"].as_str())
            .await
            .map_err(|e| e.to_string())?;
        *job.stdin.lock().await = process.take_stdin();
        job.ready.send_replace(true);
        let mut exit_code = None;
        loop {
            let event = process.next().await;
            match event {
                ProcessEvent::Output(chunk) => {
                    let mut state = job.state.lock().unwrap();
                    let keep = chunk.len().min(LOG_LIMIT - state.len);
                    let end = state.len as u64;
                    state
                        .file
                        .seek(SeekFrom::Start(end))
                        .map_err(|e| e.to_string())?;
                    state
                        .file
                        .write_all(&chunk[..keep])
                        .map_err(|e| e.to_string())?;
                    state.len += keep;
                    state.dropped = state.dropped.saturating_add(chunk.len() - keep);
                    state.unsent.push(&chunk);
                    state.since.get_or_insert_with(UnixMs::now);
                    if stands_on_its_own(&String::from_utf8_lossy(&chunk)) {
                        state.important.get_or_insert_with(UnixMs::now);
                    }
                }
                ProcessEvent::Exited(status) => exit_code = status.code(),
                ProcessEvent::Failed(error) => return Err(error),
                ProcessEvent::Closed => break,
            }
            link.lock().unwrap().waker.wake();
        }
        Ok(json!({"id": job.id, "exit_code": exit_code}))
    };
    tokio::select! {
        biased;
        _ = job.cancel.notified() => Err("Command cancelled".into()),
        result = work => result,
    }
}
async fn host_call(
    shared: &Shared,
    name: &str,
    args: Value,
    link: &Arc<Mutex<ExecState>>,
    operation: &Arc<Mutex<Operation>>,
) -> Result<Value, String> {
    if name == "image" {
        let id = args["id"].as_u64().ok_or("Expected an image reference")?;
        let image = shared
            .images
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or("Image expired or unknown")?;
        let mut cell = link.lock().unwrap();
        if cell.images.len() >= 20 {
            return Err("Cell image limit reached".into());
        }
        cell.images.push(image);
        operation.lock().unwrap().output.push(b"Image displayed");
        return Ok(Value::Null);
    }
    if matches!(name, "write_stdin" | "display" | "cancel_command") {
        let id = args["id"].as_u64().ok_or("Expected command handle")?;
        let job = shared
            .jobs
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or("Command handle expired or unknown")?;
        if name == "cancel_command" {
            job.cancel.notify_one();
            return Ok(Value::Null);
        }
        if name == "write_stdin" {
            let chars = args["chars"].as_str().ok_or("chars must be a string")?;
            if !chars.is_empty() {
                job.ready
                    .subscribe()
                    .wait_for(|ready| *ready)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut stdin = job.stdin.lock().await;
                let stdin = stdin.as_mut().ok_or("Command stdin not ready or closed")?;
                stdin
                    .write_all(chars.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                stdin.flush().await.map_err(|e| e.to_string())?;
            }
        }
        let mut state = job.state.lock().unwrap();
        let start = state.cursor;
        let size = (state.len - start)
            .min(args["max_tokens"].as_u64().unwrap_or(2000).clamp(1, 10000) as usize * 4);
        let mut bytes = vec![0; size];
        state
            .file
            .seek(SeekFrom::Start(start as u64))
            .map_err(|e| e.to_string())?;
        state
            .file
            .read_exact(&mut bytes)
            .map_err(|e| e.to_string())?;
        // Do not split a UTF-8 character merely because a page hit its budget.
        // Non-UTF-8 process output still follows the shell's lossy-text contract.
        if size < state.len - start
            && let Err(error) = std::str::from_utf8(&bytes)
            && error.error_len().is_none()
        {
            bytes.truncate(error.valid_up_to());
        }
        state.cursor += bytes.len();
        let result = json!({"id":id,"output":String::from_utf8_lossy(&bytes),"offset":start,"next_offset":state.cursor,"retained_bytes":state.len,"dropped_bytes":state.dropped,"finished":state.finished.as_ref().map(|(_,v)|v)});
        drop(state);
        operation
            .lock()
            .unwrap()
            .output
            .push(result.to_string().as_bytes());
        return Ok(result);
    }
    let tool = shared
        .others
        .get(name)
        .ok_or_else(|| format!("Unknown tool: {name}"))?;
    let spec = tool.spec();
    let call = ToolCall {
        id: format!("python-{name}")
            .try_into()
            .map_err(|_| "Invalid tool ID")?,
        name: spec.name,
        tool_type: spec.tool_type,
        arguments: if spec.tool_type == ToolType::Custom {
            args.as_str()
                .ok_or("Custom tool takes a string")?
                .to_owned()
        } else {
            args.to_string()
        },
    };
    let result = tool.call(call).await;
    if result.status != ToolOutputStatus::Success {
        return Err((*result.output).clone());
    }
    let mut content = Vec::new();
    if !result.images.is_empty() {
        let mut images = shared.images.lock().unwrap();
        for image in result.images.iter() {
            while images.len() >= 32 {
                images.pop_first();
            }
            let id = shared.sequence.fetch_add(1, Ordering::Relaxed);
            images.insert(id, image.clone());
            content.push(json!({"id":id}));
        }
    }
    if !result.output.is_empty() {
        operation
            .lock()
            .unwrap()
            .output
            .push(result.output.as_bytes());
    }
    if !content.is_empty() {
        Ok(json!({"output":*result.output,"content":content}))
    } else {
        Ok(serde_json::from_str(&result.output)
            .unwrap_or_else(|_| Value::String((*result.output).clone())))
    }
}
pub struct PythonExec {
    cell: u64,
    link: Arc<Mutex<ExecState>>,
    shared: Arc<Shared>,
    sender: Sender,
    runtime: tokio::runtime::Handle,
}
impl PythonExec {
    pub fn stream_progress(&self) -> PythonStreamProgress {
        let state = self.link.lock().unwrap();
        PythonStreamProgress {
            returned: state.returned.is_some(),
            ..state.stream.clone()
        }
    }

    pub fn feed(&self, source: String, eof: bool) -> Result<(), String> {
        self.sender.send(Input::StreamFeed {
            cell: self.cell,
            source,
            eof,
        })
    }

    pub fn permit(&self, end: usize) -> Result<(), String> {
        self.sender.send(Input::StreamPermit {
            cell: self.cell,
            end,
        })
    }

    /// Stop source admission, not the active unit or its managed commands.
    pub fn stop_stream(&self) {
        let sender = self.sender.clone();
        let cell = self.cell;
        self.runtime.spawn(async move {
            let _ = sender.send_async(Input::StreamStop { cell }).await;
        });
    }

    pub fn sequence(&self) -> u64 {
        self.cell
    }

    /// All Python activity and host operations have stopped; output may still
    /// need draining. Distinct from the submitted code's return.
    pub fn quiescent(&self) -> bool {
        self.link.lock().unwrap().closed()
    }

    pub fn facts(&self) -> crate::PythonExecFacts {
        let state = self.link.lock().unwrap();
        crate::PythonExecFacts {
            started: state.started,
            returned: state.returned,
            completion: state.completion,
            output: crate::PythonOutput {
                since: state.since,
                notification: state.important,
            },
            checkin: state.checkin,
        }
    }
}
struct PythonCell(Arc<PythonExec>);
impl std::ops::Deref for PythonCell {
    type Target = PythonExec;
    fn deref(&self) -> &PythonExec {
        &self.0
    }
}
impl rho_python::Execution for PythonExec {
    fn event(&self, event: Event) {
        match event {
            Event::UnitReady { end, .. } => {
                let mut state = self.link.lock().unwrap();
                state.stream.ready = Some(end);
                state.waker.wake();
            }
            Event::UnitSettled { end, error, .. } => {
                let mut state = self.link.lock().unwrap();
                state.stream.settled = Some((end, error));
                state.waker.wake();
            }
            Event::Call {
                request,
                name,
                arguments,
                ..
            } => {
                if *self.link.lock().unwrap().cancelled.borrow() {
                    let sender = self.sender.clone();
                    self.runtime.spawn(async move {
                        resolve(&sender, request, Err("Execution cancelled".into())).await;
                    });
                } else {
                    register_call(
                        &self.shared,
                        &self.sender,
                        request,
                        name,
                        arguments,
                        self.link.clone(),
                        &self.runtime,
                    );
                }
            }
            Event::Started { .. } => {
                let mut state = self.link.lock().unwrap();
                state.started = true;
                state.waker.wake();
            }
            Event::Returned { error, .. } => {
                let mut state = self.link.lock().unwrap();
                state.returned = Some(UnixMs::now());
                if let Some(error) = &error {
                    state.error = true;
                    state.say(error, true);
                }
                state.completion = Some(crate::PythonCompletion {
                    at: state.returned.unwrap(),
                    failed: error.is_some(),
                    produced_output: state.meaningful,
                    dispatched: state.dispatched,
                    set_checkin: state.checkin.is_some(),
                });
                state.returned_error = error;
                state.waker.wake();
            }
            Event::Text {
                text, important, ..
            } => self.link.lock().unwrap().write(&text, important),
            Event::Checkin {
                seconds,
                wake_on_tools,
                ..
            } => {
                let mut state = self.link.lock().unwrap();
                state.checkin = Some(crate::PythonCheckin {
                    after: std::time::Duration::from_secs(seconds),
                    wake_on_tools,
                });
                state.waker.wake();
            }
            Event::Finished { error, .. } => {
                let mut state = self.link.lock().unwrap();
                if let Some(error) = error {
                    let error = state
                        .returned_error
                        .as_ref()
                        .and_then(|root| error.strip_prefix(root))
                        .unwrap_or(&error)
                        .trim_start_matches('\n')
                        .to_owned();
                    if !error.is_empty() {
                        state.error = true;
                        state.say(&error, true);
                    }
                }
                state.finished = Some(UnixMs::now());
                if state.returned.is_none() {
                    state.returned = state.finished;
                }
                state.waker.wake();
            }
            Event::Stopped { error } => {
                let mut state = self.link.lock().unwrap();
                state.error = true;
                state.say(error.as_deref().unwrap_or("Python runtime stopped"), true);
                state.finished = Some(UnixMs::now());
                state.returned = state.finished;
                state.cancelled.send_replace(true);
                state.waker.wake();
            }
        }
    }
}
impl PythonCell {
    fn render(&mut self, first: bool) -> Option<ToolOutput> {
        let mut cell = self.link.lock().unwrap();
        let mut chunks = Vec::new();
        if !cell.output.is_empty() {
            chunks.push(decode_output_lossy(
                std::mem::replace(&mut cell.output, BoundedOutput::for_tokens(Some(10000)))
                    .into_bytes(),
            ));
        }
        cell.since = None;
        cell.important = None;
        cell.completion = None;
        let mut sources = Vec::new();
        let old = self.shared.next_cell.load(Ordering::Relaxed) > self.cell + 2;
        cell.operations.retain(|op| {
            let mut op = op.lock().unwrap();
            if op.finished.is_some() {
                let output = decode_output_lossy(
                    std::mem::replace(&mut op.output, BoundedOutput::for_tokens(Some(10000)))
                        .into_bytes(),
                );
                let mut parts = Vec::new();
                if op.announced {
                    parts.push(format!("Session ID: {}", session_id(op.id)));
                }
                parts.push(format!("Operation {} completed", op.name));
                if !output.is_empty() {
                    parts.push(format!("Output:\n{output}"));
                }
                sources.push((op.id, parts.join("\n")));
                false
            } else {
                if !op.announced {
                    op.announced = true;
                    sources.push((
                        op.id,
                        format!(
                            "Operation {} running with session ID {}",
                            op.name,
                            session_id(op.id)
                        ),
                    ));
                }
                true
            }
        });
        cell.jobs.retain(|job| {
            let mut state = job.state.lock().unwrap();
            let has_output = !state.unsent.is_empty();
            let finished = state.finished.clone();
            let announce = finished.is_none() && !state.announced;
            if !has_output && finished.is_none() && !announce {
                return true;
            }
            let mut parts = Vec::new();
            if let Some((_, summary)) = &finished {
                if state.announced {
                    parts.push(format!("Session ID: {}", session_id(job.id)));
                }
                if let Some(exit_code) = summary.get("exit_code") {
                    parts.push(format!("Process exited with code {exit_code}"));
                } else if let Some(error) = summary.get("error").and_then(Value::as_str) {
                    parts.push(format!("Command failed: {error}"));
                }
            } else {
                state.announced = true;
                parts.push(format!(
                    "Command running with session ID {}",
                    session_id(job.id)
                ));
            }
            if old {
                parts.push(format!("Command: {}", job.name));
            }
            if has_output {
                let output = decode_output_lossy(
                    std::mem::replace(
                        &mut state.unsent,
                        BoundedOutput::for_tokens(Some(job.budget)),
                    )
                    .into_bytes(),
                );
                parts.push(format!("Output:\n{output}"));
            }
            state.since = None;
            state.important = None;
            sources.push((job.id, parts.join("\n")));
            if finished.is_some() {
                state.delivered = true;
                false
            } else {
                true
            }
        });
        sources.sort_by_key(|(id, _)| *id);
        chunks.extend(sources.into_iter().map(|(_, text)| text));
        if cell.closed() && !cell.delivered {
            cell.delivered = true;
            if chunks.is_empty() {
                chunks.push("Execution completed.".into());
            }
        } else if first && chunks.is_empty() {
            chunks.push("No output yet. Output and completion arrive automatically.".into());
        }
        if chunks.is_empty() {
            return None;
        }
        let mut result = output(
            chunks.join("\n"),
            if *cell.cancelled.borrow() {
                ToolOutputStatus::Cancelled
            } else if cell.error {
                ToolOutputStatus::Error
            } else {
                ToolOutputStatus::Success
            },
        );
        result.images = Arc::new(std::mem::take(&mut cell.images));
        Some(result)
    }
}
impl ToolSession for PythonCell {
    fn sources(&self) -> Vec<(u64, crate::SourceFacts)> {
        use crate::{PythonOperationFacts, PythonOutput, SourceFacts};
        // Keep the exec marker distinct from zero-based host request IDs.
        let mut sources = vec![(u64::MAX, SourceFacts::PythonExec(self.facts()))];
        let cell = self.link.lock().unwrap();
        sources.extend(cell.jobs.iter().map(|job| {
            let state = job.state.lock().unwrap();
            (
                job.id,
                SourceFacts::PythonOperation(PythonOperationFacts {
                    finished: state.finished.as_ref().map(|(at, _)| *at),
                    output: PythonOutput {
                        since: state.since,
                        notification: state.important,
                    },
                }),
            )
        }));
        sources.extend(cell.operations.iter().map(|operation| {
            let operation = operation.lock().unwrap();
            (
                operation.id,
                SourceFacts::PythonOperation(PythonOperationFacts {
                    finished: operation.finished,
                    output: PythonOutput::default(),
                }),
            )
        }));
        sources
    }
    fn python_exec(&self) -> Option<Arc<PythonExec>> {
        Some(self.0.clone())
    }
    fn done(&self) -> bool {
        self.link.lock().unwrap().delivered
    }
    fn first_output(&mut self) -> ToolOutput {
        self.render(true).unwrap()
    }
    fn more_output(&mut self) -> Option<ToolOutput> {
        self.render(false)
    }
    fn cancel(&mut self) {
        self.sender.cancel(self.cell);
        self.link.lock().unwrap().cancelled.send_replace(true);
        for job in &self.link.lock().unwrap().jobs {
            job.cancel.notify_one();
        }
    }
}
impl Drop for PythonCell {
    fn drop(&mut self) {
        if !self.done() {
            self.cancel();
        }
        self.shared.cells.lock().unwrap().remove(&self.cell);
    }
}

#[cfg(test)]
mod session_id_tests {
    use super::*;

    #[tokio::test]
    async fn background_session_ids_stay_stable_and_old_previews_are_bounded() {
        let tool = PythonTool::new(
            ShellTools::in_directory(
                std::time::Duration::from_secs(5),
                "/tmp".into(),
                rho_workspaces::PathOverrides::default(),
            ),
            Vec::new(),
        )
        .unwrap();
        let wake = Arc::new(Notify::new());
        let mut cell = tool.run(
            ToolCall {
                id: "session-label-test".try_into().unwrap(),
                name: "exec".try_into().unwrap(),
                tool_type: ToolType::Custom,
                arguments: "command('sleep 600 # ☃☃☃☃☃☃☃☃')".into(),
            },
            SourceWaker::new(wake.clone()),
        );
        let exec = cell.python_exec().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while exec.facts().returned.is_none() {
                wake.notified().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !exec.link.lock().unwrap().jobs[0]
                .state
                .lock()
                .unwrap()
                .announced
        );
        let first = cell.first_output();
        let id = first
            .output
            .rsplit(' ')
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert_eq!(id, session_id(0));
        assert!(!first.output.contains("Command:"));
        for call_id in ["newer-one", "newer-two"] {
            let mut newer = tool.run(
                ToolCall {
                    id: call_id.try_into().unwrap(),
                    name: "exec".try_into().unwrap(),
                    tool_type: ToolType::Custom,
                    arguments: "pass".into(),
                },
                SourceWaker::new(wake.clone()),
            );
            let exec = newer.python_exec().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !exec.quiescent() {
                    wake.notified().await;
                }
            })
            .await
            .unwrap();
            newer.first_output();
        }
        cell.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !exec.quiescent() {
                wake.notified().await;
            }
        })
        .await
        .unwrap();
        let completed = cell.more_output().unwrap();
        let preview = completed
            .output
            .lines()
            .find_map(|line| line.strip_prefix("Command: "))
            .unwrap();
        assert!(preview.len() <= 20);
        assert!(preview.ends_with("..."));
        assert!(cell.done());
        assert!(completed.output.contains(&format!("Session ID: {id}")));
    }

    #[test]
    fn session_id_permutation_covers_the_domain_and_wraps() {
        let mut ids = (0..9_000).map(session_id).collect::<Vec<_>>();
        for internal in 0..9_000 {
            assert_eq!(session_id(internal), session_id(internal + 9_000));
        }
        ids.sort_unstable();
        assert_eq!(ids, (1_000..10_000).collect::<Vec<_>>());
        assert_eq!(session_id(u64::MAX), session_id(u64::MAX % 9_000));
        assert_eq!(
            (0..5).map(session_id).collect::<Vec<_>>(),
            [1230, 2009, 5852, 6233, 8666],
        );
    }
}
