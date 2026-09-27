//! Full-stack timing, throughput, and visible-journal checks against
//! rho-fake-model.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use camino::Utf8PathBuf;
use clap::Args as ClapArgs;
use rho_agent_types::{AgentId, AgentPos, AgentRole, ContentPart, Seq};
use rho_agents_client::protocol::transcript::{DetailBody, TranscriptEvent};
use rho_agents_client::protocol::{
    AgentCommand, ClientFrame as AgentsClientFrame, NewAgent, ServerFrame as AgentsServerFrame,
    StartMode,
};
use rho_fake_model::{REAL_TOOL_ROUNDS, Scenario};
use rho_rpc::protocol::client::Client;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};

use crate::streams::{Incoming, Streams};

const AGENTS: usize = 20;
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(ClapArgs)]
pub struct Args {
    /// Seconds for which completed turns cause another prompt.
    #[arg(long, default_value_t = 60)]
    seconds: u64,
    /// Deterministic fake-model seed.
    #[arg(long, default_value_t = 0)]
    seed: u64,
    /// Provider behavior to exercise.
    #[arg(long, value_enum, default_value_t)]
    scenario: Scenario,
    /// Sequential exchanges for real-tool-rounds.
    #[arg(long, default_value_t = REAL_TOOL_ROUNDS)]
    rounds: usize,
    /// Directory containing rho-agent-host and rho-fake-model. Defaults to the
    /// directory containing this rho-qa executable.
    #[arg(long)]
    bin_dir: Option<PathBuf>,
}

#[derive(Deserialize)]
struct FakeReady {
    openai_base_url: String,
    anthropic_base_url: String,
    pid: u32,
}

#[derive(Deserialize)]
struct FakeMetrics {
    completed_turns: u64,
    bytes_streamed: u64,
    max_input_tool_output_bytes: u64,
    request_window_us: u64,
    busy_us: u64,
    idle_us: u64,
    idle_gaps_us: Vec<u64>,
}

struct Children(Vec<Child>);

#[derive(Clone)]
enum ExpectedDetail {
    Response(Vec<rho_agents_client::protocol::transcript::Item>),
    Report(Vec<String>),
}

impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn run(args: Args) -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    ensure_network_namespace()?;
    ensure!(args.seconds > 0, "--seconds must be greater than zero");
    ensure!(args.rounds > 0, "--rounds must be greater than zero");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_async(args))
}

