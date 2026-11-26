//! First-class CLI for spawning, messaging, inspecting, and receiving replies
//! from rho agents on a running `rho-agent-host` over existing `rho_rpc`
//! protocols (`Worksets` and `Chat`).

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use camino::Utf8PathBuf;
use clap::{Args, Subcommand, ValueEnum};
use rho_agent_types::{
    Address, AgentId, AgentIdDomain, AgentRole, ChatEvent, ChatRecord, ChatSeq, ContentPart,
    EndpointId, Message, MessageId, ProjectConfig, ProjectMode, ProjectName, ProjectPlace,
    Revision, RuntimeCatalogue, RuntimeLabel, ThreadWork, UnixMs, Voice, WorksetId, WorksetKind,
    Wpos, agent_cwd,
};
use rho_agents_client::protocol::snippets::{self, Function};
use rho_agents_client::protocol::transcript::{InferenceState, Item, RuntimeState, Speaker};
use rho_agents_client::protocol::worksets::{
    self, AgentEntry, AgentLog, AgentLogPage, Cancel, CancelResult, CreateWorkset, Eval, LogEvent,
    LogNext, StopAgent, TellAgent, WorksetEntry,
};
use rho_chat::ThreadId;
use rho_chat::protocol::{
    self as chat, ClientFrame as ChatClientFrame, CreateProject, Name as NameThread, Post,
    ResolveProject, ServerFrame as ChatServerFrame,
};
use rho_rpc::protocol::RuntimePaths;
use rho_rpc::protocol::client::{self, Client};
use serde::Serialize;
use serde_json::json;
use tokio::io::AsyncReadExt as _;
use tokio::time::Instant;

const DEFAULT_PROJECT: &str = "cli";
const DEFAULT_TIMEOUT_SECS: u64 = 300;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Grace window after an agent enters `Failed`/`Lost` so any `Address::System`
/// stop mail already in flight on the `Chat` stream arrives first.
const STOP_MAIL_GRACE: Duration = Duration::from_millis(250);
/// Drain window for initial `Worksets` `Status` frames after `Directory`.
const INITIAL_STATUS_DRAIN: Duration = Duration::from_millis(35);

#[derive(Clone, Debug, Args)]
pub(crate) struct AgentArgs {
    /// Agent host runtime directory; defaults to `$RHO_RUNTIME_DIR`, then
    /// `$XDG_RUNTIME_DIR/rho`.
    #[arg(long = "runtime-dir", global = true)]
    runtime_dir: Option<PathBuf>,

    /// Emit machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: AgentCommand,
}

#[derive(Clone, Debug, Subcommand)]
enum AgentCommand {
    /// Spawn a new agent (in a new workset or existing `--workset`) and
    /// optionally send an initial prompt.
    Spawn(SpawnArgs),
    /// Send a message to an existing thread or agent.
    Send(SendArgs),
    /// Wait for and receive the next reply from a thread or agent.
    #[command(alias = "wait")]
    Recv(RecvArgs),
    /// Read recent chat messages in a thread (non-blocking).
    Read(ReadArgs),
    /// List agents on the host.
    #[command(alias = "ls")]
    List(ListArgs),
    /// Show detailed status for one agent.
    Status(StatusArgs),
    /// Read an agent's execution transcript log.
    Transcript(TranscriptArgs),
    /// Cancel an agent's in-flight model request.
    Cancel(CancelArgs),
    /// Stop (archive) an agent cleanly.
    Stop(StopArgs),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum AgentPreset {
    #[default]
    Engineer,
    Advisor,
}

impl AgentPreset {
    fn as_str(self) -> &'static str {
        match self {
            Self::Engineer => "engineer",
            Self::Advisor => "advisor",
        }
    }
}

#[derive(Clone, Debug, Args)]
struct SpawnArgs {
    /// Initial prompt text (use `-` to read from stdin).
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Read initial prompt from stdin.
    #[arg(long, conflicts_with = "prompt")]
    stdin: bool,

    /// Runtime spec (`<model>-<effort>`, e.g. `opus-high`, `fable-high`,
    /// `sol-medium`, `astra-high`, `terra-medium`, `neon-high`).
    #[arg(long, default_value = rho_agent_types::DEFAULT_RUNTIME, value_parser = RuntimeLabel::parse)]
    runtime: RuntimeLabel,

    /// Host directory to work in (relative paths are resolved from current
    /// directory). Without `--cwd` or `--repo`, an isolated workset is created.
    #[arg(long, conflicts_with_all = ["isolated", "repo"])]
    cwd: Option<Utf8PathBuf>,

    /// Create an isolated workset with its own `/src` (default when `--cwd`,
    /// `--repo`, and `--workset` are omitted).
    #[arg(long, conflicts_with_all = ["cwd", "workset"])]
    isolated: bool,

    /// Clone this git repository URL or path into a fresh isolated workset.
    #[arg(long, conflicts_with_all = ["cwd", "workset"])]
    repo: Option<Utf8PathBuf>,

    /// Git revision to check out when using `--repo`.
    #[arg(long, default_value = "", requires = "repo")]
    rev: String,

    /// Spawn inside an existing workset id instead of creating a new one.
    #[arg(long, conflicts_with_all = ["isolated", "repo"])]
    workset: Option<String>,

    /// Agent preset (`engineer` or `advisor`).
    #[arg(long, value_enum, default_value_t = AgentPreset::Engineer)]
    preset: AgentPreset,

    /// Project/channel root name for the initial thread.
    #[arg(long, default_value = DEFAULT_PROJECT)]
    project: String,

    /// Optional name for the agent and its initial thread.
    #[arg(long)]
    name: Option<String>,

    /// Policy function to invoke when creating the agent.
    #[arg(long, default_value = snippets::DEFAULT_FUNCTION, value_parser = Function::parse)]
    function: Function,

    /// Wait for the agent's first reply after sending the initial prompt.
    #[arg(long)]
    wait: bool,

    /// Timeout in seconds when `--wait` is set.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    timeout: u64,

    /// Show flash updates (stderr in text mode, `flashes` array in JSON mode)
    /// while waiting.
    #[arg(long)]
    include_flashes: bool,
}

#[derive(Clone, Debug, Args)]
struct SendArgs {
    /// Message text (use `-` to read from stdin).
    #[arg(value_name = "TEXT")]
    text: Option<String>,

    /// Read message text from stdin.
    #[arg(long, conflicts_with = "text")]
    stdin: bool,

    /// Thread id (`42`, `#42`, or `thread:<host>/42`).
    #[arg(long, required_unless_present = "agent")]
    thread: Option<String>,

    /// Target agent id (`eng-...`, `adv-...`, raw id, or unique prefix).
    #[arg(long, required_unless_present = "thread")]
    agent: Option<String>,

    /// Start a new thread when addressing `--agent` instead of reusing its
    /// existing thread.
    #[arg(long, conflicts_with = "thread")]
    new_thread: bool,

    /// Project/channel root name when starting a new thread.
    #[arg(long, default_value = DEFAULT_PROJECT)]
    project: String,

    /// Optional name when starting a new thread.
    #[arg(long)]
    name: Option<String>,

    /// Wait for the agent's reply after sending.
    #[arg(long)]
    wait: bool,

    /// Timeout in seconds when `--wait` is set.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    timeout: u64,

    /// Show flash updates (stderr in text mode, `flashes` array in JSON mode)
    /// while waiting.
    #[arg(long)]
    include_flashes: bool,
}

#[derive(Clone, Debug, Args)]
struct RecvArgs {
    /// Thread id (`42`, `#42`, or `thread:<host>/42`).
    #[arg(long, required_unless_present = "agent")]
    thread: Option<String>,

    /// Agent id (`eng-...`, `adv-...`, raw id, or unique prefix).
    #[arg(long, required_unless_present = "thread")]
    agent: Option<String>,

    /// Only return events strictly after this chat sequence cursor.
    #[arg(long)]
    after: Option<u64>,

    /// Timeout in seconds (`0` checks already-arrived events without blocking).
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    timeout: u64,

    /// Show flash updates (stderr in text mode, `flashes` array in JSON mode).
    #[arg(long)]
    include_flashes: bool,

    /// Continuously stream replies/events until timeout or interrupt.
    #[arg(long)]
    follow: bool,
}

#[derive(Clone, Debug, Args)]
struct ReadArgs {
    /// Thread id (`42`, `#42`, or `thread:<host>/42`).
    #[arg(long, required_unless_present = "agent")]
    thread: Option<String>,

    /// Agent id (reads its primary thread).
    #[arg(long, required_unless_present = "thread")]
    agent: Option<String>,

    /// Maximum number of recent messages to display (`0` for all).
    #[arg(long, default_value_t = 20)]
    last: usize,

    /// Only show messages with sequence strictly greater than `<SEQ>`.
    #[arg(long)]
    after: Option<u64>,

    /// Include `Flash` status messages.
    #[arg(long)]
    include_flashes: bool,
}

#[derive(Clone, Debug, Args)]
struct ListArgs {
    /// Filter by workset id.
    #[arg(long)]
    workset: Option<String>,

    /// Filter by state (`working`, `waiting`, `idle`, `retrying`, `failed`,
    /// `lost`, `archived`).
    #[arg(long)]
    state: Option<String>,
}

#[derive(Clone, Debug, Args)]
struct StatusArgs {
    /// Agent id (`eng-...`, `adv-...`, raw id, or unique prefix).
    agent: String,
}

#[derive(Clone, Debug, Args)]
struct TranscriptArgs {
    /// Agent id (`eng-...`, `adv-...`, raw id, or unique prefix).
    agent: String,

    /// Show the last `N` log events (`0` for all).
    #[arg(long, default_value_t = 50)]
    last: usize,

    /// Start reading from workset log position `WPOS`.
    #[arg(long, default_value_t = 0)]
    from: u64,

    /// Maximum characters per field in plain-text output (`0` for unlimited).
    #[arg(long = "max-chars", default_value_t = 2000)]
    max_chars: usize,
}

#[derive(Clone, Debug, Args)]
struct CancelArgs {
    /// Agent id (`eng-...`, `adv-...`, raw id, or unique prefix).
    agent: String,
}

#[derive(Clone, Debug, Args)]
struct StopArgs {
    /// Agent id (`eng-...`, `adv-...`, raw id, or unique prefix).
    agent: String,

    /// Also delete the agent's workset after stopping it.
    #[arg(long)]
    delete_workset: bool,
}

