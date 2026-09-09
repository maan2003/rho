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
    next_cell: AtomicU64,
    spec: ToolSpec,
    runtime: tokio::runtime::Handle,
}
struct Shared {
    shell: ShellTools,
    others: HashMap<String, Arc<dyn FutureTool>>,
    cells: Mutex<HashMap<u64, Arc<Mutex<ExecState>>>>,
    jobs: Mutex<BTreeMap<u64, Arc<Job>>>,
    sequence: AtomicU64,
    images: Mutex<BTreeMap<u64, rho_core::ImageContent>>,
}
struct ExecState {
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
    patience: Option<u64>,
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
    finished: Option<UnixMs>,
}
struct Job {
    id: u64,
    state: Mutex<JobState>,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    cancel: Notify,
    budget: usize,
    ready: tokio::sync::watch::Sender<bool>,
}
struct JobState {
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
impl PythonTool {
    pub fn new(shell: ShellTools, others: Vec<Arc<dyn FutureTool>>) -> Result<Self, String> {
        let mut description = String::from(
            r#"Run raw Python in the persistent notebook.

Work registers immediately; output arrives automatically. Put independent calls in the same cell to run them concurrently; await only when later Python statements depend on completion.
- command(cmd, workdir=None, max_tokens=2000) returns a managed handle. Assignment and await are optional. Awaiting it returns completion metadata (id, exit_code), not stdout.
- write_stdin(handle, chars='', max_tokens=2000) registers an input write and retained-output read. It waits for stdin readiness, not subsequent output; an empty page is valid. Await only if Python needs the returned page or write completion. display(handle) also reads a page.
- tools.NAME(**kwargs) registers an internal tool call. Custom tools take a single string. Await only for Python dependencies; text results are strings, JSON results are parsed values. Tool schemas follow the examples.

Python output: print(...) and text(value, max_tokens=2000) are ordinary output; notify(value, max_tokens=2000) is meaningful output that wakes sooner. image((await tools.view_image(path=...))["content"][0]) displays a returned image reference.

Examples

Run independent inspections concurrently in one exec—no gather, await, or result-printing:
```python
command("git diff --stat")
command("rg -n 'TODO' src")
```

Internal tools register the same way:
```python
tools.apply_patch("""*** Begin Patch
*** Update File: config.toml
@@
-retries = 2
+retries = 3
*** End Patch""")
```

Search the web without awaiting or reprinting the result:
```python
tools.web__run(search_query=[{"q": "Python asyncio TaskGroup documentation"}])
```
Use the returned references in a later cell for tools.web__run(open=[...]).

When an edit is already prepared in `patch`, await only the dependencies:
```python
await tools.apply_patch(patch)
check = await command("cargo check")
if check["exit_code"] == 0:
    command("cargo test")
```

Use Python directly to manipulate file data:
```python
config = Path("config.toml")
config.write_text(config.read_text().replace("retries = 2", "retries = 3"))
```

Send stdin without blocking the notebook:
```python
job = command("python3 -c 'print(input())'", max_tokens=100)
write_stdin(job, "hello\n")
```
After automatic completion, a later cell can expand retained output:
```python
write_stdin(job, max_tokens=6000)
```

A live monitoring cell:
```python
progress = {"checks": 0}
while not Path("results.json").exists():
    progress["checks"] += 1
    await asyncio.sleep(5)
notify("Results are ready")
```
Inspect its globals in a later cell without stopping it:
```python
text(progress)
```
The default check-in interval is 120 seconds. Omit set_patience unless you want
a significantly shorter or longer interval. For a significantly longer interval, set it alongside the work:
```python
command("git diff --check")
command("cargo test")
set_patience(seconds=300)
```
Then end the model turn. No separate exec is needed; this does not sleep or block Python.
If the cell also awaits a dependency, set patience before that await; a suspended cell
may resume after its originating model turn has ended.

Details and limits
- Explicit output reads have a separate cursor starting at byte 0, so they can repeat automatic previews. Wait for command completion only if Python needs a complete final read.
- Budgets are capped at 10000 tokens. Each command retains its first 8 MiB with explicit overflow counts. Up to 64 handles are retained; oldest completed, delivered handles may be evicted. Up to 32 image references are retained.
- set_patience accepts 1..3600 seconds and overrides only this turn's default 120-second interval. Meaningful events wake earlier; old cells cannot change a newer turn's patience.
- Python is in-process, not a sandbox. Cwd is private to the notebook; other process-global APIs retain normal semantics. Native extension packages are unsupported.

Available tools:
"#,
        );
        for spec in shell
            .specs()
            .into_iter()
            .filter(|s| s.name.as_str() != "exec_command" && s.name.as_str() != "write_stdin")
            .chain(others.iter().map(|t| t.spec()))
        {
            description.push_str(&format!(
                "tools.{}: {}\nArguments: {}\n",
                spec.name.as_str(),
                spec.description,
                spec.input_schema
            ));
        }
        let shared = Arc::new(Shared {
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
        let session = Session::new(move || {
            unsafe { setup_runtime.block_on(shell.enter_interpreter_thread()) }
                .map_err(|error| error.to_string())
        })?;
        Ok(Self {
            session,
            shared,
            next_cell: AtomicU64::new(1),
            spec: ToolSpec {
                name: ToolName::try_from("exec").unwrap(),
                tool_type: ToolType::Custom,
                description,
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
        let cell = self.next_cell.fetch_add(1, Ordering::Relaxed);
        let link = Arc::new(Mutex::new(ExecState {
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
            patience: None,
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
        if let Err(error) = self
            .session
            .sender()
            .execute(cell, call.arguments, exec.clone())
        {
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
                state: Mutex::new(JobState {
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
                Ok(None) => host_call(&shared, &name, args, &link).await,
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
            link.lock().unwrap().say(error, true);
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
        cell.say("Image displayed", false);
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
        link.lock().unwrap().say(&result.to_string(), false);
        return Ok(result);
    }
    let other = shared.others.get(name);
    let spec = other
        .map(|t| t.spec())
        .or_else(|| {
            shared.shell.specs().into_iter().find(|s| {
                s.name.as_str() == name && name != "exec_command" && name != "write_stdin"
            })
        })
        .ok_or_else(|| format!("Unknown tool: {name}"))?;
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
    let result = match other {
        Some(tool) => tool.call(call).await,
        None => shared.shell.call(call).await,
    };
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
        link.lock().unwrap().say(&result.output, false);
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
            patience: state.patience.map(std::time::Duration::from_secs),
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
                    set_patience: state.patience.is_some(),
                });
                state.returned_error = error;
                state.waker.wake();
            }
            Event::Text {
                text, important, ..
            } => self.link.lock().unwrap().write(&text, important),
            Event::Patience { seconds, .. } => {
                let mut state = self.link.lock().unwrap();
                state.patience = Some(seconds);
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
        cell.operations
            .retain(|op| op.lock().unwrap().finished.is_none());
        cell.jobs.retain(|job| {
            let mut state = job.state.lock().unwrap();
            if !state.unsent.is_empty() {
                chunks.push(format!(
                    "Command {} output:\n{}",
                    job.id,
                    decode_output_lossy(
                        std::mem::replace(
                            &mut state.unsent,
                            BoundedOutput::for_tokens(Some(job.budget))
                        )
                        .into_bytes()
                    )
                ));
            }
            state.since = None;
            state.important = None;
            if let Some((_, summary)) = state.finished.clone() {
                chunks.push(format!("Command completed: {summary}"));
                state.delivered = true;
                false
            } else {
                true
            }
        });
        if cell.closed() && !cell.delivered {
            cell.delivered = true;
            if chunks.is_empty() {
                chunks.push("Execution completed.".into());
            }
        } else if first {
            if cell.jobs.is_empty() {
                chunks.push("Execution is still running; output arrives automatically.".into());
            } else {
                chunks.push(format!(
                    "Commands still running: {}. Output arrives automatically; do not rerun them.",
                    cell.jobs
                        .iter()
                        .map(|job| job.id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
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
        let mut sources = vec![(0, SourceFacts::PythonExec(self.facts()))];
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