async fn run_async(args: Args) -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix("rho-fake-model-proof-")
        .tempdir()?;
    let home = root.path().join("home");
    let runtime = root.path().join("runtime");
    let state = root.path().join("state");
    let config = root.path().join("config");
    let cache = root.path().join("cache");
    let data = root.path().join("data");
    let claude = root.path().join("claude");
    let workspace = root.path().join("workspace");
    for path in [
        &home, &runtime, &state, &config, &cache, &data, &claude, &workspace,
    ] {
        fs::create_dir_all(path)?;
    }
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
    write_synthetic_auth(&state)?;
    init_workspace(&workspace)?;

    let bin_dir = match args.bin_dir {
        Some(path) => path,
        None => std::env::current_exe()?
            .parent()
            .context("rho-qa executable has no parent directory")?
            .to_owned(),
    };
    let fake_bin = bin_dir.join("rho-fake-model");
    let host_bin = bin_dir.join("rho-agent-host");
    ensure!(fake_bin.is_file(), "missing {}", fake_bin.display());
    ensure!(host_bin.is_file(), "missing {}", host_bin.display());
    let tree_commit = tree_commit()?;
    let proof_hash = sha256_file(&std::env::current_exe()?)?;
    let fake_hash = sha256_file(&fake_bin)?;

    let mut fake = isolated_command(&fake_bin, root.path());
    fake.args([
        "--seed",
        &args.seed.to_string(),
        "--scenario",
        args.scenario.as_str(),
        "--no-faults",
        "--rounds",
        &args.rounds.to_string(),
    ])
    .stdout(Stdio::piped())
    .stderr(File::create(root.path().join("fake-model.stderr.log"))?);
    let mut fake = fake
        .spawn()
        .with_context(|| format!("start {}", fake_bin.display()))?;
    let fake_pid = fake.id();
    let ready_line = BufReader::new(fake.stdout.take().context("fake model stdout")?)
        .lines()
        .next()
        .context("fake model exited before readiness")??;
    let mut children = Children(vec![fake]);
    let ready: FakeReady =
        serde_json::from_str(&ready_line).context("parse fake model readiness")?;
    ensure!(
        ready.pid == fake_pid,
        "fake model reported a mismatched pid"
    );

    let socket = runtime.join("rho.sock");
    let mut agent_host = isolated_command(&host_bin, root.path());
    agent_host
        .args([
            "--socket-path",
            socket.to_str().context("socket path is not UTF-8")?,
        ])
        .args(["--openai-base-url", &ready.openai_base_url])
        .args(["--anthropic-base-url", &ready.anthropic_base_url])
        .stdout(File::create(root.path().join("agent host.stdout.log"))?)
        .stderr(File::create(root.path().join("agent host.stderr.log"))?);
    let agent_host = agent_host
        .spawn()
        .with_context(|| format!("start {}", host_bin.display()))?;
    let host_pid = agent_host.id();
    children.0.push(agent_host);
    let _children = children;

    let mut client = Streams::open(connect(&socket).await?, &socket).await?;
    let initial_head = client.journal_head;
    ensure!(
        initial_head == Seq(0),
        "fresh isolated agent host journal was not empty"
    );
    println!(
        "PROOF_READY tree_commit={} proof_sha256={} fake_sha256={} scenario={} seed={}",
        tree_commit,
        proof_hash,
        fake_hash,
        args.scenario.as_str(),
        args.seed
    );
    client
        .send_agents(&AgentsClientFrame::Follow { since: Seq(0) })
        .await?;

    let repo = Utf8PathBuf::try_from(workspace).context("workspace path is not UTF-8")?;
    let agent_count = if args.scenario == Scenario::Baseline {
        AGENTS
    } else {
        1
    };
    let submitted = Instant::now();
    for index in 0..agent_count {
        client.create(NewAgent {
            role: AgentRole::default(),
            start: StartMode::NewOn {
                repo: repo.clone(),
                revset: "HEAD".into(),
            },
            mode: rho_agent_types::WorksetMode::View,
            content: Some(prompt(index, 0)),
        });
    }

    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.seconds);
    let mut agents = HashSet::new();
    let mut expected_pos: HashMap<AgentId, AgentPos> = HashMap::new();
    let mut report_at = HashMap::new();
    let mut responding = HashSet::new();
    let mut awaiting = HashSet::new();
    let mut received = HashMap::new();
    let mut delivered_ids = HashSet::new();
    let mut reported_calls = HashSet::new();
    let mut messages_sent = 0usize;
    let mut reported_output_sizes = Vec::new();
    let mut pending_details = HashMap::new();
    let mut latencies = Vec::new();
    let mut replies = 0u64;
    let mut failed = 0u64;
    let mut retrying = 0u64;
    let mut calls = 0usize;
    let mut compacted = 0u64;
    let mut clarifying = 0u64;

    let mut cycles: HashMap<AgentId, u64> = HashMap::new();
    let mut last_seq = Seq(0);
    let quiesce_deadline = deadline
        + if matches!(
            args.scenario,
            Scenario::HugeToolOutput | Scenario::FortyToolCalls
        ) {
            Duration::from_secs(600)
        } else {
            QUIESCE_TIMEOUT
        };

    loop {
        let all_idle = agents.len() == agent_count
            && responding.is_empty()
            && pending_details.is_empty()
            && (args.scenario == Scenario::Baseline || awaiting.len() == agent_count);
        if (args.scenario != Scenario::Baseline
            && all_idle
            && scenario_complete(
                args.scenario,
                args.rounds,
                failed,
                calls,
                compacted,
                clarifying,
                reported_calls.len(),
                messages_sent,
                &latencies,
            ))
            || (Instant::now() >= deadline && all_idle)
        {
            break;
        }
        ensure!(
            Instant::now() < quiesce_deadline,
            "agents did not quiesce after the drive window"
        );
        let event_timeout = if matches!(
            args.scenario,
            Scenario::HugeToolOutput | Scenario::FortyToolCalls | Scenario::SlowTrickle
        ) {
            QUIESCE_TIMEOUT
        } else {
            Duration::from_secs(10)
        };
        let message = tokio::time::timeout(event_timeout, client.recv())
            .await
            .with_context(|| {
                format!(
                    "agent host journal stalled: {}",
                    fs::read_to_string(root.path().join("agent host.stderr.log"))
                        .unwrap_or_default()
                )
            })??;
        match message {
            Incoming::Created(agent_id) => {
                agents.insert(agent_id);
                ensure!(
                    agents.len() <= agent_count,
                    "agent host created more than {agent_count} agents"
                );
            }
            Incoming::Agents(AgentsServerFrame::Log { entries }) => {
                for entry in entries {
                    ensure!(
                        entry.seq > last_seq,
                        "global journal sequence regressed: {:?} after {:?}",
                        entry.seq,
                        last_seq
                    );
                    last_seq = entry.seq;
                    let expected = expected_pos.entry(entry.agent_id).or_insert(AgentPos::ZERO);
                    ensure!(
                        entry.pos >= *expected,
                        "agent {:?} position regressed: {:?} expected {:?}",
                        entry.agent_id,
                        entry.pos,
                        expected
                    );
                    *expected = entry.pos.next();
                    match entry.event {
                        TranscriptEvent::Created { runtime, .. } => {
                            ensure!(
                                runtime
                                    == rho_agents_client::protocol::transcript::RuntimeKind::Rho,
                                "created a non-native agent"
                            );
                            agents.insert(entry.agent_id);
                        }
                        TranscriptEvent::Received { id, from, text, .. } => {
                            ensure!(from.is_none(), "unexpected peer message in isolated proof");
                            ensure!(!text.is_empty(), "empty human message");
                            ensure!(
                                received.insert((entry.agent_id, id), ()).is_none(),
                                "duplicate received message id"
                            );
                        }
                        TranscriptEvent::NotebookReport {
                            calls: answered,
                            delivered,
                            acknowledged,
                            at,
                            ..
                        } => {
                            for id in delivered {
                                ensure!(
                                    received.contains_key(&(entry.agent_id, id)),
                                    "report delivered an unknown message"
                                );
                                ensure!(
                                    delivered_ids.insert((entry.agent_id, id)),
                                    "message delivered twice"
                                );
                            }
                            for id in acknowledged {
                                ensure!(
                                    received.contains_key(&(entry.agent_id, id)),
                                    "report acknowledged an unknown message"
                                );
                            }
                            for id in &answered {
                                ensure!(
                                    reported_calls.insert((entry.agent_id, id.clone())),
                                    "provider call was answered twice"
                                );
                            }
                            if !answered.is_empty() {
                                pending_details.insert(
                                    (entry.agent_id, entry.pos),
                                    ExpectedDetail::Report(answered),
                                );
                                client
                                    .send_agents(&AgentsClientFrame::Detail {
                                        agent_id: entry.agent_id,
                                        pos: entry.pos,
                                        more: Vec::new(),
                                    })
                                    .await?;
                            }
                            report_at.insert(entry.agent_id, at);
                        }
                        TranscriptEvent::NotebookActivity {
                            responding: active, ..
                        } => {
                            if active {
                                responding.insert(entry.agent_id);
                            } else {
                                responding.remove(&entry.agent_id);
                            }
                        }
                        TranscriptEvent::AwaitingHuman { since, .. } => {
                            if since.is_some() {
                                awaiting.insert(entry.agent_id);
                                if args.scenario == Scenario::Baseline && Instant::now() < deadline
                                {
                                    let cycle = cycles.entry(entry.agent_id).or_default();
                                    *cycle += 1;
                                    client.send(AgentCommand::Send {
                                        agent_id: entry.agent_id,
                                        content: prompt(0, *cycle),
                                    });
                                }
                            } else {
                                awaiting.remove(&entry.agent_id);
                            }
                        }
                        TranscriptEvent::MessageSent { to, text, .. } => {
                            ensure!(to.is_none(), "fake agent sent mail to another agent");
                            ensure!(!text.trim().is_empty(), "empty user-facing message");
                            messages_sent += 1;
                            clarifying += u64::from(text.trim_end().ends_with('?'));
                        }
                        TranscriptEvent::Replied {
                            items,
                            compacted: did_compact,
                            at,
                            ..
                        } => {
                            // A usage row is a second Replied event, not another model step.
                            if items.is_empty() && !did_compact {
                                continue;
                            }
                            let started = report_at
                                .get(&entry.agent_id)
                                .context("Replied without preceding NotebookReport")?;
                            latencies.push(at.saturating_duration_since(*started));
                            replies += 1;
                            calls +=
                                items
                                    .iter()
                                    .filter(|item| {
                                        matches!(item,
                                rho_agents_client::protocol::transcript::Item::ToolCall { .. }
                            )
                                    })
                                    .count();
                            compacted += u64::from(did_compact);
                            if !items.is_empty() {
                                ensure!(items.iter().all(|item| matches!(item,
                                    rho_agents_client::protocol::transcript::Item::ToolCall { name, arguments, format, .. }
                                        if name == "exec" && !arguments.is_empty()
                                            && *format == rho_agents_client::protocol::transcript::ArgumentsFormat::Text
                                )), "native response contained prose or a non-exec tool");
                                pending_details.insert(
                                    (entry.agent_id, entry.pos),
                                    ExpectedDetail::Response(items),
                                );
                                client
                                    .send_agents(&AgentsClientFrame::Detail {
                                        agent_id: entry.agent_id,
                                        pos: entry.pos,
                                        more: Vec::new(),
                                    })
                                    .await?;
                            }
                        }
                        TranscriptEvent::Failed {
                            retrying: is_retrying,
                            ..
                        } => {
                            failed += 1;
                            retrying += u64::from(is_retrying);
                        }
                        _ => {}
                    }
                }
            }
            Incoming::Agents(AgentsServerFrame::Detail {
                agent_id,
                pos,
                body,
            }) => {
                let expected = pending_details
                    .remove(&(agent_id, pos))
                    .context("unexpected Detail response")?;
                match (expected, body) {
                    (ExpectedDetail::Response(visible), DetailBody::Response(detail)) => {
                        ensure!(
                            detail == visible,
                            "response detail did not match visible code calls"
                        );
                    }
                    (ExpectedDetail::Report(ids), DetailBody::Results(results)) => {
                        ensure!(results.len() == ids.len(), "report detail omitted calls");
                        for (result, id) in results.iter().zip(ids) {
                            ensure!(
                                result.id == id,
                                "report detail named a different provider call"
                            );
                            ensure!(result.status == rho_agents_client::protocol::transcript::ToolStatus::Reported,
                                "notebook report was mistaken for a successful tool result");
                            ensure!(
                                !result.output.is_empty(),
                                "empty notebook report for a provider call"
                            );
                            if args.scenario == Scenario::RealToolRounds {
                                let step = reported_output_sizes.len() + 1;
                                ensure!(
                                    result.output.contains(&format!("rho-e2e-step-{step}:ok")),
                                    "real shell command did not report its expected step marker"
                                );
                            }
                            reported_output_sizes.push(result.output.len());
                        }
                    }
                    _ => bail!("agent host Detail body did not match its journal event"),
                }
            }
            Incoming::Refused(reason) => {
                bail!("agent host refused proof action: {reason}")
            }
            _ => {}
        }
    }
    let scenario_us = submitted.elapsed().as_micros() as u64;
    ensure!(
        agents.len() == agent_count,
        "created {} agents, expected {agent_count}",
        agents.len()
    );
    ensure!(!latencies.is_empty(), "no model replies completed");
    ensure!(
        messages_sent > 0,
        "model prose did not become a human.send delivery"
    );
    ensure!(
        received.len() >= agent_count,
        "not every agent received a message"
    );
    ensure!(
        delivered_ids.len() >= agent_count,
        "not every agent delivered a message to its model"
    );

    let final_head = Streams::open(connect(&socket).await?, &socket)
        .await?
        .journal_head;
    // The wire projects only visible journal rows. Filtered rows advance
    // the durable head without a Log message, so gaps are valid and the final
    // visible row need not equal the durable head.
    ensure!(
        last_seq <= final_head,
        "visible journal advanced beyond durable head"
    );

    let metrics: FakeMetrics = reqwest::get(format!(
        "{}/metrics",
        ready.anthropic_base_url.trim_end_matches('/')
    ))
    .await?
    .error_for_status()?
    .json()
    .await?;
    println!(
        "MODEL_TIMING window_us={} busy_us={} idle_us={}",
        metrics.request_window_us, metrics.busy_us, metrics.idle_us
    );
    if args.rounds <= REAL_TOOL_ROUNDS {
        println!("MODEL_IDLE_GAPS_US {:?}", metrics.idle_gaps_us);
    }
    if args.scenario == Scenario::RealToolRounds {
        ensure!(
            metrics.idle_gaps_us.len() >= args.rounds,
            "missing model request gaps"
        );
        let gaps = &metrics.idle_gaps_us[metrics.idle_gaps_us.len() - args.rounds..];
        let mut sorted = gaps.to_vec();
        sorted.sort_unstable();
        println!(
            "WARM_ROUNDS gap_p50_us={} gap_p95_us={}",
            percentile(&sorted, 50),
            percentile(&sorted, 95)
        );
    }
    println!(
        "END_TO_END_US total={} model_active={} between_requests={} outside_request_window={}",
        scenario_us,
        metrics.busy_us,
        metrics.idle_us,
        scenario_us.saturating_sub(metrics.request_window_us)
    );
    latencies.sort_unstable();
    let scenario_error = verify_scenario(
        args.scenario,
        args.rounds,
        &ScenarioResults {
            failed,
            retrying,
            calls,
            compacted,
            clarifying,
            reported_calls: reported_calls.len(),
            messages_sent,
            latencies: &latencies,
            model_result_max: metrics.max_input_tool_output_bytes,
            reported_output_sizes: &reported_output_sizes,
        },
    )
    .err();
    let elapsed = started.elapsed().as_secs_f64().min(args.seconds as f64);
    println!(
        "scenario={} agents={} duration_s={} replies={} failures={} retrying_failures={} tool_calls={} compacted={} clarifying={} reported_calls={} messages_sent={} delivered={} model_result_max={} turns_per_sec={:.2} fake_completed_turns={} fake_bytes={} report_reply_p50_ms={} report_reply_p99_ms={} fake_vmrss_kib={} host_vmrss_kib={} journal_head={}",
        args.scenario.as_str(),
        agent_count,
        args.seconds,
        replies,
        failed,
        retrying,
        calls,
        compacted,
        clarifying,
        reported_calls.len(),
        messages_sent,
        delivered_ids.len(),
        metrics.max_input_tool_output_bytes,
        metrics.completed_turns as f64 / elapsed,
        metrics.completed_turns,
        metrics.bytes_streamed,
        percentile(&latencies, 50),
        percentile(&latencies, 99),
        vmrss_kib(fake_pid)?,
        vmrss_kib(host_pid)?,
        final_head.0
    );
    if let Some(error) = scenario_error {
        return Err(error);
    }
    Ok(())
}

