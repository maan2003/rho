//! The workset's Python ([ARCH-rho2](../../../specs/ARCH-rho2.md)): the
//! `_rho` module policy code calls, the workset thread that runs
//! `rho._boot._main`, and the hooks the agent loops call through
//! `rho._boot._hook`.
//!
//! `_rho` asks the agent host for everything it does, over the workset
//! port, so its calls take the same path whether or not the agent they name
//! is loaded. The contract is `rho/_rho.pyi`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule};
use rho_agent_types::{
    Address, AgentId, ContentPart, InputWake, Mail, MailId, SendKind, ThreadId, WorksetId,
};
use tokio::sync::oneshot;

use crate::inference::PromptCacheKey;
use crate::inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use crate::ipc::protocol::Control;
use crate::ipc::transport::{Port, Sender};
use crate::ipc::workset::{Message, PyReply, PyRequest, encode};
use crate::record::{AgentSpec, Runtime};

/// How long a hook may take before the loop goes on without it.
const HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// How `_rho` reaches the agent host: over the workset port, or a test's
/// stand-in.
pub(crate) type HostCall = Arc<
    dyn Fn(PyRequest) -> futures::future::BoxFuture<'static, anyhow::Result<PyReply>> + Send + Sync,
>;

/// The workset process's link to the agent host for `_rho`.
pub(crate) struct PyHost {
    pub call: HostCall,
    pub runtime: tokio::runtime::Handle,
    pub workset: WorksetId,
    pub isolated: bool,
}

static HOST: OnceLock<PyHost> = OnceLock::new();

/// `_rho` calls waiting for the host's answer, by id.
static PENDING: LazyLock<Mutex<HashMap<u64, oneshot::Sender<PyReply>>>> =
    LazyLock::new(Mutex::default);
static NEXT: AtomicU64 = AtomicU64::new(1);

impl PyHost {
    /// `_rho` over the workset port: each call a `Python` message, each
    /// answer a `PythonReply` handed to [`replied`].
    pub fn over(
        sender: Sender,
        runtime: tokio::runtime::Handle,
        workset: WorksetId,
        isolated: bool,
    ) -> Self {
        let call: HostCall = Arc::new(move |request| {
            let sender = sender.clone();
            Box::pin(async move {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                let (reply, answer) = oneshot::channel();
                PENDING.lock().expect("poison").insert(id, reply);
                let sent = sender
                    .send(Port::Workset, encode(&Message::Python { id, request })?)
                    .await;
                if let Err(error) = sent {
                    PENDING.lock().expect("poison").remove(&id);
                    return Err(error.into());
                }
                answer
                    .await
                    .map_err(|_| anyhow::anyhow!("the agent host went away"))
            })
        });
        Self {
            call,
            runtime,
            workset,
            isolated,
        }
    }

    async fn call(&self, request: PyRequest) -> anyhow::Result<PyReply> {
        match (self.call)(request).await? {
            PyReply::Error(error) => Err(anyhow::anyhow!(error)),
            reply => Ok(reply),
        }
    }

    /// Block this Python thread, without the GIL, on a host call.
    fn block(py: Python<'_>, request: PyRequest) -> PyResult<PyReply> {
        let host = host()?;
        py.detach(|| host.runtime.block_on(host.call(request)))
            .map_err(|error| PyRuntimeError::new_err(format!("{error:#}")))
    }
}

fn host() -> PyResult<&'static PyHost> {
    HOST.get()
        .ok_or_else(|| PyRuntimeError::new_err("_rho: no agent host"))
}

/// The host answered the `_rho` call `id`.
pub(crate) fn replied(id: u64, body: PyReply) {
    if let Some(reply) = PENDING.lock().expect("poison").remove(&id) {
        let _ = reply.send(body);
    }
}