pub(crate) async fn run(args: AgentArgs) -> Result<()> {
    let paths = RuntimePaths::resolve(args.runtime_dir)?;
    let json = args.json;
    run_inner(&paths, json, args.command).await
}

async fn run_inner(paths: &RuntimePaths, json: bool, command: AgentCommand) -> Result<()> {
    match command {
        AgentCommand::Spawn(cmd) => run_spawn(paths, json, cmd).await,
        AgentCommand::Send(cmd) => run_send(paths, json, cmd).await,
        AgentCommand::Recv(cmd) => run_recv(paths, json, cmd).await,
        AgentCommand::Read(cmd) => run_read(paths, json, cmd).await,
        AgentCommand::List(cmd) => run_list(paths, json, cmd).await,
        AgentCommand::Status(cmd) => run_status(paths, json, cmd).await,
        AgentCommand::Transcript(cmd) => run_transcript(paths, json, cmd).await,
        AgentCommand::Cancel(cmd) => run_cancel(paths, json, cmd).await,
        AgentCommand::Stop(cmd) => run_stop(paths, json, cmd).await,
    }
}

async fn run_spawn(paths: &RuntimePaths, json: bool, args: SpawnArgs) -> Result<()> {
    let prompt = read_optional_input(args.prompt.as_deref(), args.stdin).await?;
    if args.wait && prompt.is_none() {
        anyhow::bail!("`rho agent spawn --wait` requires an initial prompt");
    }
    let resolved_cwd = args.cwd.as_deref().map(resolve_cwd).transpose()?;

    let (workset, agent_id, effective_cwd) = if let Some(workset_str) = &args.workset {
        let workset: WorksetId = workset_str
            .parse()
            .with_context(|| format!("invalid workset id `{workset_str}`"))?;
        let snapshot = fetch_directory_snapshot(paths).await?;
        let workset_entry = snapshot
            .worksets
            .get(&workset)
            .with_context(|| format!("no workset {workset} on this host"))?;
        let target_cwd = match &resolved_cwd {
            Some(cwd) => cwd.clone(),
            None => default_workset_root(&workset_entry.kind),
        };
        let code = build_spawn_in_code(&args.function, &target_cwd, &args.runtime, args.preset);
        let result = client::call(paths, Eval { workset, code }).await?;
        if let Some(error) = result.error.as_deref() {
            let reason = result.raised().unwrap_or(error);
            anyhow::bail!("failed to create agent in workset {workset}: {reason}");
        }
        let agent = result
            .agent
            .context("agent creation code succeeded but created no agent")?;
        (workset, agent, target_cwd.into_string())
    } else {
        let (kind, code, effective_cwd) = if let Some(repo) = &args.repo {
            let mut code =
                snippets::new_agent_on(&args.function, repo, &args.rev, &args.runtime, None);
            if args.preset != AgentPreset::Engineer {
                code = splice_preset_arg(&code, args.preset);
            }
            (
                WorksetKind::Isolated,
                code,
                rho_fs_view::MOUNT_ROOT.to_owned(),
            )
        } else if let Some(cwd) = &resolved_cwd {
            let code = build_spawn_in_code(&args.function, cwd, &args.runtime, args.preset);
            (
                WorksetKind::Host { cwd: cwd.clone() },
                code,
                cwd.to_string(),
            )
        } else {
            let root = Utf8PathBuf::from(rho_fs_view::MOUNT_ROOT);
            let code = build_spawn_in_code(&args.function, &root, &args.runtime, args.preset);
            (WorksetKind::Isolated, code, root.into_string())
        };
        let created = client::call(paths, CreateWorkset { kind, code }).await?;
        if let Some(error) = created.result.error.as_deref() {
            let reason = created.result.raised().unwrap_or(error);
            anyhow::bail!("failed to create workset/agent: {reason}");
        }
        let agent = created
            .result
            .agent
            .context("agent creation code succeeded but created no agent")?;
        (created.workset, agent, effective_cwd)
    };

    if let Some(name) = &args.name {
        let code = format!(
            "import rho\n_agent = rho.agent({id})\n_props = dict(_agent.props)\n_props[\"name\"] = {name}\n_agent.set_props(_props)\n",
            id = snippets::py_str(&agent_id.encoded()),
            name = snippets::py_str(name),
        );
        let eval_res = client::call(paths, Eval { workset, code }).await?;
        if let Some(error) = eval_res.error.as_deref() {
            let reason = eval_res.raised().unwrap_or(error);
            anyhow::bail!("created agent, but failed to set name: {reason}");
        }
    }

    let handle = preset_role(args.preset).full_handle(agent_id);

    let Some(prompt_text) = prompt else {
        if json {
            print_json(&json!({
                "workset": workset.to_string(),
                "agent": handle,
                "agent_id": agent_id.encoded(),
                "runtime": args.runtime.as_str(),
                "cwd": effective_cwd,
                "preset": args.preset.as_str(),
                "name": args.name,
            }))?;
        } else {
            println!("workset: {workset}");
            println!("agent: {handle}");
            println!("runtime: {}", args.runtime);
            println!("cwd: {effective_cwd}");
            if let Some(name) = &args.name {
                println!("name: {name}");
            }
        }
        return Ok(());
    };

    let project_root = ensure_project_root(paths, &args.project).await?;
    let posted = post_message(paths, Some(project_root), Some(agent_id), prompt_text).await?;
    if let Some(name) = &args.name {
        client::call(
            paths,
            NameThread {
                thread: posted,
                name: name.clone(),
            },
        )
        .await?;
    }

    if !args.wait {
        if json {
            print_json(&json!({
                "workset": workset.to_string(),
                "agent": handle,
                "agent_id": agent_id.encoded(),
                "runtime": args.runtime.as_str(),
                "cwd": effective_cwd,
                "preset": args.preset.as_str(),
                "name": args.name,
                "project": project_root.0.0,
                "thread": posted.0.0,
                "message": posted.0.0,
                "seq": posted.0.0,
                "cursor": posted.0.0,
            }))?;
        } else {
            println!("workset: {workset}");
            println!("agent: {handle}");
            println!("runtime: {}", args.runtime);
            println!("cwd: {effective_cwd}");
            println!("thread: {}", posted.0.0);
            println!("message: {}", posted.0.0);
            println!("cursor: {}", posted.0.0);
        }
        return Ok(());
    }

    let (snapshot, worksets_client) = fetch_directory(paths).await?;
    let wait_res = wait_for_reply(
        paths,
        snapshot,
        worksets_client,
        WaitTarget {
            thread: Some(posted),
            agent: Some(agent_id),
        },
        Some(posted.0),
        Duration::from_secs(args.timeout),
        args.include_flashes,
        json,
    )
    .await?;

    emit_wait_outcome(
        wait_res,
        json,
        args.include_flashes,
        Some(json!({
            "workset": workset.to_string(),
            "agent": handle,
            "agent_id": agent_id.encoded(),
            "runtime": args.runtime.as_str(),
            "cwd": effective_cwd,
            "preset": args.preset.as_str(),
            "name": args.name,
            "project": project_root.0.0,
            "sent_message": posted.0.0,
        })),
        Some(&format!("workset={workset} agent={handle} ")),
    )
}

async fn run_send(paths: &RuntimePaths, json: bool, args: SendArgs) -> Result<()> {
    let text = read_required_input(args.text.as_deref(), args.stdin).await?;
    let (snapshot, mut worksets_client) = fetch_directory(paths).await?;

    let target_agent = args
        .agent
        .as_deref()
        .map(|query| resolve_agent(&snapshot, query).map(|entry| entry.agent))
        .transpose()?;

    let explicit_thread = args.thread.as_deref().map(parse_thread_id).transpose()?;

    let (parent_for_post, created_new_thread) = match (explicit_thread, target_agent) {
        (Some(thread), _) => {
            // Verify thread exists in chat log.
            let chat_state = catch_up_chat_snapshot(paths).await?;
            anyhow::ensure!(
                chat_state.messages.contains_key(&thread),
                "no thread #{} on this host",
                thread.0.0
            );
            (thread, false)
        }
        (None, Some(agent)) => {
            if !args.new_thread {
                let chat_state = catch_up_chat_snapshot(paths).await?;
                if let Some(existing) =
                    chat_state.agent_primary_thread(agent, snapshot.agents.get(&agent))
                {
                    (existing, false)
                } else {
                    let project_root = ensure_project_root(paths, &args.project).await?;
                    (project_root, true)
                }
            } else {
                let project_root = ensure_project_root(paths, &args.project).await?;
                (project_root, true)
            }
        }
        (None, None) => unreachable!("clap enforces --thread or --agent"),
    };

    let posted = post_message(paths, Some(parent_for_post), target_agent, text).await?;
    let effective_thread = if created_new_thread {
        if let Some(name) = &args.name {
            client::call(
                paths,
                NameThread {
                    thread: posted,
                    name: name.clone(),
                },
            )
            .await?;
        }
        posted
    } else {
        parent_for_post
    };

    if !args.wait {
        let _ = worksets_client.shutdown().await;
        if json {
            print_json(&json!({
                "thread": effective_thread.0.0,
                "message": posted.0.0,
                "seq": posted.0.0,
                "cursor": posted.0.0,
                "agent": target_agent.and_then(|id| snapshot.agents.get(&id)).map(agent_handle),
            }))?;
        } else {
            println!("thread: {}", effective_thread.0.0);
            println!("message: {}", posted.0.0);
            println!("cursor: {}", posted.0.0);
        }
        return Ok(());
    }

    let wait_res = wait_for_reply(
        paths,
        snapshot,
        worksets_client,
        WaitTarget {
            thread: Some(effective_thread),
            agent: target_agent,
        },
        Some(posted.0),
        Duration::from_secs(args.timeout),
        args.include_flashes,
        json,
    )
    .await?;

    emit_wait_outcome(
        wait_res,
        json,
        args.include_flashes,
        Some(json!({ "sent_message": posted.0.0 })),
        None,
    )
}

async fn run_recv(paths: &RuntimePaths, json: bool, args: RecvArgs) -> Result<()> {
    let (snapshot, worksets_client) = fetch_directory(paths).await?;
    let target_agent = args
        .agent
        .as_deref()
        .map(|query| resolve_agent(&snapshot, query).map(|entry| entry.agent))
        .transpose()?;
    let target_thread = args.thread.as_deref().map(parse_thread_id).transpose()?;
    let after = args.after.map(ChatSeq);

    if args.follow {
        return follow_replies(
            paths,
            snapshot,
            worksets_client,
            WaitTarget {
                thread: target_thread,
                agent: target_agent,
            },
            after,
            Duration::from_secs(args.timeout),
            args.include_flashes,
            json,
        )
        .await;
    }

    let wait_res = wait_for_reply(
        paths,
        snapshot,
        worksets_client,
        WaitTarget {
            thread: target_thread,
            agent: target_agent,
        },
        after,
        Duration::from_secs(args.timeout),
        args.include_flashes,
        json,
    )
    .await?;

    emit_wait_outcome(wait_res, json, args.include_flashes, None, None)
}