fn scenario_complete(
    scenario: Scenario,
    rounds: usize,
    failed: u64,
    calls: usize,
    compacted: u64,
    clarifying: u64,
    reported_calls: usize,
    messages_sent: usize,
    latencies: &[u64],
) -> bool {
    match scenario {
        Scenario::Baseline => false,
        Scenario::RealToolRounds => {
            calls == rounds + 1 && reported_calls == rounds && messages_sent >= 1
        }
        Scenario::RateLimit => failed >= 2 && messages_sent >= 1,
        Scenario::StreamCut => failed >= 1 && messages_sent >= 1,
        Scenario::SlowTrickle => messages_sent >= 1 && !latencies.is_empty(),
        Scenario::HugeToolOutput => calls >= 101 && reported_calls >= 100 && messages_sent >= 1,
        Scenario::FortyToolCalls => calls >= 41 && reported_calls >= 40 && messages_sent >= 1,
        Scenario::ReasoningCompaction => compacted >= 1 && messages_sent >= 1,
        Scenario::ClarifyingQuestion => clarifying >= 1 && messages_sent >= 1,
    }
}

struct ScenarioResults<'a> {
    failed: u64,
    retrying: u64,
    calls: usize,
    compacted: u64,
    clarifying: u64,
    reported_calls: usize,
    messages_sent: usize,
    latencies: &'a [u64],
    model_result_max: u64,
    reported_output_sizes: &'a [usize],
}