/// Install `_rho`, then start the workset thread: it runs the default
/// policy and `init`, and then the workset loop for the life of the
/// process. A failing `init.py` is reported on stderr; the default policy
/// stays.
pub(crate) fn start(host: PyHost, init: Option<camino::Utf8PathBuf>) -> anyhow::Result<()> {
    HOST.set(host)
        .map_err(|_| anyhow::anyhow!("the workset Python started twice"))?;
    rho_notebook::start_python().map_err(anyhow::Error::msg)?;
    Python::attach(|py| -> PyResult<()> {
        let module = module(py)?;
        py.import("sys")?
            .getattr("modules")?
            .set_item("_rho", module)?;
        Ok(())
    })
    .map_err(|error| anyhow::anyhow!("install _rho: {error}"))?;
    std::thread::Builder::new()
        .name("rho-workset".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let result = Python::attach(|py| -> PyResult<()> {
                let ready = pyo3::types::PyCFunction::new_closure(
                    py,
                    None,
                    None,
                    |args, _| -> PyResult<()> {
                        let error: Option<String> = args.get_item(0)?.extract()?;
                        if let Some(error) = error {
                            eprintln!("rho: init.py failed; the default policy runs:\n{error}");
                        }
                        Ok(())
                    },
                )?;
                py.import("rho._boot")?
                    .call_method1("_main", (init.as_ref().map(|path| path.as_str()), ready))?;
                Ok(())
            });
            if let Err(error) = result {
                Python::attach(|py| {
                    eprintln!("rho: the workset loop stopped: {}", traceback(py, &error))
                });
            }
        })?;
    // Hooks run on the workset loop: wait until it exists, not until
    // init.py is done, which may take long and may call back into the host.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let running = Python::attach(|py| -> PyResult<bool> {
            Ok(!py.import("rho._boot")?.getattr("_loop")?.is_none())
        })
        .map_err(|error| anyhow::anyhow!("start the workset loop: {error}"))?;
        if running {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the workset loop did not start"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn traceback(py: Python<'_>, error: &PyErr) -> String {
    py.import("traceback")
        .and_then(|traceback| {
            traceback
                .call_method1("format_exception", (error.value(py),))?
                .extract::<Vec<String>>()
        })
        .map_or_else(|_| error.to_string(), |lines| lines.concat())
}

/// Run `code` in the workset namespace: what it printed, and its traceback.
pub(crate) async fn eval(code: String) -> anyhow::Result<(String, Option<String>)> {
    tokio::task::spawn_blocking(move || {
        Python::attach(|py| {
            py.import("rho._boot")?
                .call_method1("_eval", (code,))?
                .extract::<(String, Option<String>)>()
        })
        .map_err(|error| Python::attach(|py| anyhow::anyhow!(traceback(py, &error))))
    })
    .await?
}

/// Call `hooks.<name>(agent, *args)` on the workset loop.
async fn hook<T: Send + 'static>(
    name: &'static str,
    agent: AgentId,
    args: impl for<'py> FnOnce(Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> + Send + 'static,
    extract: impl for<'py> FnOnce(Bound<'py, PyAny>) -> PyResult<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(move || {
        Python::attach(|py| {
            let mut all = vec![
                name.into_pyobject(py)?.into_any(),
                agent.encoded().into_pyobject(py)?.into_any(),
            ];
            all.extend(args(py)?);
            let kwargs = PyDict::new(py);
            kwargs.set_item("timeout", HOOK_TIMEOUT.as_secs_f64())?;
            let result = py
                .import("rho._boot")?
                .getattr("_hook")?
                .call(pyo3::types::PyTuple::new(py, all)?, Some(&kwargs))?;
            extract(result)
        })
        .map_err(|error: PyErr| {
            Python::attach(|py| anyhow::anyhow!("hooks.{name}: {}", traceback(py, &error)))
        })
    })
    .await?
}

/// `hooks.instructions`: the model's instructions for its next request.
pub(crate) async fn instructions(agent: AgentId) -> anyhow::Result<String> {
    hook(
        "instructions",
        agent,
        |_| Ok(Vec::new()),
        |value| value.extract(),
    )
    .await
}

