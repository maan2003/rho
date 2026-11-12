//! The workset's Python ([ARCH-agent-host](../../../specs/ARCH-agent-host.md)):
//! the `_rho` module policy code calls, the workset thread that runs
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
    Address, AgentId, ChatSeq, ContentPart, Mention, MentionKind, MessageId, Role, TopicId,
    WorksetId,
};
use tokio::sync::oneshot;

use crate::inference::PromptCacheKey;
use crate::inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use crate::ipc::protocol::Control;
use crate::ipc::transport::{Port, Sender};
use crate::ipc::workset::{Message, PyMoveTo, PyReply, PyRequest, encode};
use crate::record::{AgentSpec, Runtime};
use crate::worker::permissions::{self, PERMISSIONS, Permissions};
use crate::worker::slack::{self, Routes};

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
    /// The workset's props: its head's at start, then what Python set.
    pub props: Mutex<rho_agent_types::Props>,
    /// The workset's state directory, where `workset.redb` lives.
    pub state: camino::Utf8PathBuf,
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
        props: rho_agent_types::Props,
        state: camino::Utf8PathBuf,
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
            props: Mutex::new(props),
            state,
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
/// Starts the workset Python and returns once its loop runs, before init.py
/// ends. `ended` hears how init.py ended, once: its traceback if it failed,
/// or why the loop stopped before it could say.
pub(crate) fn start(
    host: PyHost,
    init: Option<camino::Utf8PathBuf>,
    ended: impl FnOnce(Option<String>) + Send + 'static,
) -> anyhow::Result<()> {
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
    type Ended = Box<dyn FnOnce(Option<String>) + Send>;
    let ended: Arc<Mutex<Option<Ended>>> = Arc::new(Mutex::new(Some(Box::new(ended))));
    let end = move |error| {
        if let Some(ended) = ended.lock().expect("poison").take() {
            ended(error);
        }
    };
    std::thread::Builder::new()
        .name("rho-workset".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let result = Python::attach(|py| -> PyResult<()> {
                let end = end.clone();
                let ready = pyo3::types::PyCFunction::new_closure(
                    py,
                    None,
                    None,
                    move |args, _| -> PyResult<()> {
                        let error: Option<String> = args.get_item(0)?.extract()?;
                        if let Some(error) = &error {
                            eprintln!("rho: init.py failed:\n{error}");
                        }
                        end(error);
                        Ok(())
                    },
                )?;
                py.import("rho._boot")?
                    .call_method1("_main", (init.as_ref().map(|path| path.as_str()), ready))?;
                Ok(())
            });
            let error = match result {
                Ok(()) => "the workset loop stopped".to_owned(),
                Err(error) => {
                    let error = Python::attach(|py| {
                        format!("the workset loop stopped: {}", traceback(py, &error))
                    });
                    eprintln!("rho: {error}");
                    error
                }
            };
            // Only if init.py had not ended: then the host still waits.
            end(Some(error));
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
/// was compacted, as `system` inputs.
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
    module.add_function(pyo3::wrap_pyfunction!(workset_props, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(set_workset_props, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(set_permissions, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(workset_permissions, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(clone, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(repo_context, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(skills_in, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(builtin_skills, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(agents_file, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(models, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(create_agent, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(runs, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(agents, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(set_spec, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(input, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(delivered, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(cancel, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(compact, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(rewind, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(new_cache_key, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(post, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(start_topic, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(join, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(leave, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(change_topic, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(move_messages, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(direct, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(resolve_agent, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(external_address, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(external_accept, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(external_send, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(external_contacts, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(message, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(read, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(topic, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(inbox, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(current_agent, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(slack_who_hears, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(slack_owner, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(slack_route, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(slack_unroute, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(slack_posted, &module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(slack_routes, &module)?)?;
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
fn workset_props() -> PyResult<rho_agent_types::Props> {
    Ok(host()?.props.lock().expect("poison").clone())
}

#[pyfunction]
fn set_workset_props(py: Python<'_>, props: rho_agent_types::Props) -> PyResult<()> {
    match PyHost::block(py, PyRequest::SetWorksetProps(props.clone()))? {
        PyReply::Done => {
            *host()?.props.lock().expect("poison") = props;
            Ok(())
        }
        _ => Err(unexpected()),
    }
}

#[pyfunction]
#[pyo3(signature = (mode, environment = None))]
fn set_permissions(mode: &str, environment: Option<Vec<String>>) -> PyResult<()> {
    let permissions = match (mode, environment) {
        ("bypass", None) => Permissions::Bypass,
        ("bypass", Some(_)) => {
            return Err(PyValueError::new_err("bypass takes no environment"));
        }
        ("auto", environment) => Permissions::Auto { environment },
        (other, _) => {
            return Err(PyValueError::new_err(format!(
                "permissions are \"bypass\" or \"auto\", not {other:?}"
            )));
        }
    };
    PERMISSIONS
        .set(permissions)
        .map_err(|error| PyValueError::new_err(format!("{error:#}")))
}

#[pyfunction]
fn workset_permissions() -> Option<&'static str> {
    PERMISSIONS.get().map(|permissions| match *permissions {
        Permissions::Bypass => "bypass",
        Permissions::Auto { .. } => "auto",
    })
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

fn print_diagnostics(diagnostics: &[rho_context_config::ContextDiagnostic]) {
    for diagnostic in diagnostics {
        eprintln!(
            "rho: context config {:?}: {}: {}",
            diagnostic.kind,
            diagnostic.path.display(),
            diagnostic.message
        );
    }
}

fn skills_to_py<'py>(
    py: Python<'py>,
    skills: Vec<rho_context_config::Skill>,
) -> PyResult<Bound<'py, PyList>> {
    let out = PyList::empty(py);
    for skill in skills {
        let entry = PyDict::new(py);
        entry.set_item("name", skill.name)?;
        entry.set_item("description", skill.description)?;
        entry.set_item("path", skill.file_path.as_str())?;
        out.append(entry)?;
    }
    Ok(out)
}

fn agents_file_to_py<'py>(
    py: Python<'py>,
    file: rho_context_config::AgentsFile,
) -> PyResult<Bound<'py, PyDict>> {
    let entry = PyDict::new(py);
    entry.set_item("path", file.file_path.as_str())?;
    entry.set_item("content", file.content)?;
    Ok(entry)
}

/// The AGENTS.md and skills of the repository `cwd` is in. A checkout's
/// root counts where the directory is: inside the workset only within it,
/// and a host directory's wherever its checkout begins.
#[pyfunction]
fn repo_context(py: Python<'_>, cwd: String) -> PyResult<Bound<'_, PyDict>> {
    let cwd = camino::Utf8PathBuf::from(cwd);
    let found = py.detach(|| {
        let (root, _) = rho_fs_view::resolve_workdir_root(cwd.as_std_path()).ok()?;
        let in_workset = cwd.starts_with(rho_fs_view::MOUNT_ROOT);
        let root = if !in_workset || root.starts_with(rho_fs_view::MOUNT_ROOT) {
            root
        } else {
            cwd.clone()
        };
        let found = rho_context_config::DiscoveredContext::discover_repo(&root, &root);
        print_diagnostics(&found.diagnostics);
        Some(found)
    });
    let out = PyDict::new(py);
    let files = PyList::empty(py);
    let (agents_files, skills) =
        found.map_or_else(Default::default, |found| (found.agents_files, found.skills));
    for file in agents_files {
        files.append(agents_file_to_py(py, file)?)?;
    }
    out.set_item("agents_files", files)?;
    out.set_item("skills", skills_to_py(py, skills)?)?;
    Ok(out)
}

/// The skills in one skills directory; none if it does not exist.
#[pyfunction]
fn skills_in(py: Python<'_>, directory: std::path::PathBuf) -> PyResult<Bound<'_, PyList>> {
    let (skills, diagnostics) = py.detach(|| rho_context_config::discover_skills(&directory));
    print_diagnostics(&diagnostics);
    skills_to_py(py, skills)
}

/// The skills this build of Rho bundles.
#[pyfunction]
fn builtin_skills(py: Python<'_>) -> PyResult<Bound<'_, PyList>> {
    let (skills, diagnostics) = py.detach(|| {
        rho_context_config::bundled_skills_dir()
            .map(|directory| rho_context_config::discover_skills(&directory))
            .unwrap_or_default()
    });
    print_diagnostics(&diagnostics);
    skills_to_py(py, skills)
}

/// One AGENTS.md-style file, under a repository's limits; None if absent.
#[pyfunction]
fn agents_file(py: Python<'_>, path: std::path::PathBuf) -> PyResult<Option<Bound<'_, PyDict>>> {
    let (file, diagnostics) = py.detach(|| rho_context_config::read_agents_file(&path));
    print_diagnostics(&diagnostics);
    file.map(|file| agents_file_to_py(py, file)).transpose()
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
    let permissions = PERMISSIONS.fix();
    permissions::admits(&permissions, matches!(spec.runtime, Runtime::Native { .. }))
        .map_err(|error| PyValueError::new_err(format!("{error:#}")))?;
    match PyHost::block(py, PyRequest::CreateAgent(spec))? {
        PyReply::Agent(agent) => Ok(agent.encoded().to_string()),
        _ => Err(unexpected()),
    }
}

/// Whether this run's permissions let `agent` run: an auto workset runs no
/// native agent, so nothing can hand one input.
#[pyfunction]
fn runs(py: Python<'_>, agent: &str) -> PyResult<bool> {
    let agent = agent_id(agent)?;
    let spec = all_agents(py)?
        .into_iter()
        .find(|(id, _)| *id == agent)
        .map(|(_, spec)| spec)
        .ok_or_else(|| PyValueError::new_err(format!("no agent {}", agent.encoded())))?;
    let native = matches!(spec.runtime, Runtime::Native { .. });
    Ok(permissions::admits(&PERMISSIONS.fix(), native).is_ok())
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
#[pyo3(signature = (agent, text, *, role, message = None))]
fn input(
    py: Python<'_>,
    agent: &str,
    text: String,
    role: &str,
    message: Option<u64>,
) -> PyResult<()> {
    let role = match role {
        "user" => Role::User,
        "tool" => Role::Tool,
        _ => {
            return Err(PyValueError::new_err("role is one of \"user\" or \"tool\""));
        }
    };
    control(
        py,
        agent,
        Control::Input {
            content: vec![ContentPart::Text { text }],
            role,
            message: message.map(|seq| MessageId(ChatSeq(seq))),
        },
    )
}

#[pyfunction]
fn delivered(py: Python<'_>, agent: &str) -> PyResult<u64> {
    match PyHost::block(py, PyRequest::Delivered(agent_id(agent)?))? {
        PyReply::Seq(seq) => Ok(seq.0),
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

fn mentions(py_mentions: Option<Vec<(String, String)>>) -> PyResult<Vec<Mention>> {
    py_mentions
        .unwrap_or_default()
        .into_iter()
        .map(|(to, kind)| {
            let kind = match kind.as_str() {
                "ask" => MentionKind::Ask,
                "result" => MentionKind::Result,
                "fyi" => MentionKind::Fyi,
                _ => {
                    return Err(PyValueError::new_err(
                        "a mention's kind is one of \"ask\", \"result\" or \"fyi\"",
                    ));
                }
            };
            Ok(Mention {
                to: address(&to)?,
                kind,
            })
        })
        .collect()
}

/// The content of an agent's message, refused when it mentions the user in
/// other than Simplified Technical English.
fn message_content(text: String, mentions: &[Mention]) -> PyResult<Vec<ContentPart>> {
    if text.trim().is_empty() {
        return Err(PyValueError::new_err("a message needs text"));
    }
    if mentions.iter().any(|mention| mention.to == Address::User) {
        let findings = crate::worker::ste::ste_findings(&text);
        if !findings.is_empty() {
            return Err(PyValueError::new_err(format!(
                "not sent. Write the message in Simplified Technical English \
                 (ASD-STE100): fix these problems, then send it again.\n\n{}",
                findings.join("\n\n")
            )));
        }
    }
    Ok(vec![ContentPart::Text { text }])
}

/// Post `text` in `topic`; `mentions` are `(address, kind)` pairs.
#[pyfunction]
#[pyo3(signature = (sender, topic, text, *, mentions = None))]
fn post(
    py: Python<'_>,
    sender: &str,
    topic: u64,
    text: String,
    mentions: Option<Vec<(String, String)>>,
) -> PyResult<u64> {
    let sender = agent_id(sender)?;
    let mentions = self::mentions(mentions)?;
    let content = message_content(text, &mentions)?;
    let request = PyRequest::Post {
        sender,
        topic: TopicId(ChatSeq(topic)),
        mentions,
        content,
    };
    match PyHost::block(py, request)? {
        PyReply::Seq(seq) => Ok(seq.0),
        _ => Err(unexpected()),
    }
}

#[pyfunction]
#[pyo3(signature = (sender, stream, name, text, *, mentions = None))]
fn start_topic(
    py: Python<'_>,
    sender: &str,
    stream: String,
    name: String,
    text: String,
    mentions: Option<Vec<(String, String)>>,
) -> PyResult<(u64, u64)> {
    let sender = agent_id(sender)?;
    let mentions = self::mentions(mentions)?;
    let content = message_content(text, &mentions)?;
    let request = PyRequest::StartTopic {
        sender,
        stream,
        name,
        mentions,
        content,
    };
    match PyHost::block(py, request)? {
        PyReply::Messages(messages) => match &messages[..] {
            [first] => Ok((first.topic.0.0, first.id.0.0)),
            _ => Err(unexpected()),
        },
        _ => Err(unexpected()),
    }
}

/// The one agent of the host whose id starts with `prefix`.
#[pyfunction]
fn resolve_agent(py: Python<'_>, prefix: &str) -> PyResult<String> {
    match PyHost::block(py, PyRequest::ResolveAgent(prefix.to_owned()))? {
        PyReply::Agent(agent) => Ok(agent.encoded().to_string()),
        _ => Err(unexpected()),
    }
}

/// The direct conversation of `sender` and `other`, started if needed.
#[pyfunction]
fn direct(py: Python<'_>, sender: &str, other: &str) -> PyResult<u64> {
    let request = PyRequest::Direct {
        sender: agent_id(sender)?,
        other: agent_id(other)?,
    };
    match PyHost::block(py, request)? {
        PyReply::Seq(seq) => Ok(seq.0),
        _ => Err(unexpected()),
    }
}

/// The agent's own address for agents on other hosts.
#[pyfunction]
fn external_address(py: Python<'_>, agent: &str) -> PyResult<String> {
    match PyHost::block(py, PyRequest::ExternalAddress(agent_id(agent)?))? {
        PyReply::Text(address) => Ok(address),
        _ => Err(unexpected()),
    }
}

/// The agent accepts an address on another host under a name.
#[pyfunction]
fn external_accept(py: Python<'_>, agent: &str, address: String, name: String) -> PyResult<()> {
    let request = PyRequest::ExternalAccept {
        agent: agent_id(agent)?,
        peer: address,
        name,
    };
    match PyHost::block(py, request)? {
        PyReply::Done => Ok(()),
        _ => Err(unexpected()),
    }
}

/// The agent sends a message to an address it accepted, as an awaitable
/// of the caller's loop: the name it gave the address, once that address's
/// host took the message.
#[pyfunction]
fn external_send(
    py: Python<'_>,
    agent: &str,
    address: String,
    text: String,
) -> PyResult<Py<PyAny>> {
    let request = PyRequest::ExternalSend {
        agent: agent_id(agent)?,
        peer: address,
        body: text,
    };
    later(py, request, |py, reply| match reply {
        PyReply::Text(name) => Ok(name.into_pyobject(py)?.into_any()),
        _ => Err(unexpected()),
    })
}

/// The addresses the agent accepted, each a dict.
#[pyfunction]
fn external_contacts(py: Python<'_>, agent: &str) -> PyResult<Py<PyAny>> {
    match PyHost::block(py, PyRequest::ExternalContacts(agent_id(agent)?))? {
        PyReply::ExternalContacts(contacts) => {
            let out = pyo3::types::PyList::empty(py);
            for contact in contacts {
                let held = PyDict::new(py);
                held.set_item("address", contact.peer.to_string())?;
                held.set_item("name", contact.name)?;
                out.append(held)?;
            }
            Ok(out.into_any().unbind())
        }
        _ => Err(unexpected()),
    }
}

/// One message as a dict, or None.
#[pyfunction]
fn message(py: Python<'_>, id: u64) -> PyResult<Option<Py<PyAny>>> {
    match PyHost::block(py, PyRequest::Message(MessageId(ChatSeq(id))))? {
        PyReply::Messages(messages) => messages
            .first()
            .map(|message| Ok(message_to_py(py, message)?.into_any().unbind()))
            .transpose(),
        _ => Err(unexpected()),
    }
}

/// A conversation's `last` newest messages, oldest first.
#[pyfunction]
fn read(py: Python<'_>, topic: u64, last: u64) -> PyResult<Py<PyAny>> {
    let request = PyRequest::Read {
        topic: TopicId(ChatSeq(topic)),
        last,
    };
    match PyHost::block(py, request)? {
        PyReply::Messages(messages) => {
            let list = pyo3::types::PyList::empty(py);
            for message in &messages {
                list.append(message_to_py(py, message)?)?;
            }
            Ok(list.into_any().unbind())
        }
        _ => Err(unexpected()),
    }
}

#[pyfunction]
fn join(py: Python<'_>, topic: u64, agent: &str) -> PyResult<()> {
    let request = PyRequest::Join {
        topic: TopicId(ChatSeq(topic)),
        agent: agent_id(agent)?,
    };
    match PyHost::block(py, request)? {
        PyReply::Done => Ok(()),
        _ => Err(unexpected()),
    }
}

#[pyfunction]
fn leave(py: Python<'_>, topic: u64, agent: &str) -> PyResult<()> {
    let request = PyRequest::Leave {
        topic: TopicId(ChatSeq(topic)),
        agent: agent_id(agent)?,
    };
    match PyHost::block(py, request)? {
        PyReply::Done => Ok(()),
        _ => Err(unexpected()),
    }
}

/// Gives a topic its whole head, for `agent`: the stream of that name,
/// created if new, its name, and whether it is resolved.
#[pyfunction]
fn change_topic(
    py: Python<'_>,
    agent: &str,
    topic: u64,
    stream: String,
    name: String,
    resolved: bool,
) -> PyResult<()> {
    let request = PyRequest::ChangeTopic {
        agent: agent_id(agent)?,
        topic: TopicId(ChatSeq(topic)),
        stream,
        name,
        resolved,
    };
    match PyHost::block(py, request)? {
        PyReply::Done => Ok(()),
        _ => Err(unexpected()),
    }
}

/// Moves messages for `agent` to topic `to`, or to a new topic named
/// `name` in `stream`; returns the topic they are in.
#[pyfunction]
#[pyo3(signature = (agent, messages, to=None, *, stream=None, name=None))]
fn move_messages(
    py: Python<'_>,
    agent: &str,
    messages: Vec<u64>,
    to: Option<u64>,
    stream: Option<String>,
    name: Option<String>,
) -> PyResult<u64> {
    let to = match (to, stream, name) {
        (Some(topic), None, None) => PyMoveTo::Topic(TopicId(ChatSeq(topic))),
        (None, Some(stream), Some(name)) => PyMoveTo::NewTopic { stream, name },
        _ => {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "move to a topic id, or to a new topic with stream= and name=",
            ));
        }
    };
    let request = PyRequest::Move {
        agent: agent_id(agent)?,
        messages: messages
            .into_iter()
            .map(|id| MessageId(ChatSeq(id)))
            .collect(),
        to,
    };
    match PyHost::block(py, request)? {
        PyReply::Seq(topic) => Ok(topic.0),
        _ => Err(unexpected()),
    }
}

/// A conversation: its stream's name and its name for a topic, None for
/// both in a direct conversation, and its members; None for no topic.
#[pyfunction]
fn topic(py: Python<'_>, id: u64) -> PyResult<Option<Py<PyAny>>> {
    match PyHost::block(py, PyRequest::Topic(TopicId(ChatSeq(id))))? {
        PyReply::Topic(None) => Ok(None),
        PyReply::Topic(Some((head, stream))) => {
            let out = PyDict::new(py);
            out.set_item("id", id)?;
            let (name, resolved) = match head.place {
                crate::store::Place::Stream { name, resolved, .. } => (Some(name), resolved),
                crate::store::Place::Direct => (None, false),
            };
            out.set_item("name", name)?;
            out.set_item("resolved", resolved)?;
            out.set_item("stream", stream)?;
            out.set_item(
                "agents",
                head.agents
                    .iter()
                    .map(|agent| agent.encoded().to_string())
                    .collect::<Vec<_>>(),
            )?;
            Ok(Some(out.into_any().unbind()))
        }
        _ => Err(unexpected()),
    }
}

fn message_to_py<'py>(
    py: Python<'py>,
    message: &rho_agent_types::Message,
) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);
    out.set_item("id", message.id.0.0)?;
    out.set_item("at", message.at.0)?;
    out.set_item("topic", message.topic.0.0)?;
    out.set_item("sender", address_text(message.from))?;
    let mentions = message
        .mentions
        .iter()
        .map(|mention| {
            (
                address_text(mention.to),
                match mention.kind {
                    MentionKind::Ask => "ask",
                    MentionKind::Result => "result",
                    MentionKind::Fyi => "fyi",
                },
            )
        })
        .collect::<Vec<_>>();
    out.set_item("mentions", mentions)?;
    out.set_item("text", text(&message.content))?;
    Ok(out)
}

/// What `agent` receives after `after`, as an awaitable of the caller's
/// loop.
#[pyfunction]
fn inbox(py: Python<'_>, agent: &str, after: u64) -> PyResult<Py<PyAny>> {
    let request = PyRequest::Inbox {
        agent: agent_id(agent)?,
        after: ChatSeq(after),
    };
    later(py, request, |py, reply| match reply {
        PyReply::Messages(messages) => messages
            .iter()
            .map(|message| message_to_py(py, message))
            .collect::<PyResult<Vec<_>>>()
            .and_then(|messages| Ok(PyList::new(py, messages)?.into_any())),
        _ => Err(unexpected()),
    })
}

/// A host call as an awaitable of the caller's loop: `value` makes what it
/// resolves to from the host's reply, and a refusal raises `RuntimeError`
/// in whoever awaits.
fn later(
    py: Python<'_>,
    request: PyRequest,
    value: impl for<'py> FnOnce(Python<'py>, PyReply) -> PyResult<Bound<'py, PyAny>> + Send + 'static,
) -> PyResult<Py<PyAny>> {
    let host = host()?;
    let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
    let future = event_loop.call_method0("create_future")?;
    let (event_loop, settle) = (event_loop.unbind(), future.clone().unbind());
    host.runtime.spawn(async move {
        let reply = host.call(request).await;
        Python::attach(|py| {
            let settle = settle.bind(py);
            let outcome = match reply {
                Ok(reply) => value(py, reply),
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

// -- the workset's database and its Slack routes ---------------------------

/// The workset process's own state, in `workset.redb` in the workset's
/// state directory: what only this workset reads, such as its Slack
/// routes. Opened on first use, so a workset that keeps nothing has no
/// file.
static WORKSET_DB: OnceLock<rho_db::RhoDb> = OnceLock::new();

fn workset_db(py: Python<'_>) -> PyResult<&'static rho_db::RhoDb> {
    let host = host()?;
    let path = host.state.join("workset.redb");
    Ok(py.detach(|| {
        WORKSET_DB.get_or_init(|| {
            let db = rho_db::RhoDb::open(path.as_std_path());
            host.runtime.block_on(Routes::create_tables(&db));
            db
        })
    }))
}

/// Run `work` on the routes without the GIL.
fn on_routes<T: Send>(
    py: Python<'_>,
    work: impl AsyncFnOnce(Routes<'static>) -> T + Send,
) -> PyResult<T> {
    let routes = Routes::of(workset_db(py)?);
    let runtime = &host()?.runtime;
    Ok(py.detach(|| runtime.block_on(work(routes))))
}

fn wide(channel: Option<String>) -> slack::Wide {
    match channel {
        Some(channel) => slack::Wide::Channel(channel),
        None => slack::Wide::Workspace,
    }
}

#[pyfunction]
fn slack_who_hears(
    py: Python<'_>,
    event: Bound<'_, PyAny>,
    bot_user: String,
    bot_id: String,
) -> PyResult<Option<(String, String)>> {
    let event: serde_json::Value = pythonize::depythonize(&event)
        .map_err(|error| PyValueError::new_err(format!("not a Slack event: {error}")))?;
    let to = on_routes(py, async move |routes| {
        let bot = slack::Bot {
            user: &bot_user,
            id: &bot_id,
        };
        routes.who_hears(&event, bot).await
    })?;
    Ok(to.map(|(agent, thread)| (agent.encoded().to_string(), thread)))
}

#[pyfunction]
#[pyo3(signature = (channel = None, thread_ts = None))]
fn slack_owner(
    py: Python<'_>,
    channel: Option<&str>,
    thread_ts: Option<&str>,
) -> PyResult<Option<String>> {
    let routes = Routes::of(workset_db(py)?);
    Ok(routes
        .owner(channel, thread_ts)
        .map(|agent| agent.encoded().to_string()))
}

#[pyfunction]
#[pyo3(signature = (agent, channel = None, thread_ts = None, mode = None))]
fn slack_route(
    py: Python<'_>,
    agent: &str,
    channel: Option<String>,
    thread_ts: Option<String>,
    mode: Option<&str>,
) -> PyResult<()> {
    let agent = agent_id(agent)?;
    match (channel, thread_ts, mode) {
        (Some(channel), Some(thread_ts), None) => on_routes(py, async move |routes| {
            routes.route_thread(&channel, &thread_ts, agent).await
        }),
        (_, Some(_), Some(_)) => Err(PyValueError::new_err(
            "a thread route hears its whole thread: it takes no mode",
        )),
        (None, Some(_), None) => Err(PyValueError::new_err("a thread is in a channel")),
        (channel, None, mode) => {
            let mode = slack::Mode::parse(mode.unwrap_or("mentions")).ok_or_else(|| {
                PyValueError::new_err(format!(
                    "a mode is \"mentions\", \"top_level\" or \"all\", not {mode:?}"
                ))
            })?;
            on_routes(py, async move |routes| {
                routes.route_wide(wide(channel), agent, mode).await
            })
        }
    }
}

#[pyfunction]
#[pyo3(signature = (channel = None, thread_ts = None))]
fn slack_unroute(
    py: Python<'_>,
    channel: Option<String>,
    thread_ts: Option<String>,
) -> PyResult<()> {
    match (channel, thread_ts) {
        (Some(channel), Some(thread_ts)) => on_routes(py, async move |routes| {
            routes.unroute_thread(&channel, &thread_ts).await
        }),
        (None, Some(_)) => Err(PyValueError::new_err("a thread is in a channel")),
        (channel, None) => on_routes(py, async move |routes| {
            routes.unroute_wide(wide(channel)).await
        }),
    }
}

#[pyfunction]
fn slack_posted(
    py: Python<'_>,
    channel: String,
    ts: String,
    thread_ts: String,
    agent: &str,
) -> PyResult<String> {
    let agent = agent_id(agent)?;
    let owner = on_routes(py, async move |routes| {
        routes.posted(&channel, &ts, &thread_ts, agent).await
    })?;
    Ok(owner.encoded().to_string())
}

#[pyfunction]
fn slack_routes(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let all = Routes::of(workset_db(py)?).all();
    let route = |agent: AgentId, mode: slack::Mode| -> PyResult<Bound<'_, PyDict>> {
        let row = PyDict::new(py);
        row.set_item("agent", agent.encoded().to_string())?;
        row.set_item("mode", mode.name())?;
        Ok(row)
    };
    let out = PyDict::new(py);
    out.set_item("workspace", py.None())?;
    let channels = PyDict::new(py);
    for (scope, agent, mode) in all.wide {
        match scope {
            slack::Wide::Workspace => out.set_item("workspace", route(agent, mode)?)?,
            slack::Wide::Channel(channel) => channels.set_item(channel, route(agent, mode)?)?,
        }
    }
    out.set_item("channels", channels)?;
    let threads = PyDict::new(py);
    for ((channel, thread), agent) in all.threads {
        threads.set_item((channel, thread), agent.encoded().to_string())?;
    }
    out.set_item("threads", threads)?;
    Ok(out)
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
                "haiku" => rho_claude::Model::Haiku,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each Claude model goes out to Python by the name Python gives it back.
    #[test]
    fn claude_models_round_trip_through_python() {
        rho_notebook::start_python().unwrap();
        Python::attach(|py| {
            for (model, name) in [
                (rho_claude::Model::Fable, "fable"),
                (rho_claude::Model::Opus, "opus"),
                (rho_claude::Model::Sonnet, "sonnet"),
                (rho_claude::Model::Haiku, "haiku"),
            ] {
                let spec = AgentSpec {
                    runtime: Runtime::Claude {
                        model,
                        effort: rho_claude::Effort::Max,
                        session_id: uuid::Uuid::nil(),
                    },
                    props: Default::default(),
                };
                let dict = spec_to_py(py, &spec).unwrap();
                let runtime: Bound<'_, PyDict> = item(&dict, "runtime").unwrap();
                assert_eq!(item::<String>(&runtime, "model").unwrap(), name);
                assert_eq!(spec_from_py(&dict, Some(&spec)).unwrap(), spec);
            }
        });
    }

    /// `_rho`'s Slack functions keep the routes in the workset's state
    /// directory, and take Slack events as Python gives them.
    #[test]
    fn slack_routes_go_through_python() {
        use rho_agent_types::AgentIdDomain;

        use crate::store::StoreWrite as _;
        let main = AgentId::from_counter(1, &AgentIdDomain(7)).unwrap();
        let other = AgentId::from_counter(2, &AgentIdDomain(7)).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = rho_db::RhoDb::in_memory();
        runtime.block_on(async {
            let mut write = db.write().await;
            write.init_store_tables();
            write.create_workset(
                WorksetId([7; 6]),
                rho_agent_types::UnixMs(1),
                rho_agent_types::WorksetKind::Isolated,
                String::new(),
            );
            write.commit();
        });
        crate::testing::python(main, &db);
        let code = format!(
            r#"
import _rho
main, other = {main:?}, {other:?}
assert _rho.slack_owner() is None
_rho.slack_route(main)
assert _rho.slack_owner("C1", "1.0") == main
mention = {{"type": "message", "channel": "C1", "channel_type": "channel", "user": "U1",
    "ts": "1.0", "text": "<@UBOT> hi", "blocks": [{{"type": "rich_text", "elements": []}}], "edited": None}}
assert _rho.slack_who_hears(mention, "UBOT", "BBOT") == (main, "1.0")
assert _rho.slack_who_hears(dict(mention, ts="2.0", text="hi"), "UBOT", "BBOT") is None
assert _rho.slack_posted("C1", "1.5", "1.0", other) == main
reaction = {{"type": "reaction_added", "user": "U1", "reaction": "eyes",
    "item": {{"type": "message", "channel": "C1", "ts": "1.5"}}, "item_user": "UBOT"}}
assert _rho.slack_who_hears(reaction, "UBOT", "BBOT") == (other, "1.0")
_rho.slack_route(other, "C3")
_rho.slack_route(main, "C2", mode="top_level")
_rho.slack_route(other, "C4", "4.0")
for bad in (dict(channel="C2", mode="loud"), dict(channel="C2", thread_ts="1.0", mode="all"), dict(thread_ts="1.0")):
    try:
        _rho.slack_route(main, **bad)
    except ValueError:
        pass
    else:
        raise AssertionError(f"took {{bad}}")
_rho.slack_unroute("C4", "4.0")
routes = _rho.slack_routes()
assert routes == {{
    "workspace": {{"agent": main, "mode": "mentions"}},
    "channels": {{"C2": {{"agent": main, "mode": "top_level"}}, "C3": {{"agent": other, "mode": "mentions"}}}},
    "threads": {{("C1", "1.0"): main}},
}}, routes
"#,
            main = main.encoded().to_string(),
            other = other.encoded().to_string(),
        );
        let (output, error) = runtime.block_on(eval(code)).unwrap();
        assert_eq!(error, None, "{output}");
        assert!(host().unwrap().state.join("workset.redb").exists());
    }
}