fn verify_scenario(scenario: Scenario, rounds: usize, results: &ScenarioResults<'_>) -> Result<()> {
    match scenario {
        Scenario::RealToolRounds => {
            ensure!(
                results.calls == rounds + 1
                    && results.reported_calls == rounds
                    && results.messages_sent == 1
                    && results.failed == 0,
                "real-tool-rounds did not complete all sequential exchanges and send a final message"
            );
        }
        Scenario::Baseline => {}
        Scenario::RateLimit => {
            ensure!(
                results.failed >= 2,
                "agent host did not journal both provider failures"
            );
            ensure!(
                results.retrying >= 1,
                "agent host did not mark a provider failure retrying"
            );
        }
        Scenario::StreamCut => {
            ensure!(
                results.failed >= 1,
                "agent host did not journal the cut stream"
            );
            ensure!(
                results.retrying >= 1,
                "agent host did not retry the cut stream"
            );
        }
        Scenario::SlowTrickle => {
            ensure!(
                percentile(results.latencies, 50) >= 59_000,
                "agent host ended the one-minute trickle early"
            );
        }
        Scenario::HugeToolOutput => {
            ensure!(
                results.calls == 101 && results.reported_calls == 100,
                "100 sequential heavy-tail cells were not answered before the final message"
            );
            ensure!(
                results.model_result_max >= 7_000 && results.model_result_max <= 40_100,
                "model-facing output did not exercise or exceeded the 10,000-token budget"
            );
            ensure!(
                results.reported_output_sizes.len() == 100,
                "missing notebook detail for heavy-tail calls"
            );
            let mut sizes = results.reported_output_sizes.to_vec();
            sizes.sort_unstable();
            ensure!(
                sizes[49] >= 227 && sizes[49] < 2_000,
                "heavy-tail median report drifted"
            );
            ensure!(
                sizes[89] >= 5_000 && sizes[99] >= 7_000,
                "heavy-tail report tail was not preserved"
            );
        }
        Scenario::FortyToolCalls => {
            ensure!(
                results.calls == 41 && results.reported_calls == 40,
                "forty sequential cells were not answered before the final message"
            );
        }
        Scenario::ReasoningCompaction => {
            ensure!(
                results.compacted >= 1,
                "agent host did not retain the compaction item"
            );
        }
        Scenario::ClarifyingQuestion => {
            ensure!(
                results.clarifying == 1,
                "client did not see the clarifying question"
            );
            ensure!(
                results.calls == 1 && results.messages_sent == 1,
                "clarifying question was not delivered by a single exec call"
            );
        }
    }
    Ok(())
}