/// `hooks.on_compaction`: what the model is given back after its context
/// was compacted, as quiet inputs.
pub(crate) async fn on_compaction(agent: AgentId) -> anyhow::Result<Vec<String>> {
    hook(
        "on_compaction",
        agent,
        |_| Ok(Vec::new()),
        |value| {
            if value.is_none() {
                return Ok(Vec::new());
            }
            value.extract()
        },
    )
    .await
}

/// Why a model's turn ended, for `hooks.on_turn_end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    EndTurn,
    Archived,
    Failed,
}

/// `hooks.on_turn_end`. Its failure is the policy's: it is reported and the
/// loop goes on.
pub(crate) async fn on_turn_end(agent: AgentId, end: TurnEnd, error: Option<String>) {
    let reason = match end {
        TurnEnd::EndTurn => "end_turn",
        TurnEnd::Archived => "archived",
        TurnEnd::Failed => "failed",
    };
    let result = hook(
        "on_turn_end",
        agent,
        move |py| {
            let outcome = PyDict::new(py);
            outcome.set_item("reason", reason)?;
            outcome.set_item("error", error)?;
            Ok(vec![outcome.into_any()])
        },
        |_| Ok(()),
    )
    .await;
    if let Err(error) = result {
        eprintln!("rho: {error:#}");
    }
}

/// What a new notebook of `agent` runs first, on its thread: it is that
/// agent's (`current_agent`), and `hooks.on_notebook` fills its globals.
pub(crate) fn on_notebook(agent: AgentId) -> rho_notebook::OnStart {
    Box::new(move |py, namespace| {
        let id = agent.encoded().to_string();
        py.import("_rho")?
            .getattr("_current")?
            .call_method1("set", (id.as_str(),))?;
        py.import("rho._boot")?
            .call_method1("_on_notebook", (id, namespace))?;
        Ok(())
    })
}

// -- `_rho` ------------------------------------------------------------------