fn emit_wait_outcome(
    outcome: WaitOutcome,
    json: bool,
    include_flashes: bool,
    extra_json: Option<serde_json::Value>,
    stderr_prefix: Option<&str>,
) -> Result<()> {
    match outcome {
        WaitOutcome::Reply(reply) => {
            if json {
                let mut payload = json!({
                    "thread": reply.thread,
                    "seq": reply.seq,
                    "message": reply.message,
                    "cursor": reply.seq,
                    "kind": reply.kind,
                    "from": reply.from,
                    "agent": reply.agent,
                    "agent_id": reply.agent_id,
                    "text": reply.text,
                });
                if let Some(serde_json::Value::Object(extra)) = extra_json
                    && let Some(obj) = payload.as_object_mut()
                {
                    for (k, v) in extra {
                        obj.insert(k, v);
                    }
                }
                if include_flashes {
                    payload["flashes"] = serde_json::to_value(&reply.flashes)?;
                }
                print_json(&payload)?;
            } else {
                let prefix = stderr_prefix.unwrap_or("");
                eprintln!(
                    "{prefix}thread={} message={} cursor={}",
                    reply.thread, reply.message, reply.seq
                );
                println!("{}", reply.text);
            }
            Ok(())
        }
        WaitOutcome::Stop(stop) => {
            if json {
                let mut payload = json!({
                    "thread": stop.thread,
                    "seq": stop.seq,
                    "cursor": stop.seq,
                    "kind": stop.kind,
                    "error": stop.text,
                });
                if let Some(serde_json::Value::Object(extra)) = extra_json
                    && let Some(obj) = payload.as_object_mut()
                {
                    for (k, v) in extra {
                        obj.insert(k, v);
                    }
                }
                if include_flashes {
                    payload["flashes"] = serde_json::to_value(&stop.flashes)?;
                }
                print_json(&payload)?;
            }
            anyhow::bail!("agent stopped: {}", stop.text);
        }
    }
}

async fn run_read(paths: &RuntimePaths, json: bool, args: ReadArgs) -> Result<()> {
    let (snapshot, mut worksets_client) = fetch_directory(paths).await?;
    let _ = worksets_client.shutdown().await;
    let (chat, mut chat_client) = catch_up_chat(paths).await?;
    let _ = chat_client.shutdown().await;
    let target_agent = args
        .agent
        .as_deref()
        .map(|query| resolve_agent(&snapshot, query).map(|entry| entry.agent))
        .transpose()?;
    let thread = match (&args.thread, target_agent) {
        (Some(thread), _) => {
            let id = parse_thread_id(thread)?;
            anyhow::ensure!(chat.messages.contains_key(&id), "no thread #{}", id.0.0);
            id
        }
        (None, Some(agent)) => chat
            .agent_primary_thread(agent, snapshot.agents.get(&agent))
            .with_context(|| {
                format!(
                    "agent {} has not joined any thread",
                    snapshot
                        .agents
                        .get(&agent)
                        .map(agent_handle)
                        .unwrap_or_else(|| agent.encoded())
                )
            })?,
        (None, None) => unreachable!("clap enforces --thread or --agent"),
    };

    let after = ChatSeq(args.after.unwrap_or(0));
    let mut rows = chat
        .messages
        .values()
        .filter(|msg| msg.revision != Revision::Deleted)
        .filter(|msg| msg.id == thread || chat.thread_of(msg.id) == thread)
        .filter(|msg| args.include_flashes || msg.voice != Voice::Flash)
        .filter(|msg| {
            let seq = chat.effective_seq(msg);
            seq > after
        })
        .map(|msg| message_view(&snapshot, &chat, msg, thread))
        .collect::<Vec<_>>();

    if args.last > 0 && rows.len() > args.last {
        let start = rows.len() - args.last;
        rows = rows.split_off(start);
    }

    let cursor = rows.last().map(|r| r.seq).unwrap_or(after.0);
    let thread_name = chat.thread_name(thread);

    if json {
        print_json(&json!({
            "thread": thread.0.0,
            "name": thread_name,
            "cursor": cursor,
            "messages": rows,
        }))?;
    } else {
        println!(
            "thread #{} ({})",
            thread.0.0,
            thread_name.as_deref().unwrap_or("unnamed")
        );
        for row in &rows {
            let kind_tag = match row.kind.as_str() {
                "flash" => " [flash]",
                "status" => " [status]",
                "stop" => " [stop]",
                _ => "",
            };
            println!(
                "#{} (seq {}) {}{}:",
                row.message, row.seq, row.from, kind_tag
            );
            for line in row.text.lines() {
                println!("  {line}");
            }
            if row.text.is_empty() {
                println!("  (empty)");
            }
        }
    }
    Ok(())
}