fn prompt(agent: usize, cycle: u64) -> Vec<ContentPart> {
    vec![ContentPart::Text {
        text: format!(
            "QA agent {agent}, cycle {cycle}: perform the next deterministic bounded action."
        ),
    }]
}

fn isolated_command(program: &Path, root: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "SHELL",
            std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()),
        )
        .env("USER", "rho-qa")
        .env("LOGNAME", "rho-qa");
    command
}

fn write_synthetic_auth(state: &Path) -> Result<()> {
    let dir = state.join("rho/auth.d");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    for name in ["default", "backup"] {
        let path = dir.join(format!("{name}.json"));
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "access_token": format!("rho-qa-synthetic-{name}-token"),
                "expires_at_ms": u64::MAX,
                "account_id": format!("rho-qa-synthetic-{name}-account"),
                "client_secret": vec![0u8; 32],
            }))?,
        )?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn init_workspace(path: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .arg(path)
        .status()
        .context("run git init")?;
    ensure!(status.success(), "git init failed");
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Rho QA",
            "-c",
            "user.email=qa@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Synthetic QA workspace",
        ])
        .status()
        .context("initialize QA repository HEAD")?;
    ensure!(status.success(), "QA initial commit failed");
    Ok(())
}

async fn connect(socket: &Path) -> Result<Client> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match Client::connect(socket).await {
            Ok(client) => return Ok(client),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(error) => return Err(error).context("connect to rho-agent-host"),
        }
    }
}