fn module(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    let module = PyModule::new(py, "_rho")?;
    module.add(
        "_current",
        py.import("contextvars")?.getattr("ContextVar")?.call(
            ("rho_current_agent",),
            Some(&[("default", py.None())].into_py_dict(py)?),
        )?,
    )?;
    module.add_function(pyo3::wrap_pyfunction!(workset_id, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(workset_kind, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(clone, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(context, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(models, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(create_agent, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(agents, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(set_spec, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(input, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(delivered, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(cancel, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(compact, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(rewind, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(new_cache_key, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(send_mail, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(inbox, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(newest_mail_between, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(current_agent, &module)?)?;
    Ok(module)
}

use pyo3::types::IntoPyDict as _;

fn unexpected() -> PyErr {
    PyRuntimeError::new_err("_rho: unexpected reply from the agent host")
}

fn agent_id(text: &str) -> PyResult<AgentId> {
    AgentId::from_encoded(text)
        .map_err(|_| PyValueError::new_err(format!("not an agent id: {text:?}")))
}

fn address(text: &str) -> PyResult<Address> {
    if text == "user" {
        Ok(Address::User)
    } else {
        agent_id(text).map(Address::Agent)
    }
}

fn address_text(address: Address) -> String {
    match address {
        Address::User => "user".to_owned(),
        Address::Agent(agent) => agent.encoded().to_string(),
    }
}

fn text(content: &[ContentPart]) -> String {
    rho_agent_types::transcript::text_content(content)
}

#[pyfunction]
fn workset_id() -> PyResult<String> {
    Ok(host()?.workset.to_string())
}

#[pyfunction]
fn workset_kind() -> PyResult<&'static str> {
    Ok(if host()?.isolated { "isolated" } else { "host" })
}

#[pyfunction]
#[pyo3(signature = (url, dest, rev = String::new()))]
fn clone(py: Python<'_>, url: String, dest: String, rev: String) -> PyResult<()> {
    let dest = std::path::absolute(&dest)
        .ok()
        .and_then(|dest| camino::Utf8PathBuf::try_from(dest).ok())
        .ok_or_else(|| PyValueError::new_err(format!("not a path: {dest:?}")))?;
    match PyHost::block(py, PyRequest::Clone { url, dest, rev })? {
        PyReply::Done => Ok(()),
        _ => Err(unexpected()),
    }
}

/// AGENTS.md files and skills for `cwd`. A checkout's root counts where the
/// directory is: inside the workset only within it, and a host directory's
/// wherever its checkout begins.
#[pyfunction]
fn context(py: Python<'_>, cwd: String) -> PyResult<Bound<'_, PyDict>> {
    let cwd = camino::Utf8PathBuf::from(cwd);
    let found = py.detach(|| {
        let (root, _) = rho_fs_view::resolve_workdir_root(cwd.as_std_path()).ok()?;
        let in_workset = cwd.starts_with(rho_fs_view::MOUNT_ROOT);
        let root = if !in_workset || root.starts_with(rho_fs_view::MOUNT_ROOT) {
            root
        } else {
            cwd.clone()
        };
        let found = rho_context_config::DiscoveredContext::discover(&root, &root);
        for diagnostic in &found.diagnostics {
            eprintln!(
                "rho: context config {:?}: {}: {}",
                diagnostic.kind,
                diagnostic.path.display(),
                diagnostic.message
            );
        }
        Some(found)
    });
    let out = PyDict::new(py);
    let files = PyList::empty(py);
    let skills = PyList::empty(py);
    if let Some(found) = found {
        for file in found.agents_files {
            let entry = PyDict::new(py);
            entry.set_item("path", file.file_path.as_str())?;
            entry.set_item("content", file.content)?;
            files.append(entry)?;
        }
        for skill in found.skills {
            let entry = PyDict::new(py);
            entry.set_item("name", skill.name)?;
            entry.set_item("description", skill.description)?;
            entry.set_item("path", skill.file_path.as_str())?;
            skills.append(entry)?;
        }
    }
    out.set_item("agents_files", files)?;
    out.set_item("skills", skills)?;
    Ok(out)
}

#[pyfunction]
fn models(py: Python<'_>) -> PyResult<Vec<Bound<'_, PyDict>>> {
    match PyHost::block(py, PyRequest::Models)? {
        PyReply::Models(runtimes) => runtimes
            .iter()
            .map(|runtime| runtime_to_py(py, runtime))
            .collect(),
        _ => Err(unexpected()),
    }
}

#[pyfunction]
fn create_agent(py: Python<'_>, spec: Bound<'_, PyDict>) -> PyResult<String> {
    let spec = spec_from_py(&spec, None)?;
    match PyHost::block(py, PyRequest::CreateAgent(spec))? {
        PyReply::Agent(agent) => Ok(agent.encoded().to_string()),
        _ => Err(unexpected()),
    }
}

fn all_agents(py: Python<'_>) -> PyResult<Vec<(AgentId, AgentSpec)>> {
    match PyHost::block(py, PyRequest::Agents)? {
        PyReply::Agents(agents) => Ok(agents),
        _ => Err(unexpected()),
    }
}

#[pyfunction]
fn agents(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let out = PyDict::new(py);
    for (agent, spec) in all_agents(py)? {
        out.set_item(agent.encoded().to_string(), spec_to_py(py, &spec)?)?;
    }
    Ok(out)
}

#[pyfunction]
fn set_spec(py: Python<'_>, agent: &str, spec: Bound<'_, PyDict>) -> PyResult<()> {
    let agent = agent_id(agent)?;
    let current = all_agents(py)?
        .into_iter()
        .find(|(id, _)| *id == agent)
        .map(|(_, spec)| spec)
        .ok_or_else(|| PyValueError::new_err(format!("no agent {}", agent.encoded())))?;
    let spec = spec_from_py(&spec, Some(&current))?;
    match PyHost::block(py, PyRequest::SetSpec { agent, spec })? {
        PyReply::Done => Ok(()),
        _ => Err(unexpected()),
    }
}

fn control(py: Python<'_>, agent: &str, control: Control) -> PyResult<()> {
    let agent = agent_id(agent)?;
    match PyHost::block(py, PyRequest::Control { agent, control })? {
        PyReply::Done => Ok(()),
        _ => Err(unexpected()),
    }
}

#[pyfunction]
#[pyo3(signature = (agent, text, *, wake, mail = None))]
fn input(py: Python<'_>, agent: &str, text: String, wake: &str, mail: Option<u64>) -> PyResult<()> {
    let wake = match wake {
        "person" => InputWake::Person,
        "agent" => InputWake::Agent,
        "quiet" => InputWake::Quiet,
        _ => {
            return Err(PyValueError::new_err(
                "wake is one of \"person\", \"agent\" or \"quiet\"",
            ));
        }
    };
    control(
        py,
        agent,
        Control::Input {
            content: vec![ContentPart::Text { text }],
            wake,
            mail: mail.map(MailId),
        },
    )
}

#[pyfunction]
fn delivered(py: Python<'_>, agent: &str) -> PyResult<u64> {
    match PyHost::block(py, PyRequest::Delivered(agent_id(agent)?))? {
        PyReply::Mail(id) => Ok(id.0),
        _ => Err(unexpected()),
    }
}

#[pyfunction]
fn newest_mail_between(py: Python<'_>, a: &str, b: &str) -> PyResult<Option<Py<PyAny>>> {
    let request = PyRequest::NewestMailBetween(address(a)?, address(b)?);
    match PyHost::block(py, request)? {
        PyReply::NewestMail(mail) => mail
            .map(|mail| Ok(mail_to_py(py, &mail)?.into_any().unbind()))
            .transpose(),
        _ => Err(unexpected()),
    }
}

#[pyfunction]
fn cancel(py: Python<'_>, agent: &str) -> PyResult<()> {
    control(py, agent, Control::Cancel)
}

#[pyfunction]
fn compact(py: Python<'_>, agent: &str) -> PyResult<()> {
    control(py, agent, Control::Compact)
}

#[pyfunction]
fn rewind(py: Python<'_>, agent: &str, turns: u32) -> PyResult<()> {
    control(py, agent, Control::Rewind(turns))
}

#[pyfunction]
fn new_cache_key(py: Python<'_>, agent: &str) -> PyResult<()> {
    control(py, agent, Control::CacheKey)
}

#[pyfunction]
#[pyo3(signature = (sender, to, text, *, kind = None, thread = None))]
fn send_mail(
    py: Python<'_>,
    sender: &str,
    to: &str,
    text: String,
    kind: Option<&str>,
    thread: Option<u64>,
) -> PyResult<u64> {
    let sender = agent_id(sender)?;
    let to = address(to)?;
    if text.trim().is_empty() {
        return Err(PyValueError::new_err("a message needs text"));
    }
    let kind = kind
        .map(|kind| match kind {
            "ask" => Ok(SendKind::Ask),
            "result" => Ok(SendKind::Result),
            "status" => Ok(SendKind::Status),
            "fyi" => Ok(SendKind::Fyi),
            _ => Err(PyValueError::new_err(
                "kind is one of \"ask\", \"result\", \"fyi\" or \"status\"",
            )),
        })
        .transpose()?;
    if to == Address::User {
        let Some(kind) = kind else {
            return Err(PyValueError::new_err("mail to the user needs a kind"));
        };
        if kind != SendKind::Status {
            let findings = crate::worker::ste::ste_findings(&text);
            if !findings.is_empty() {
                return Err(PyValueError::new_err(format!(
                    "not sent. Write the message in Simplified Technical English \
                     (ASD-STE100): fix these problems, then send it again.\n\n{}",
                    findings.join("\n\n")
                )));
            }
        }
    }
    let request = PyRequest::SendMail {
        sender,
        to,
        content: vec![ContentPart::Text { text }],
        kind,
        thread: thread.map(|thread| ThreadId(MailId(thread))),
    };
    match PyHost::block(py, request)? {
        PyReply::Mail(id) => Ok(id.0),
        _ => Err(unexpected()),
    }
}

fn mail_to_py<'py>(py: Python<'py>, mail: &Mail) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);
    out.set_item("id", mail.id.0)?;
    out.set_item("at", mail.at.0)?;
    out.set_item("sender", address_text(mail.from))?;
    out.set_item("to", address_text(mail.to))?;
    out.set_item("thread", mail.thread.0.0)?;
    out.set_item(
        "kind",
        mail.kind.map(|kind| match kind {
            SendKind::Ask => "ask",
            SendKind::Result => "result",
            SendKind::Status => "status",
            SendKind::Fyi => "fyi",
        }),
    )?;
    out.set_item("text", text(&mail.content))?;
    Ok(out)
}

/// Mail to `address` after `after`, as an awaitable of the caller's loop.
#[pyfunction]
fn inbox(py: Python<'_>, address: &str, after: u64) -> PyResult<Py<PyAny>> {
    let address = self::address(address)?;
    let host = host()?;
    let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
    let future = event_loop.call_method0("create_future")?;
    let (event_loop, settle) = (event_loop.unbind(), future.clone().unbind());
    host.runtime.spawn(async move {
        let reply = host
            .call(PyRequest::Inbox {
                address,
                after: MailId(after),
            })
            .await;
        Python::attach(|py| {
            let settle = settle.bind(py);
            let outcome = match reply {
                Ok(PyReply::Mails(mails)) => mails
                    .iter()
                    .map(|mail| mail_to_py(py, mail))
                    .collect::<PyResult<Vec<_>>>()
                    .and_then(|mails| Ok(PyList::new(py, mails)?.into_any())),
                Ok(_) => Err(unexpected()),
                Err(error) => Err(PyRuntimeError::new_err(format!("{error:#}"))),
            };
            let result = (|| -> PyResult<()> {
                let settle_now = py
                    .import("_rho")?
                    .getattr("_settle")
                    .or_else(|_| define_settle(py))?;
                let (value, error) = match outcome {
                    Ok(value) => (value, py.None().into_bound(py)),
                    Err(error) => (
                        py.None().into_bound(py),
                        error.into_value(py).into_bound(py).into_any(),
                    ),
                };
                event_loop
                    .bind(py)
                    .call_method1("call_soon_threadsafe", (settle_now, settle, value, error))?;
                Ok(())
            })();
            if let Err(error) = result {
                // The caller's loop has closed; nobody waits.
                let _ = error;
            }
        });
    });
    Ok(future.unbind())
}

/// `_rho._settle(future, value, error)`: resolve a future unless it was
/// cancelled, on the future's own loop.
fn define_settle(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    let code = c"def _settle(future, value, error):\n    if future.done():\n        return\n    if error is not None:\n        future.set_exception(error)\n    else:\n        future.set_result(value)\n";
    let helper = PyModule::from_code(py, code, c"rho_settle.py", c"rho_settle")?;
    let settle = helper.getattr("_settle")?;
    py.import("_rho")?.setattr("_settle", &settle)?;
    Ok(settle)
}

#[pyfunction]
fn current_agent(py: Python<'_>) -> PyResult<Option<String>> {
    py.import("_rho")?
        .getattr("_current")?
        .call_method0("get")?
        .extract()
}

// -- specs -------------------------------------------------------------------

fn runtime_to_py<'py>(py: Python<'py>, runtime: &Runtime) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);
    out.set_item(
        "kind",
        match runtime {
            Runtime::Native { .. } => "native",
            Runtime::Claude { .. } => "claude",
        },
    )?;
    out.set_item("model", runtime.model_name())?;
    out.set_item("effort", runtime.effort_name())?;
    if let Runtime::Native { profile, .. } = runtime {
        out.set_item("fast", profile.fast_mode)?;
    }
    Ok(out)
}

fn spec_to_py<'py>(py: Python<'py>, spec: &AgentSpec) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);
    out.set_item("runtime", runtime_to_py(py, &spec.runtime)?)?;
    out.set_item("props", spec.props.clone().into_py_dict(py)?)?;
    Ok(out)
}

fn item<'py, T: for<'a> pyo3::FromPyObject<'a, 'py>>(
    dict: &Bound<'py, PyDict>,
    key: &str,
) -> PyResult<T>
where
    for<'a> <T as pyo3::FromPyObject<'a, 'py>>::Error: Into<PyErr>,
{
    dict.get_item(key)?
        .ok_or_else(|| PyValueError::new_err(format!("spec needs {key:?}")))?
        .extract()
        .map_err(Into::into)
}

/// A spec as Python gives it. The runtime's identity (a native cache key,
/// a Claude session) is kept from `current` when the kind stays, and new
/// otherwise.
fn spec_from_py(spec: &Bound<'_, PyDict>, current: Option<&AgentSpec>) -> PyResult<AgentSpec> {
    let runtime: Bound<'_, PyDict> = item(spec, "runtime")?;
    let props: std::collections::BTreeMap<String, String> = match spec.get_item("props")? {
        Some(props) => props.extract()?,
        None => Default::default(),
    };
    let kind: String = item(&runtime, "kind")?;
    let model: String = item(&runtime, "model")?;
    let effort: String = item(&runtime, "effort")?;
    let runtime = match kind.as_str() {
        "native" => {
            let model = match model.as_str() {
                "sol" => InferenceModel::Gpt61Sol,
                "terra" => InferenceModel::Gpt61Terra,
                "astra" => InferenceModel::Gpt6Astra,
                _ => return Err(PyValueError::new_err(format!("no native model {model:?}"))),
            };
            let effort = match effort.as_str() {
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                "xhigh" => ReasoningEffort::Xhigh,
                _ => {
                    return Err(PyValueError::new_err(format!(
                        "no native effort {effort:?}"
                    )));
                }
            };
            let fast_mode = match runtime.get_item("fast")? {
                Some(fast) => fast.extract()?,
                None => false,
            };
            let cache_key = match current.map(|current| &current.runtime) {
                Some(Runtime::Native { cache_key, .. }) => *cache_key,
                _ => PromptCacheKey::generate(),
            };
            Runtime::Native {
                model,
                profile: InferenceProfile { effort, fast_mode },
                cache_key,
            }
        }
        "claude" => {
            let model = match model.as_str() {
                "fable" => rho_claude::Model::Fable,
                "opus" => rho_claude::Model::Opus,
                "sonnet" => rho_claude::Model::Sonnet,
                _ => return Err(PyValueError::new_err(format!("no Claude model {model:?}"))),
            };
            let effort = match effort.as_str() {
                "low" => rho_claude::Effort::Low,
                "medium" => rho_claude::Effort::Medium,
                "high" => rho_claude::Effort::High,
                "xhigh" => rho_claude::Effort::Xhigh,
                "max" => rho_claude::Effort::Max,
                _ => {
                    return Err(PyValueError::new_err(format!(
                        "no Claude effort {effort:?}"
                    )));
                }
            };
            let session_id = match current.map(|current| &current.runtime) {
                Some(Runtime::Claude { session_id, .. }) => *session_id,
                _ => uuid::Uuid::new_v4(),
            };
            Runtime::Claude {
                model,
                effort,
                session_id,
            }
        }
        _ => {
            return Err(PyValueError::new_err(
                "runtime kind is \"native\" or \"claude\"",
            ));
        }
    };
    Ok(AgentSpec { runtime, props })
}