async fn run_list(paths: &RuntimePaths, json: bool, args: ListArgs) -> Result<()> {
    let (snapshot, mut worksets_client) = fetch_directory(paths).await?;
    let _ = worksets_client.shutdown().await;
    let workset_filter: Option<WorksetId> = args
        .workset
        .as_deref()
        .map(|s| {
            s.parse()
                .with_context(|| format!("invalid workset id `{s}`"))
        })
        .transpose()?;
    let state_filter = args.state.as_deref().map(str::to_ascii_lowercase);

    let mut entries = snapshot.agents.values().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.created_at);

    let mut items = Vec::new();
    for entry in entries {
        if workset_filter.is_some_and(|w| w != entry.workset) {
            continue;
        }
        let state_word = snapshot
            .statuses
            .get(&entry.agent)
            .map(|(state, _)| state.activity().word())
            .unwrap_or("idle");
        if state_filter
            .as_deref()
            .is_some_and(|filter| filter != state_word)
        {
            continue;
        }
        items.push(AgentSummary {
            agent: agent_handle(entry),
            agent_id: entry.agent.encoded(),
            workset: entry.workset.to_string(),
            runtime: entry.spec.runtime_label(),
            state: state_word.to_owned(),
            cwd: workset_cwd(entry, &snapshot.worksets),
            name: entry
                .spec
                .props
                .get("name")
                .or_else(|| entry.spec.props.get("title"))
                .cloned(),
            origin: entry.spec.props.get("origin").cloned(),
        });
    }

    if json {
        print_json(&json!({ "agents": items }))?;
    } else if items.is_empty() {
        println!("no agents");
    } else {
        println!("AGENT\tWORKSET\tRUNTIME\tSTATE\tCWD\tNAME");
        for item in &items {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                item.agent,
                item.workset,
                item.runtime,
                item.state,
                item.cwd,
                item.name.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

async fn run_status(paths: &RuntimePaths, json: bool, args: StatusArgs) -> Result<()> {
    let (snapshot, mut worksets_client) = fetch_directory(paths).await?;
    let _ = worksets_client.shutdown().await;
    let entry = resolve_agent(&snapshot, &args.agent)?;
    let (chat, mut chat_client) = catch_up_chat(paths).await?;
    let _ = chat_client.shutdown().await;
    let runtime_state = snapshot.statuses.get(&entry.agent);
    let activity = runtime_state
        .map(|(state, _)| state.activity().word())
        .unwrap_or("idle");
    let inference_detail = runtime_state.map(|(state, _)| describe_inference(&state.inference));
    let threads = chat
        .joined
        .get(&entry.agent)
        .map(|threads| threads.iter().map(|t| t.0.0).collect::<Vec<_>>())
        .unwrap_or_default();
    let cwd = workset_cwd(entry, &snapshot.worksets);
    let handle = agent_handle(entry);

    if json {
        print_json(&json!({
            "agent": handle,
            "agent_id": entry.agent.encoded(),
            "workset": entry.workset.to_string(),
            "runtime": entry.spec.runtime_label(),
            "runtime_kind": match entry.spec.runtime {
                rho_agent_types::RuntimeKind::Rho => "native",
                rho_agent_types::RuntimeKind::Claude => "claude",
            },
            "state": activity,
            "inference": inference_detail,
            "running_tasks": runtime_state.map(|(s, _)| s.running_tasks).unwrap_or(0),
            "awaiting_human": runtime_state.is_some_and(|(s, _)| s.awaiting_human),
            "archived": runtime_state.is_some_and(|(s, _)| s.archived),
            "since_ms": runtime_state.map(|(_, since)| since.0),
            "cwd": cwd,
            "threads": threads,
            "props": entry.spec.props,
        }))?;
    } else {
        println!("agent: {handle}");
        println!("agent_id: {}", entry.agent.encoded());
        println!("workset: {}", entry.workset);
        println!("runtime: {}", entry.spec.runtime_label());
        println!("state: {activity}");
        if let Some(detail) = inference_detail {
            println!("inference: {detail}");
        }
        println!("cwd: {cwd}");
        if !threads.is_empty() {
            let list = threads
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("threads: {list}");
        }
        if !entry.spec.props.is_empty() {
            for (k, v) in &entry.spec.props {
                println!("prop.{k}: {v}");
            }
        }
    }
    Ok(())
}

async fn run_transcript(paths: &RuntimePaths, json: bool, args: TranscriptArgs) -> Result<()> {
    let snapshot = fetch_directory_snapshot(paths).await?;
    let entry = resolve_agent(&snapshot, &args.agent)?;
    let handle = agent_handle(entry);

    let mut from = Wpos(args.from);
    let mut all_rows = Vec::new();
    loop {
        let AgentLogPage { rows, next } = client::call(
            paths,
            AgentLog {
                agent: entry.agent,
                from,
            },
        )
        .await?;
        all_rows.extend(rows);
        match next {
            LogNext::Page(next_pos) => from = next_pos,
            LogNext::Follow(_) => break,
        }
    }

    if args.last > 0 && all_rows.len() > args.last {
        let start = all_rows.len() - args.last;
        all_rows = all_rows.split_off(start);
    }

    if json {
        let events = all_rows
            .iter()
            .map(|(pos, event)| log_event_json(*pos, event))
            .collect::<Vec<_>>();
        print_json(&json!({
            "agent": handle,
            "agent_id": entry.agent.encoded(),
            "events": events,
        }))?;
    } else {
        for (pos, event) in &all_rows {
            let line = format_log_event(event, args.max_chars);
            println!("#{} {}", pos.0, line);
        }
    }
    Ok(())
}

async fn run_cancel(paths: &RuntimePaths, json: bool, args: CancelArgs) -> Result<()> {
    let snapshot = fetch_directory_snapshot(paths).await?;
    let entry = resolve_agent(&snapshot, &args.agent)?;
    let handle = agent_handle(entry);
    let result = client::call(paths, Cancel { agent: entry.agent }).await?;
    let was_asking = matches!(result, CancelResult::WasAsking);
    if json {
        print_json(&json!({
            "agent": handle,
            "agent_id": entry.agent.encoded(),
            "was_asking": was_asking,
        }))?;
    } else if was_asking {
        println!("cancelled active request for {handle}");
    } else {
        println!("{handle} had no active request");
    }
    Ok(())
}

async fn run_stop(paths: &RuntimePaths, json: bool, args: StopArgs) -> Result<()> {
    let snapshot = fetch_directory_snapshot(paths).await?;
    let entry = resolve_agent(&snapshot, &args.agent)?;
    let handle = agent_handle(entry);
    client::call(paths, StopAgent { agent: entry.agent }).await?;
    let mut deleted_workset = false;
    if args.delete_workset {
        client::call(
            paths,
            worksets::DeleteWorksets {
                worksets: vec![entry.workset],
            },
        )
        .await?;
        deleted_workset = true;
    }
    if json {
        print_json(&json!({
            "agent": handle,
            "agent_id": entry.agent.encoded(),
            "workset": entry.workset.to_string(),
            "stopped": true,
            "deleted_workset": deleted_workset,
        }))?;
    } else if deleted_workset {
        println!("stopped {handle} and deleted workset {}", entry.workset);
    } else {
        println!("stopped {handle}");
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

#[derive(Serialize)]
struct AgentSummary {
    agent: String,
    agent_id: String,
    workset: String,
    runtime: String,
    state: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct FlashView {
    seq: u64,
    message: u64,
    from: String,
    kind: String,
    text: String,
}

#[derive(Clone, Debug, Serialize)]
struct MessageView {
    seq: u64,
    message: u64,
    thread: u64,
    from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    kind: String,
    pings_user: bool,
    text: String,
}

#[derive(Clone, Copy)]
struct WaitTarget {
    thread: Option<MessageId>,
    agent: Option<AgentId>,
}

enum WaitOutcome {
    Reply(ReplyEvent),
    Stop(StopEvent),
}

struct ReplyEvent {
    thread: u64,
    seq: u64,
    message: u64,
    from: String,
    agent: Option<String>,
    agent_id: Option<String>,
    kind: &'static str,
    text: String,
    flashes: Vec<FlashView>,
}

struct StopEvent {
    thread: u64,
    seq: u64,
    kind: &'static str,
    text: String,
    flashes: Vec<FlashView>,
}

struct DirectorySnapshot {
    machine_seed: u64,
    agent_counter: u64,
    worksets: BTreeMap<WorksetId, WorksetEntry>,
    agents: BTreeMap<AgentId, AgentEntry>,
    statuses: HashMap<AgentId, (RuntimeState, UnixMs)>,
}

#[derive(Default)]
struct ChatState {
    host: Option<EndpointId>,
    head: ChatSeq,
    messages: BTreeMap<MessageId, Message>,
    /// Latest log sequence affecting each message (`Posted`, `Revised`,
    /// `Recovered`, `TakenBack`).
    last_seq: HashMap<MessageId, ChatSeq>,
    joined: HashMap<AgentId, Vec<MessageId>>,
    names: HashMap<MessageId, String>,
    work: BTreeMap<MessageId, ThreadWork>,
}

impl ChatState {
    fn apply(&mut self, record: &ChatRecord) {
        if record.seq <= self.head {
            return;
        }
        self.head = record.seq;
        match &record.event {
            ChatEvent::Posted { .. } => {
                if let Some(message) = Message::from_record(record) {
                    let id = message.id;
                    self.messages.insert(id, message);
                    self.last_seq.insert(id, record.seq);
                }
            }
            ChatEvent::Reparented { message, parent } => {
                if let Some(msg) = self.messages.get_mut(message) {
                    msg.parent = *parent;
                }
            }
            ChatEvent::Edited { message, .. } | ChatEvent::Deleted { message } => {
                if let Some(msg) = self.messages.get_mut(message) {
                    msg.apply_revision(record);
                }
            }
            ChatEvent::Revised { message, .. }
            | ChatEvent::TakenBack { message, .. }
            | ChatEvent::Recovered { message, .. } => {
                if let Some(msg) = self.messages.get_mut(message) {
                    msg.apply_revision(record);
                    self.last_seq.insert(*message, record.seq);
                }
            }
            ChatEvent::Joined { thread, agent } => {
                let threads = self.joined.entry(*agent).or_default();
                if !threads.contains(thread) {
                    threads.push(*thread);
                }
            }
            ChatEvent::Left { thread, agent } => {
                if let Some(threads) = self.joined.get_mut(agent) {
                    threads.retain(|t| t != thread);
                }
            }
            ChatEvent::Named { thread, name } => {
                if name.is_empty() {
                    self.names.remove(thread);
                } else {
                    self.names.insert(*thread, name.clone());
                }
            }
            ChatEvent::Project { .. } => {}
        }
    }

    fn effective_seq(&self, msg: &Message) -> ChatSeq {
        self.last_seq.get(&msg.id).copied().unwrap_or(msg.id.0)
    }

    /// Mirrors `HostChat::thread_of`: a root's direct child is a thread root;
    /// anything deeper hangs under that thread.
    fn thread_of(&self, id: MessageId) -> MessageId {
        let mut cursor = id;
        loop {
            let Some(parent) = self.messages.get(&cursor).and_then(|msg| msg.parent) else {
                return cursor;
            };
            if self
                .messages
                .get(&parent)
                .is_none_or(|msg| msg.parent.is_none())
            {
                return cursor;
            }
            cursor = parent;
        }
    }

    fn thread_name(&self, id: MessageId) -> Option<String> {
        self.names
            .get(&id)
            .filter(|name| !name.is_empty())
            .cloned()
            .or_else(|| self.messages.get(&id).map(Message::headline))
    }

    fn agent_primary_thread(
        &self,
        agent: AgentId,
        entry: Option<&AgentEntry>,
    ) -> Option<MessageId> {
        if let Some(threads) = self.joined.get(&agent)
            && let Some(&latest) = threads.last()
        {
            return Some(latest);
        }
        let origin = entry?
            .spec
            .props
            .get("origin")?
            .strip_prefix("chat:")?
            .parse::<u64>()
            .ok()?;
        let id = MessageId(ChatSeq(origin));
        self.messages.contains_key(&id).then(|| self.thread_of(id))
    }

    fn is_message_in_scope(&self, msg: &Message, target: WaitTarget) -> bool {
        if let Some(thread) = target.thread
            && self.thread_of(msg.id) != thread
        {
            return false;
        }
        if let Some(agent) = target.agent {
            match msg.from {
                Address::Agent(from) | Address::System { agent: from, .. } => {
                    if from != agent {
                        return false;
                    }
                }
                Address::StartFailed => {
                    if target.thread.is_none() {
                        return false;
                    }
                }
                Address::User => {
                    let joined = self
                        .joined
                        .get(&agent)
                        .is_some_and(|threads| threads.contains(&self.thread_of(msg.id)));
                    if !joined {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn latest_user_seq(&self, target: WaitTarget) -> ChatSeq {
        self.messages
            .values()
            .filter(|msg| msg.from == Address::User && self.is_message_in_scope(msg, target))
            .map(|msg| msg.id.0)
            .max()
            .unwrap_or(ChatSeq::ZERO)
    }
}

async fn fetch_directory_snapshot(paths: &RuntimePaths) -> Result<DirectorySnapshot> {
    let (snapshot, mut client) = fetch_directory(paths).await?;
    let _ = client.shutdown().await;
    Ok(snapshot)
}

async fn catch_up_chat_snapshot(paths: &RuntimePaths) -> Result<ChatState> {
    let (state, mut client) = catch_up_chat(paths).await?;
    let _ = client.shutdown().await;
    Ok(state)
}

async fn fetch_directory(paths: &RuntimePaths) -> Result<(DirectorySnapshot, Client)> {
    let mut client = Client::connect(paths).await?;
    client.open(&worksets::Open::Session).await?;

    let mut machine_seed = None;
    let mut agent_counter = None;
    let mut worksets = BTreeMap::new();
    let mut agents = BTreeMap::new();
    let mut statuses = HashMap::new();
    let mut got_directory = false;
    let mut got_auth = false;
    let mut got_quota = false;

    loop {
        if got_directory && got_auth && got_quota {
            match tokio::time::timeout(INITIAL_STATUS_DRAIN, client.recv::<worksets::ServerFrame>())
                .await
            {
                Ok(Ok(frame)) => {
                    apply_worksets_frame(
                        frame,
                        &mut machine_seed,
                        &mut agent_counter,
                        &mut worksets,
                        &mut agents,
                        &mut statuses,
                        &mut got_directory,
                        &mut got_auth,
                        &mut got_quota,
                    );
                    continue;
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => break,
            }
        }
        let frame = client.recv::<worksets::ServerFrame>().await?;
        apply_worksets_frame(
            frame,
            &mut machine_seed,
            &mut agent_counter,
            &mut worksets,
            &mut agents,
            &mut statuses,
            &mut got_directory,
            &mut got_auth,
            &mut got_quota,
        );
    }

    Ok((
        DirectorySnapshot {
            machine_seed: machine_seed.context("worksets session omitted Hello")?,
            agent_counter: agent_counter.unwrap_or(0),
            worksets,
            agents,
            statuses,
        },
        client,
    ))
}

#[expect(clippy::too_many_arguments)]
fn apply_worksets_frame(
    frame: worksets::ServerFrame,
    machine_seed: &mut Option<u64>,
    agent_counter: &mut Option<u64>,
    worksets: &mut BTreeMap<WorksetId, WorksetEntry>,
    agents: &mut BTreeMap<AgentId, AgentEntry>,
    statuses: &mut HashMap<AgentId, (RuntimeState, UnixMs)>,
    got_directory: &mut bool,
    got_auth: &mut bool,
    got_quota: &mut bool,
) {
    match frame {
        worksets::ServerFrame::Hello {
            machine_seed: seed,
            agent_counter: counter,
            ..
        } => {
            *machine_seed = Some(seed);
            *agent_counter = Some(counter);
        }
        worksets::ServerFrame::Directory {
            worksets: ws,
            agents: ag,
        } => {
            *worksets = ws.into_iter().map(|e| (e.workset, e)).collect();
            *agents = ag.into_iter().map(|e| (e.agent, e)).collect();
            *got_directory = true;
        }
        worksets::ServerFrame::Auth { .. } => *got_auth = true,
        worksets::ServerFrame::QuotaUsage { .. } => *got_quota = true,
        worksets::ServerFrame::Status {
            agent_id,
            state,
            since,
        } => {
            statuses.insert(agent_id, (state, since));
        }
        worksets::ServerFrame::Workset(entry) => {
            worksets.insert(entry.workset, entry);
        }
        worksets::ServerFrame::WorksetDeleted { workset } => {
            worksets.remove(&workset);
            agents.retain(|_, entry| entry.workset != workset);
        }
        worksets::ServerFrame::Agent(entry) => {
            agents.insert(entry.agent, entry);
        }
        worksets::ServerFrame::Rows { .. } | worksets::ServerFrame::Live { .. } => {}
    }
}

async fn catch_up_chat(paths: &RuntimePaths) -> Result<(ChatState, Client)> {
    let mut client = Client::connect(paths).await?;
    client.open(&chat::Open::Session).await?;

    let mut state = ChatState::default();
    let ChatServerFrame::Hello { host, .. } = client.recv::<ChatServerFrame>().await? else {
        anyhow::bail!("chat session did not start with Hello");
    };
    state.host = Some(host);
    client
        .send(&ChatClientFrame::Follow {
            since: ChatSeq::ZERO,
        })
        .await?;

    loop {
        match client.recv::<ChatServerFrame>().await? {
            ChatServerFrame::Messages { records } => {
                for record in &records {
                    state.apply(record);
                }
            }
            ChatServerFrame::Work { threads } => {
                state.work = threads.into_iter().collect();
                break;
            }
            ChatServerFrame::Hello { .. } => {}
        }
    }
    Ok((state, client))
}

#[expect(clippy::too_many_arguments)]
async fn wait_for_reply(
    paths: &RuntimePaths,
    mut snapshot: DirectorySnapshot,
    mut worksets_client: Client,
    target: WaitTarget,
    after: Option<ChatSeq>,
    timeout: Duration,
    include_flashes: bool,
    json: bool,
) -> Result<WaitOutcome> {
    let (mut chat, mut chat_client) = catch_up_chat(paths).await?;
    if let Some(thread) = target.thread {
        anyhow::ensure!(
            chat.messages.contains_key(&thread),
            "no thread #{} on this host",
            thread.0.0
        );
    }

    let min_seq = after.unwrap_or_else(|| chat.latest_user_seq(target));
    let result = wait_for_reply_loop(
        &mut snapshot,
        &mut worksets_client,
        &mut chat,
        &mut chat_client,
        target,
        min_seq,
        timeout,
        include_flashes,
        json,
    )
    .await;
    let _ = chat_client.shutdown().await;
    let _ = worksets_client.shutdown().await;
    result
}

#[expect(clippy::too_many_arguments)]
async fn wait_for_reply_loop(
    snapshot: &mut DirectorySnapshot,
    worksets_client: &mut Client,
    chat: &mut ChatState,
    chat_client: &mut Client,
    target: WaitTarget,
    min_seq: ChatSeq,
    timeout: Duration,
    include_flashes: bool,
    json: bool,
) -> Result<WaitOutcome> {
    let mut flashes = Vec::new();

    for (seq, msg) in sorted_messages_after(chat, target, min_seq) {
        if let Some(outcome) = classify_message(
            snapshot,
            chat,
            &msg,
            seq,
            include_flashes,
            json,
            &mut flashes,
        ) {
            return Ok(outcome);
        }
    }

    if timeout.is_zero() {
        anyhow::bail!(
            "timed out waiting for reply (no reply after seq {})",
            min_seq.0
        );
    }

    let deadline = Instant::now() + timeout;
    let mut pending_failure: Option<(Instant, String)> = None;

    loop {
        let sleep_until = pending_failure
            .as_ref()
            .map(|(grace, _)| (*grace).min(deadline))
            .unwrap_or(deadline);

        tokio::select! {
            frame = chat_client.recv::<ChatServerFrame>() => {
                match frame? {
                    ChatServerFrame::Messages { records } => {
                        for record in records {
                            chat.apply(&record);
                            let Some(id) = affected_message_id(&record) else { continue };
                            let Some(msg) = chat.messages.get(&id).cloned() else { continue };
                            if msg.revision == Revision::Deleted
                                || !chat.is_message_in_scope(&msg, target)
                                || record.seq <= min_seq
                            {
                                continue;
                            }
                            if let Some(outcome) = classify_message(
                                snapshot,
                                chat,
                                &msg,
                                record.seq,
                                include_flashes,
                                json,
                                &mut flashes,
                            ) {
                                return Ok(outcome);
                            }
                        }
                    }
                    ChatServerFrame::Work { threads } => {
                        let next_work: BTreeMap<MessageId, ThreadWork> = threads.into_iter().collect();
                        if let Some(thread) = target.thread
                            && next_work.get(&thread) == Some(&ThreadWork::StoppedByYou)
                            && chat.work.get(&thread) != Some(&ThreadWork::StoppedByYou)
                        {
                            return Ok(WaitOutcome::Stop(StopEvent {
                                thread: thread.0.0,
                                seq: chat.head.0,
                                kind: "stopped_by_user",
                                text: "stopped by user".to_owned(),
                                flashes,
                            }));
                        }
                        chat.work = next_work;
                    }
                    ChatServerFrame::Hello { .. } => {}
                }
            }
            frame = worksets_client.recv::<worksets::ServerFrame>() => {
                if let worksets::ServerFrame::Status { agent_id, state, since } = frame? {
                    snapshot.statuses.insert(agent_id, (state.clone(), since));
                    let matches_target = target.agent.is_some_and(|a| a == agent_id)
                        || target.thread.is_some_and(|t| {
                            chat.joined.get(&agent_id).is_some_and(|threads| threads.contains(&t))
                        });
                    if matches_target {
                        if state.archived {
                            let thread = target
                                .thread
                                .or_else(|| chat.agent_primary_thread(agent_id, snapshot.agents.get(&agent_id)))
                                .map(|t| t.0.0)
                                .unwrap_or(0);
                            return Ok(WaitOutcome::Stop(StopEvent {
                                thread,
                                seq: chat.head.0,
                                kind: "stopped_by_user",
                                text: "agent archived/stopped".to_owned(),
                                flashes,
                            }));
                        }
                        match &state.inference {
                            InferenceState::Failed { error, .. }
                            | InferenceState::Lost { error, .. }
                                if pending_failure.is_none() =>
                            {
                                pending_failure =
                                    Some((Instant::now() + STOP_MAIL_GRACE, error.clone()));
                            }
                            _ => {}
                        }
                    }
                }
            }
            () = tokio::time::sleep_until(sleep_until) => {
                if let Some((_, error)) = pending_failure.take()
                    && Instant::now() < deadline
                {
                    let thread = target
                        .thread
                        .or_else(|| target.agent.and_then(|a| chat.agent_primary_thread(a, snapshot.agents.get(&a))))
                        .map(|t| t.0.0)
                        .unwrap_or(0);
                    return Ok(WaitOutcome::Stop(StopEvent {
                        thread,
                        seq: chat.head.0,
                        kind: "stop",
                        text: error,
                        flashes,
                    }));
                }
                anyhow::bail!(
                    "timed out after {}s waiting for reply (last seq {})",
                    timeout.as_secs(),
                    chat.head.0
                );
            }
        }
    }
}

#[expect(clippy::too_many_arguments)]
async fn follow_replies(
    paths: &RuntimePaths,
    mut snapshot: DirectorySnapshot,
    mut worksets_client: Client,
    target: WaitTarget,
    after: Option<ChatSeq>,
    timeout: Duration,
    include_flashes: bool,
    json: bool,
) -> Result<()> {
    let (mut chat, mut chat_client) = catch_up_chat(paths).await?;
    if let Some(thread) = target.thread {
        anyhow::ensure!(
            chat.messages.contains_key(&thread),
            "no thread #{} on this host",
            thread.0.0
        );
    }

    let mut min_seq = after.unwrap_or_else(|| chat.latest_user_seq(target));
    for (seq, msg) in sorted_messages_after(&chat, target, min_seq) {
        emit_follow_message(&snapshot, &chat, &msg, seq, include_flashes, json)?;
        min_seq = min_seq.max(seq);
    }

    if !timeout.is_zero() {
        let deadline = Instant::now() + timeout;
        loop {
            tokio::select! {
                frame = chat_client.recv::<ChatServerFrame>() => {
                    if let ChatServerFrame::Messages { records } = frame? {
                        for record in records {
                            chat.apply(&record);
                            let Some(id) = affected_message_id(&record) else { continue };
                            let Some(msg) = chat.messages.get(&id).cloned() else { continue };
                            if msg.revision == Revision::Deleted
                                || !chat.is_message_in_scope(&msg, target)
                                || record.seq <= min_seq
                            {
                                continue;
                            }
                            emit_follow_message(&snapshot, &chat, &msg, record.seq, include_flashes, json)?;
                            min_seq = min_seq.max(record.seq);
                        }
                    }
                }
                frame = worksets_client.recv::<worksets::ServerFrame>() => {
                    if let worksets::ServerFrame::Status { agent_id, state, since } = frame? {
                        snapshot.statuses.insert(agent_id, (state, since));
                    }
                }
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
    }
    let _ = chat_client.shutdown().await;
    let _ = worksets_client.shutdown().await;
    Ok(())
}

fn affected_message_id(record: &ChatRecord) -> Option<MessageId> {
    match &record.event {
        ChatEvent::Posted { .. } => Some(MessageId(record.seq)),
        ChatEvent::Revised { message, .. } | ChatEvent::Recovered { message, .. } => Some(*message),
        _ => None,
    }
}

fn sorted_messages_after(
    chat: &ChatState,
    target: WaitTarget,
    min_seq: ChatSeq,
) -> Vec<(ChatSeq, Message)> {
    let mut existing = chat
        .messages
        .values()
        .filter(|msg| msg.revision != Revision::Deleted)
        .filter(|msg| chat.is_message_in_scope(msg, target))
        .map(|msg| (chat.effective_seq(msg), msg.clone()))
        .filter(|(seq, _)| *seq > min_seq)
        .collect::<Vec<_>>();
    existing.sort_by_key(|(seq, msg)| (*seq, msg.id));
    existing
}

fn emit_follow_message(
    snapshot: &DirectorySnapshot,
    chat: &ChatState,
    msg: &Message,
    seq: ChatSeq,
    include_flashes: bool,
    json: bool,
) -> Result<()> {
    let thread = chat.thread_of(msg.id);
    if msg.voice == Voice::Flash
        || (matches!(msg.from, Address::Agent(_)) && msg.to_user() && !msg.pings_user())
    {
        if !include_flashes {
            return Ok(());
        }
        let view = flash_view(snapshot, msg, seq);
        if json {
            print_json(&view)?;
        } else {
            println!(
                "[{} #{} {}] {}",
                view.kind, view.message, view.from, view.text
            );
        }
        return Ok(());
    }
    if msg.from == Address::User
        || (matches!(msg.from, Address::Agent(_)) && msg.pings_user())
        || matches!(msg.from, Address::System { .. } | Address::StartFailed)
    {
        let mut view = message_view(snapshot, chat, msg, thread);
        view.seq = seq.0;
        if json {
            print_json(&view)?;
        } else {
            println!(
                "[{} #{} seq={} {}] {}",
                view.kind, view.message, view.seq, view.from, view.text
            );
        }
    }
    Ok(())
}

fn classify_message(
    snapshot: &DirectorySnapshot,
    chat: &ChatState,
    msg: &Message,
    seq: ChatSeq,
    include_flashes: bool,
    json: bool,
    flashes: &mut Vec<FlashView>,
) -> Option<WaitOutcome> {
    let thread = chat.thread_of(msg.id).0.0;
    match msg.from {
        Address::Agent(agent_id) => {
            if msg.voice == Voice::Flash || (msg.to_user() && !msg.pings_user()) {
                let flash = flash_view(snapshot, msg, seq);
                if include_flashes {
                    if !json {
                        eprintln!(
                            "[{} #{} {}] {}",
                            flash.kind, flash.message, flash.from, flash.text
                        );
                    }
                    flashes.push(flash);
                }
                None
            } else if msg.pings_user() {
                let handle = snapshot
                    .agents
                    .get(&agent_id)
                    .map(agent_handle)
                    .unwrap_or_else(|| agent_id.encoded());
                Some(WaitOutcome::Reply(ReplyEvent {
                    thread,
                    seq: seq.0,
                    message: msg.id.0.0,
                    from: handle.clone(),
                    agent: Some(handle),
                    agent_id: Some(agent_id.encoded()),
                    kind: "message",
                    text: msg.text(),
                    flashes: std::mem::take(flashes),
                }))
            } else {
                None
            }
        }
        Address::System { .. } | Address::StartFailed => Some(WaitOutcome::Stop(StopEvent {
            thread,
            seq: seq.0,
            kind: "stop",
            text: msg.text(),
            flashes: std::mem::take(flashes),
        })),
        Address::User => None,
    }
}

fn flash_view(snapshot: &DirectorySnapshot, msg: &Message, seq: ChatSeq) -> FlashView {
    let kind = if msg.voice == Voice::Flash {
        "flash"
    } else {
        "status"
    };
    FlashView {
        seq: seq.0,
        message: msg.id.0.0,
        from: format_address(snapshot, msg.from),
        kind: kind.to_owned(),
        text: msg.text(),
    }
}

fn message_view(
    snapshot: &DirectorySnapshot,
    chat: &ChatState,
    msg: &Message,
    thread: MessageId,
) -> MessageView {
    let (agent, agent_id) = match msg.from {
        Address::Agent(id) | Address::System { agent: id, .. } => {
            let handle = snapshot
                .agents
                .get(&id)
                .map(agent_handle)
                .unwrap_or_else(|| id.encoded());
            (Some(handle), Some(id.encoded()))
        }
        Address::User | Address::StartFailed => (None, None),
    };
    let kind = match msg.from {
        Address::System { .. } | Address::StartFailed => "stop",
        _ if msg.voice == Voice::Flash => "flash",
        Address::Agent(_) if msg.to_user() && !msg.pings_user() => "status",
        _ => "message",
    };
    MessageView {
        seq: chat.effective_seq(msg).0,
        message: msg.id.0.0,
        thread: thread.0.0,
        from: format_address(snapshot, msg.from),
        agent,
        agent_id,
        kind: kind.to_owned(),
        pings_user: msg.pings_user(),
        text: msg.text(),
    }
}

fn format_address(snapshot: &DirectorySnapshot, from: Address) -> String {
    match from {
        Address::User => "user".to_owned(),
        Address::Agent(id) => snapshot
            .agents
            .get(&id)
            .map(agent_handle)
            .unwrap_or_else(|| id.encoded()),
        Address::System { agent, stop, .. } => {
            let who = snapshot
                .agents
                .get(&agent)
                .map(agent_handle)
                .unwrap_or_else(|| agent.encoded());
            format!("system({who}: {})", stop.word())
        }
        Address::StartFailed => "system(start_failed)".to_owned(),
    }
}

/// The project named `name`, created at the caller's directory when the
/// host has none by that name.
async fn ensure_project_root(paths: &RuntimePaths, name: &str) -> Result<MessageId> {
    let name = ProjectName::parse(name).map_err(anyhow::Error::msg)?;
    if let Some(project) = client::call(
        paths,
        ResolveProject {
            name: name.as_str().to_owned(),
        },
    )
    .await?
    {
        return Ok(project.id.0);
    }
    let cwd = std::env::current_dir().context("the current directory")?;
    let place = ProjectPlace::parse(&cwd.to_string_lossy()).map_err(anyhow::Error::msg)?;
    let runtime = RuntimeCatalogue::host()
        .default_runtime()
        .context("the host offers no runtime")?
        .clone();
    let config = ProjectConfig {
        place,
        mode: ProjectMode::One,
        runtime,
    };
    let project = client::call(paths, CreateProject { name, config }).await?;
    Ok(project.0)
}

/// The user's `text` under `parent`. With `agent`, the agent joins the
/// message's thread, so the message reaches it.
async fn post_message(
    paths: &RuntimePaths,
    parent: Option<MessageId>,
    agent: Option<AgentId>,
    text: String,
) -> Result<MessageId> {
    if let Some(agent) = agent {
        return client::call(
            paths,
            TellAgent {
                agent,
                parent,
                text,
            },
        )
        .await;
    }
    client::call(
        paths,
        Post {
            client_id: uuid::Uuid::new_v4().as_u64_pair().0,
            parent,
            settles: None,
            content: vec![ContentPart::Text { text }],
        },
    )
    .await
}

fn resolve_agent<'a>(snapshot: &'a DirectorySnapshot, query: &str) -> Result<&'a AgentEntry> {
    let trimmed = query.trim();
    anyhow::ensure!(!trimmed.is_empty(), "agent id must not be empty");
    let (role_prefix, raw) = match trimmed.split_once('-') {
        Some((prefix @ ("eng" | "adv"), raw)) => (Some(prefix), raw),
        Some((other, _)) => anyhow::bail!("invalid agent handle prefix `{other}-` in `{trimmed}`"),
        None => (None, trimmed),
    };
    let domain = AgentIdDomain(snapshot.machine_seed);
    let resolved = match AgentId::from_prefix(raw, snapshot.agent_counter + 1, &domain)? {
        prefix_id::PrefixResolution::Unique(id) => id,
        prefix_id::PrefixResolution::Ambiguous { .. } => {
            anyhow::bail!("ambiguous agent id `{trimmed}`; give a longer prefix")
        }
        prefix_id::PrefixResolution::NotFound => {
            anyhow::bail!("no agent with id `{trimmed}` on this host")
        }
    };
    let entry = snapshot
        .agents
        .get(&resolved)
        .with_context(|| format!("no agent with id `{trimmed}` on this host"))?;
    if let Some(prefix) = role_prefix {
        let actual_prefix = handle_prefix(entry);
        anyhow::ensure!(
            actual_prefix == prefix,
            "agent `{trimmed}` is `{}` (role prefix `{actual_prefix}-`)",
            agent_handle(entry)
        );
    }
    Ok(entry)
}

fn parse_thread_id(input: &str) -> Result<MessageId> {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("thread:") {
        let parsed =
            ThreadId::parse(rest).with_context(|| format!("invalid thread address `{trimmed}`"))?;
        return Ok(parsed.message);
    }
    let digits = trimmed.strip_prefix('#').unwrap_or(trimmed);
    let seq: u64 = digits
        .parse()
        .with_context(|| format!("invalid thread id `{trimmed}`"))?;
    anyhow::ensure!(seq > 0, "thread id must be positive");
    Ok(MessageId(ChatSeq(seq)))
}

fn resolve_cwd(path: &camino::Utf8Path) -> Result<Utf8PathBuf> {
    if path.as_str() == "~" || path.as_str().starts_with("~/") || path.is_absolute() {
        return Ok(path.to_owned());
    }
    let abs = std::env::current_dir()
        .context("resolve current directory for `--cwd`")?
        .join(path.as_std_path());
    let normalized = normalize_path(&abs);
    Utf8PathBuf::from_path_buf(normalized)
        .map_err(|p| anyhow::anyhow!("`--cwd` path is not valid UTF-8: {}", p.display()))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn preset_role(preset: AgentPreset) -> AgentRole {
    match preset {
        AgentPreset::Engineer => AgentRole::default(),
        AgentPreset::Advisor => AgentRole::Advisor {
            intelligence: rho_agent_types::AdvisorIntelligence::Medium,
        },
    }
}

fn handle_prefix(entry: &AgentEntry) -> &'static str {
    if entry.spec.props.get("preset").map(String::as_str) == Some("advisor") {
        "adv"
    } else {
        "eng"
    }
}

fn agent_handle(entry: &AgentEntry) -> String {
    format!("{}-{}", handle_prefix(entry), entry.agent.encoded())
}

fn default_workset_root(kind: &WorksetKind) -> Utf8PathBuf {
    match kind {
        WorksetKind::Isolated => Utf8PathBuf::from(rho_fs_view::MOUNT_ROOT),
        WorksetKind::Host { cwd } => cwd.clone(),
    }
}

fn workset_cwd(entry: &AgentEntry, worksets: &BTreeMap<WorksetId, WorksetEntry>) -> String {
    if let Some(cwd) = agent_cwd(&entry.spec.props) {
        return cwd.to_string();
    }
    worksets
        .get(&entry.workset)
        .map(|ws| default_workset_root(&ws.kind).into_string())
        .unwrap_or_else(|| rho_fs_view::MOUNT_ROOT.to_owned())
}

fn build_spawn_in_code(
    function: &Function,
    cwd: &camino::Utf8Path,
    runtime: &RuntimeLabel,
    preset: AgentPreset,
) -> String {
    let code = snippets::new_agent_in(function, cwd, runtime, None);
    if preset == AgentPreset::Engineer {
        code
    } else {
        splice_preset_arg(&code, preset)
    }
}

fn splice_preset_arg(code: &str, preset: AgentPreset) -> String {
    let trimmed = code.trim_end();
    if let Some(prefix) = trimmed.strip_suffix(')') {
        format!("{prefix}, preset={})\n", snippets::py_str(preset.as_str()))
    } else {
        code.to_owned()
    }
}

async fn read_optional_input(arg: Option<&str>, use_stdin: bool) -> Result<Option<String>> {
    let read_stdin = use_stdin || matches!(arg, Some("-"));
    let text = if read_stdin {
        let mut buf = String::new();
        tokio::io::stdin()
            .take((MAX_INPUT_BYTES + 1) as u64)
            .read_to_string(&mut buf)
            .await
            .context("read prompt from stdin")?;
        Some(buf)
    } else {
        arg.map(str::to_owned)
    };
    if let Some(text) = &text {
        anyhow::ensure!(!text.trim().is_empty(), "message text must not be empty");
        anyhow::ensure!(
            text.len() <= MAX_INPUT_BYTES,
            "message text exceeds {MAX_INPUT_BYTES} bytes"
        );
    }
    Ok(text)
}

async fn read_required_input(arg: Option<&str>, use_stdin: bool) -> Result<String> {
    read_optional_input(arg, use_stdin)
        .await?
        .context("message text is required (pass `<TEXT>`, `-`, or `--stdin`)")
}

fn describe_inference(inference: &InferenceState) -> String {
    match inference {
        InferenceState::Idle => "idle".to_owned(),
        InferenceState::Responding => "responding".to_owned(),
        InferenceState::Retrying { error, .. } => format!("retrying: {error}"),
        InferenceState::Failed { error, .. } => format!("failed: {error}"),
        InferenceState::Lost { error, .. } => format!("lost: {error}"),
    }
}

fn format_log_event(event: &LogEvent, max_chars: usize) -> String {
    let raw = match event {
        LogEvent::WorksetCreated { kind, .. } => format!("WorksetCreated {kind:?}"),
        LogEvent::Evaluated { code, .. } => format!("Evaluated {}", one_line_cut(code, max_chars)),
        LogEvent::AgentCreated { spec, .. } => {
            format!("AgentCreated runtime={}", spec.runtime_label())
        }
        LogEvent::AgentChanged { spec, .. } => {
            format!("AgentChanged runtime={}", spec.runtime_label())
        }
        LogEvent::CompactionRequested { .. } => "CompactionRequested".to_owned(),
        LogEvent::Request {
            inputs,
            output,
            compaction,
            ..
        } => {
            let input_summary = inputs
                .iter()
                .map(|i| format!("{:?}:{}", i.role, one_line_cut(&i.text, max_chars)))
                .collect::<Vec<_>>()
                .join(" | ");
            if output.is_empty() {
                format!("Request(compaction={compaction}) [{input_summary}]")
            } else {
                format!(
                    "Request(compaction={compaction}) output={} [{input_summary}]",
                    one_line_cut(output, max_chars)
                )
            }
        }
        LogEvent::CellEnded { end, .. } => {
            format!("CellEnded call={} failed={}", end.call.as_str(), end.failed)
        }
        LogEvent::CallsEnded { calls, .. } => format!("CallsEnded count={}", calls.len()),
        LogEvent::Replied {
            items,
            compacted,
            context_used,
            ..
        } => {
            let summary = items
                .iter()
                .map(|item| match item {
                    Item::Text { text, .. } => format!("text:{}", one_line_cut(text, max_chars)),
                    Item::Reasoning { text } => {
                        format!("reasoning:{}", one_line_cut(text, max_chars))
                    }
                    Item::ToolCall {
                        name, arguments, ..
                    } => format!("tool:{name}({})", one_line_cut(arguments, max_chars)),
                })
                .collect::<Vec<_>>()
                .join(" | ");
            format!("Replied(compacted={compacted}, context={context_used:?}) [{summary}]")
        }
        LogEvent::Rewound { to, .. } => format!("Rewound to #{}", to.0),
        LogEvent::Failed {
            error, retrying, ..
        } => format!(
            "Failed(retrying={retrying}) {}",
            one_line_cut(error, max_chars)
        ),
        LogEvent::Stopped { error, .. } => {
            format!("Stopped {}", one_line_cut(error, max_chars))
        }
        LogEvent::ClaudeMessage { speaker, text, .. } => {
            let who = match speaker {
                Speaker::User => "user",
                Speaker::Agent => "agent",
                Speaker::Assistant => "assistant",
            };
            format!("ClaudeMessage({who}) {}", one_line_cut(text, max_chars))
        }
        LogEvent::Cancelled { .. } => "Cancelled".to_owned(),
        LogEvent::MailWritten {
            message, version, ..
        } => {
            format!(
                "MailWritten #{} (version {:?})",
                message.0.0,
                version.map(|v| v.0)
            )
        }
    };
    truncate_str(&raw, max_chars)
}

fn log_event_json(pos: Wpos, event: &LogEvent) -> serde_json::Value {
    match event {
        LogEvent::WorksetCreated { kind, code, at } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "workset_created",
            "workset_kind": format!("{kind:?}"),
            "code": code,
        }),
        LogEvent::Evaluated { code, at } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "evaluated",
            "code": code,
        }),
        LogEvent::AgentCreated { spec, at, .. } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "agent_created",
            "runtime": spec.runtime_label(),
            "props": spec.props,
        }),
        LogEvent::AgentChanged { spec, at, .. } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "agent_changed",
            "runtime": spec.runtime_label(),
            "props": spec.props,
        }),
        LogEvent::CompactionRequested { at, .. } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "compaction_requested",
        }),
        LogEvent::Request {
            inputs,
            answers,
            compaction,
            output,
            at,
            ..
        } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "request",
            "answers": answers,
            "compaction": compaction,
            "output": output,
            "inputs": inputs.iter().map(|i| json!({
                "role": format!("{:?}", i.role).to_ascii_lowercase(),
                "message": i.message.map(|m| m.0.0),
                "text": i.text,
            })).collect::<Vec<_>>(),
        }),
        LogEvent::CellEnded { end, at, .. } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "cell_ended",
            "call": end.call.as_str(),
            "failed": end.failed,
        }),
        LogEvent::CallsEnded { calls, at, .. } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "calls_ended",
            "calls": calls.iter().map(|c| json!({
                "id": c.id,
                "output": c.output,
                "failed": c.failed,
            })).collect::<Vec<_>>(),
        }),
        LogEvent::Replied {
            items,
            compacted,
            usage,
            context_used,
            at,
            ..
        } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "replied",
            "compacted": compacted,
            "context_used": context_used,
            "usage": usage.as_ref().map(|u| json!({
                "model": u.model,
                "input_tokens": u.input_tokens,
                "cache_read_tokens": u.cache_read_tokens,
                "cache_write_tokens": u.cache_write_tokens,
                "cache_write_1h_tokens": u.cache_write_1h_tokens,
                "output_tokens": u.output_tokens,
            })),
            "items": items.iter().map(|item| match item {
                Item::Text { text, phase } => json!({
                    "type": "text",
                    "phase": phase.map(|p| format!("{p:?}")),
                    "text": text,
                }),
                Item::Reasoning { text } => json!({
                    "type": "reasoning",
                    "text": text,
                }),
                Item::ToolCall { id, name, arguments, .. } => json!({
                    "type": "tool_call",
                    "id": id,
                    "name": name,
                    "arguments": arguments,
                }),
            }).collect::<Vec<_>>(),
        }),
        LogEvent::Rewound { to, at, .. } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "rewound",
            "to": to.0,
        }),
        LogEvent::Failed {
            text,
            error,
            retrying,
            at,
            ..
        } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "failed",
            "text": text,
            "error": error,
            "retrying": retrying,
        }),
        LogEvent::Stopped { error, at, .. } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "stopped",
            "error": error,
        }),
        LogEvent::ClaudeMessage {
            speaker, text, at, ..
        } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "claude_message",
            "speaker": match speaker {
                Speaker::User => "user",
                Speaker::Agent => "agent",
                Speaker::Assistant => "assistant",
            },
            "text": text,
        }),
        LogEvent::Cancelled { at, .. } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "cancelled",
        }),
        LogEvent::MailWritten {
            message,
            version,
            at,
            ..
        } => json!({
            "pos": pos.0,
            "at": at.0,
            "kind": "mail_written",
            "message": message.0.0,
            "version": version.map(|v| v.0),
        }),
    }
}