fn ensure_network_namespace() -> Result<()> {
    // /sys may still be the host mount after unshare(2); procfs's network
    // view follows the process's actual network namespace.
    let devices = fs::read_to_string("/proc/net/dev")?;
    let names = devices
        .lines()
        .skip(2)
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim()))
        .collect::<Vec<_>>();
    ensure!(
        only_loopback(names),
        "fake-model-proof requires a loopback-only network namespace; run it under `unshare --user --map-root-user --net` and bring lo up"
    );
    Ok(())
}

fn only_loopback<'a>(names: impl IntoIterator<Item = &'a str>) -> bool {
    names.into_iter().eq(["lo"])
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    sorted[(sorted.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn vmrss_kib(pid: u32) -> Result<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .context("VmRSS missing from proc status")?;
    line.split_whitespace()
        .nth(1)
        .context("malformed VmRSS")?
        .parse()
        .context("parse VmRSS")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn tree_commit() -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .context("read proof tree commit")?;
    ensure!(
        output.status.success(),
        "git could not read proof tree commit"
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use rho_fake_model::{REAL_TOOL_ROUNDS, Scenario};

    use super::{only_loopback, percentile, scenario_complete};

    #[test]
    fn network_namespace_must_contain_exactly_loopback() {
        assert!(only_loopback(["lo"]));
        assert!(!only_loopback(["eth0", "lo"]));
        assert!(!only_loopback([]));
    }

    #[test]
    fn real_rounds_require_all_calls_and_results() {
        assert!(!scenario_complete(
            Scenario::RealToolRounds,
            REAL_TOOL_ROUNDS,
            0,
            REAL_TOOL_ROUNDS,
            0,
            0,
            REAL_TOOL_ROUNDS - 1,
            &[1]
        ));
        assert!(!scenario_complete(
            Scenario::RealToolRounds,
            REAL_TOOL_ROUNDS,
            0,
            REAL_TOOL_ROUNDS - 1,
            0,
            0,
            REAL_TOOL_ROUNDS,
            &[1]
        ));
        assert!(scenario_complete(
            Scenario::RealToolRounds,
            REAL_TOOL_ROUNDS,
            0,
            REAL_TOOL_ROUNDS,
            0,
            0,
            REAL_TOOL_ROUNDS,
            &[1]
        ));
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 99), 4);
    }
}