fn one_line_cut(text: &str, max_chars: usize) -> String {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_str(&single, max_chars)
}

fn truncate_str(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return text.to_owned();
    }
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Args, Command};

    fn parse(args: &[&str]) -> std::result::Result<AgentArgs, clap::Error> {
        let parsed = Args::try_parse(args.iter().map(|s| (*s).to_owned()))?;
        match parsed.command {
            Command::Agent(agent) => Ok(agent),
            _ => panic!("expected Command::Agent"),
        }
    }

    #[test]
    fn cli_parses_spawn_send_recv_and_management_commands() {
        let spawn = parse(&[
            "agent",
            "spawn",
            "--runtime",
            "sol-medium",
            "--cwd",
            ".",
            "--wait",
            "--timeout",
            "45",
            "--json",
            "hello world",
        ])
        .unwrap();
        assert!(spawn.json);
        let AgentCommand::Spawn(spawn) = spawn.command else {
            panic!("expected spawn");
        };
        assert_eq!(spawn.runtime.as_str(), "sol-medium");
        assert_eq!(spawn.cwd.as_deref().map(|p| p.as_str()), Some("."));
        assert!(spawn.wait);
        assert_eq!(spawn.timeout, 45);
        assert_eq!(spawn.prompt.as_deref(), Some("hello world"));

        let spawn_opus =
            parse(&["agent", "spawn", "--runtime", "opus-high", "--isolated"]).unwrap();
        let AgentCommand::Spawn(spawn_opus) = spawn_opus.command else {
            panic!("expected spawn");
        };
        assert_eq!(spawn_opus.runtime.as_str(), "opus-high");
        assert!(spawn_opus.isolated);

        let spawn_fable = parse(&["agent", "spawn", "--runtime", "fable-high", "--stdin"]).unwrap();
        let AgentCommand::Spawn(spawn_fable) = spawn_fable.command else {
            panic!("expected spawn");
        };
        assert_eq!(spawn_fable.runtime.as_str(), "fable-high");
        assert!(spawn_fable.stdin);

        assert!(parse(&["agent", "spawn", "--runtime", "bogus-high"]).is_err());
        assert!(parse(&["agent", "spawn", "--cwd", ".", "--isolated"]).is_err());

        let send = parse(&["agent", "send", "--thread", "#42", "--wait", "ping"]).unwrap();
        let AgentCommand::Send(send) = send.command else {
            panic!("expected send");
        };
        assert_eq!(send.thread.as_deref(), Some("#42"));
        assert!(send.wait);
        assert_eq!(send.text.as_deref(), Some("ping"));
        assert!(parse(&["agent", "send", "no-target"]).is_err());

        let recv = parse(&[
            "agent",
            "wait",
            "--thread",
            "42",
            "--after",
            "41",
            "--include-flashes",
            "--timeout",
            "10",
        ])
        .unwrap();
        let AgentCommand::Recv(recv) = recv.command else {
            panic!("expected recv");
        };
        assert_eq!(recv.thread.as_deref(), Some("42"));
        assert_eq!(recv.after, Some(41));
        assert!(recv.include_flashes);
        assert_eq!(recv.timeout, 10);

        assert!(matches!(
            parse(&["agent", "ls"]).unwrap().command,
            AgentCommand::List(_)
        ));
        assert!(matches!(
            parse(&["agent", "status", "eng-abc"]).unwrap().command,
            AgentCommand::Status(_)
        ));
        assert!(matches!(
            parse(&["agent", "transcript", "eng-abc", "--last", "10"])
                .unwrap()
                .command,
            AgentCommand::Transcript(_)
        ));
        assert!(matches!(
            parse(&["agent", "cancel", "eng-abc"]).unwrap().command,
            AgentCommand::Cancel(_)
        ));
        assert!(matches!(
            parse(&["agent", "stop", "eng-abc", "--delete-workset"])
                .unwrap()
                .command,
            AgentCommand::Stop(StopArgs {
                delete_workset: true,
                ..
            })
        ));
    }

    #[test]
    fn thread_id_parser_accepts_numbers_hashes_and_addresses() {
        assert_eq!(parse_thread_id("42").unwrap(), MessageId(ChatSeq(42)));
        assert_eq!(parse_thread_id("#42").unwrap(), MessageId(ChatSeq(42)));
        let host = EndpointId([0; 32]);
        let addr = ThreadId {
            host,
            message: MessageId(ChatSeq(99)),
        }
        .to_string();
        assert_eq!(parse_thread_id(&addr).unwrap(), MessageId(ChatSeq(99)));
        assert!(parse_thread_id("0").is_err());
        assert!(parse_thread_id("not-a-thread").is_err());
    }

    #[test]
    fn resolve_agent_handles_full_handles_raw_ids_and_unique_prefixes() {
        let seed = 42;
        let domain = AgentIdDomain(seed);
        let a1 = AgentId::from_counter(1, &domain).unwrap();
        let a2 = AgentId::from_counter(2, &domain).unwrap();
        let ws = WorksetId([1; 6]);
        let mut agents = BTreeMap::new();
        agents.insert(
            a1,
            AgentEntry {
                agent: a1,
                workset: ws,
                spec: rho_agents_client::protocol::worksets::SpecView {
                    runtime: rho_agent_types::RuntimeKind::Rho,
                    model: "sol".into(),
                    effort: "medium".into(),
                    props: Default::default(),
                },
                created_at: UnixMs(1),
            },
        );
        agents.insert(
            a2,
            AgentEntry {
                agent: a2,
                workset: ws,
                spec: rho_agents_client::protocol::worksets::SpecView {
                    runtime: rho_agent_types::RuntimeKind::Claude,
                    model: "fable".into(),
                    effort: "high".into(),
                    props: [("preset".to_owned(), "advisor".to_owned())]
                        .into_iter()
                        .collect(),
                },
                created_at: UnixMs(2),
            },
        );
        let snapshot = DirectorySnapshot {
            machine_seed: seed,
            agent_counter: 2,
            worksets: BTreeMap::new(),
            agents,
            statuses: HashMap::new(),
        };

        assert_eq!(resolve_agent(&snapshot, &a1.encoded()).unwrap().agent, a1);
        assert_eq!(
            resolve_agent(&snapshot, &format!("eng-{}", a1.encoded()))
                .unwrap()
                .agent,
            a1
        );
        assert_eq!(
            resolve_agent(&snapshot, &format!("adv-{}", a2.encoded()))
                .unwrap()
                .agent,
            a2
        );
        // Unique prefix match
        let p1 = &a1.encoded()[..6];
        let p2 = &a2.encoded()[..6];
        assert_ne!(p1, p2);
        assert_eq!(resolve_agent(&snapshot, p1).unwrap().agent, a1);
        assert_eq!(
            resolve_agent(&snapshot, &format!("adv-{p2}"))
                .unwrap()
                .agent,
            a2
        );
        // Mismatched role prefix rejected
        assert!(resolve_agent(&snapshot, &format!("adv-{}", a1.encoded())).is_err());
        assert!(resolve_agent(&snapshot, &format!("pm-{}", a1.encoded())).is_err());
    }

    #[test]
    fn chat_state_folds_threads_revisions_flashes_and_cursors() {
        let domain = AgentIdDomain(7);
        let agent = AgentId::from_counter(1, &domain).unwrap();
        let snapshot = DirectorySnapshot {
            machine_seed: 7,
            agent_counter: 1,
            worksets: BTreeMap::new(),
            agents: BTreeMap::new(),
            statuses: HashMap::new(),
        };
        let mut chat = ChatState::default();
        // Root #1 ("cli"), thread #2 (user prompt), flash #3, message #4,
        // revised #5, the agent joined to #2 at #6
        let records = [
            ChatRecord {
                seq: ChatSeq(1),
                at: UnixMs(10),
                event: ChatEvent::Posted {
                    parent: None,
                    retry_of: None,
                    thread_reply: None,
                    from: Address::User,
                    voice: Voice::Message,
                    content: vec![ContentPart::Text { text: "cli".into() }],
                },
            },
            ChatRecord {
                seq: ChatSeq(2),
                at: UnixMs(20),
                event: ChatEvent::Posted {
                    parent: Some(MessageId(ChatSeq(1))),
                    retry_of: None,
                    thread_reply: None,
                    from: Address::User,
                    voice: Voice::Message,
                    content: vec![ContentPart::Text {
                        text: "check clock".into(),
                    }],
                },
            },
            ChatRecord {
                seq: ChatSeq(3),
                at: UnixMs(30),
                event: ChatEvent::Posted {
                    parent: Some(MessageId(ChatSeq(2))),
                    retry_of: None,
                    thread_reply: None,
                    from: Address::Agent(agent),
                    voice: Voice::Flash,
                    content: vec![ContentPart::Text {
                        text: "reading files".into(),
                    }],
                },
            },
            ChatRecord {
                seq: ChatSeq(4),
                at: UnixMs(40),
                event: ChatEvent::Posted {
                    parent: Some(MessageId(ChatSeq(2))),
                    retry_of: None,
                    thread_reply: None,
                    from: Address::Agent(agent),
                    voice: Voice::Message,
                    content: vec![ContentPart::Text {
                        text: "working".into(),
                    }],
                },
            },
            ChatRecord {
                seq: ChatSeq(5),
                at: UnixMs(50),
                event: ChatEvent::Revised {
                    message: MessageId(ChatSeq(4)),
                    author: agent,
                    content: vec![ContentPart::Text {
                        text: "done!".into(),
                    }],
                },
            },
            // `TellAgent` joins the agent to the thread of the user's words.
            ChatRecord {
                seq: ChatSeq(6),
                at: UnixMs(60),
                event: ChatEvent::Joined {
                    thread: MessageId(ChatSeq(2)),
                    agent,
                },
            },
        ];
        for r in &records {
            chat.apply(r);
        }

        assert_eq!(chat.thread_of(MessageId(ChatSeq(2))), MessageId(ChatSeq(2)));
        assert_eq!(chat.thread_of(MessageId(ChatSeq(3))), MessageId(ChatSeq(2)));
        assert_eq!(chat.thread_of(MessageId(ChatSeq(4))), MessageId(ChatSeq(2)));
        let target = WaitTarget {
            thread: Some(MessageId(ChatSeq(2))),
            agent: Some(agent),
        };
        assert_eq!(chat.latest_user_seq(target), ChatSeq(2));

        let mut flashes = Vec::new();
        // Flash does not complete wait
        let flash_msg = chat.messages.get(&MessageId(ChatSeq(3))).unwrap();
        assert!(
            classify_message(
                &snapshot,
                &chat,
                flash_msg,
                ChatSeq(3),
                true,
                true,
                &mut flashes
            )
            .is_none()
        );
        assert_eq!(flashes.len(), 1);
        assert_eq!(flashes[0].text, "reading files");

        // Revised pinging message completes wait with seq 5
        let reply_msg = chat.messages.get(&MessageId(ChatSeq(4))).unwrap();
        let outcome = classify_message(
            &snapshot,
            &chat,
            reply_msg,
            chat.effective_seq(reply_msg),
            true,
            true,
            &mut flashes,
        );
        let Some(WaitOutcome::Reply(reply)) = outcome else {
            panic!("expected reply");
        };
        assert_eq!(reply.seq, 5);
        assert_eq!(reply.message, 4);
        assert_eq!(reply.text, "done!");
        assert_eq!(reply.flashes.len(), 1);
    }

    #[test]
    fn spawn_snippets_handle_advisor_and_cwd_normalization() {
        let runtime = RuntimeLabel::parse("sol-medium").unwrap();
        let code = build_spawn_in_code(
            &Function::default(),
            camino::Utf8Path::new("/tmp/ws"),
            &runtime,
            AgentPreset::Advisor,
        );
        assert!(code.contains("preset=\"advisor\""));
        assert_eq!(
            normalize_path(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }
}
