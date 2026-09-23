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
use rho_agent_host_proto::agents::{
    ClientFrame as AgentsClientFrame, Reply, ServerFrame as AgentsServerFrame,
};
use rho_agent_host_proto::client::Client;
use rho_agent_host_proto::transcript::{AgentPos, DetailBody, Seq, TranscriptEvent, TurnEdge};
use rho_agent_host_proto::{
    AgentCommand, AgentId, AgentRole, ContentPart, MessageDelivery, StartMode,
};
use rho_fake_model::{REAL_TOOL_ROUNDS, Scenario};
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
    /// Directory containing rho-daemon and rho-fake-model. Defaults to the
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

#[derive(Clone, Copy)]
enum ExpectedDetail {
    Response,
    Results,
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
    let daemon_bin = bin_dir.join("rho-daemon");
    ensure!(fake_bin.is_file(), "missing {}", fake_bin.display());
    ensure!(daemon_bin.is_file(), "missing {}", daemon_bin.display());
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
    let mut daemon = isolated_command(&daemon_bin, root.path());
    daemon
        .args([
            "--socket-path",
            socket.to_str().context("socket path is not UTF-8")?,
        ])
        .args(["--openai-base-url", &ready.openai_base_url])
        .args(["--anthropic-base-url", &ready.anthropic_base_url])
        .stdout(File::create(root.path().join("daemon.stdout.log"))?)
        .stderr(File::create(root.path().join("daemon.stderr.log"))?);
    let daemon = daemon
        .spawn()
        .with_context(|| format!("start {}", daemon_bin.display()))?;
    let daemon_pid = daemon.id();
    children.0.push(daemon);
    let _children = children;

    let mut client = Streams::open(connect(&socket).await?, &socket).await?;
    let initial_head = client.journal_head;
    ensure!(
        initial_head == Seq(0),
        "fresh isolated daemon journal was not empty"
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
        client.send(AgentCommand::New {
            role: AgentRole::default(),
            start: StartMode::NewOn {
                repo: repo.clone(),
                revset: "HEAD".into(),
            },
            mode: rho_agent_host_proto::WorksetMode::View,
            content: Some(prompt(index, 0)),
        });
    }

    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.seconds);
    let mut agents = HashSet::new();
    let mut expected_pos: HashMap<AgentId, AgentPos> = HashMap::new();
    let mut sent_at = HashMap::new();
    let mut open_turns = HashSet::new();
    let mut pending_details = HashMap::new();
    let mut latencies = Vec::new();
    let mut replies = 0u64;
    let mut failed = 0u64;
    let mut retrying = 0u64;
    let mut calls = 0usize;
    let mut compacted = 0u64;
    let mut clarifying = 0u64;
    let mut result_sizes = Vec::new();
    let mut tool_durations_ms = Vec::new();
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
        let all_idle =
            agents.len() == agent_count && open_turns.is_empty() && pending_details.is_empty();
        if (args.scenario != Scenario::Baseline
            && all_idle
            && scenario_complete(
                args.scenario,
                args.rounds,
                failed,
                calls,
                compacted,
                clarifying,
                result_sizes.len(),
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
                    "daemon journal stalled: {}",
                    fs::read_to_string(root.path().join("daemon.stderr.log")).unwrap_or_default()
                )
            })??;
        match message {
            Incoming::Reply(Reply::AgentCreated { agent_id }) => {
                agents.insert(agent_id);
                ensure!(
                    agents.len() <= agent_count,
                    "daemon created more than {agent_count} agents"
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
                                runtime == rho_agent_host_proto::transcript::RuntimeKind::Rho,
                                "created a non-native agent"
                            );
                            agents.insert(entry.agent_id);
                        }
                        TranscriptEvent::Sent { results, at, .. } => {
                            for result in &results {
                                tool_durations_ms.push(
                                    result
                                        .finished_at
                                        .saturating_duration_since(result.started_at),
                                );
                                if args.scenario == Scenario::RealToolRounds {
                                    ensure!(
                                        result.status
                                            == rho_agent_host_proto::transcript::ToolStatus::Success,
                                        "real-tool-rounds tool failed"
                                    );
                                }
                            }
                            sent_at.insert(entry.agent_id, at);
                            if !results.is_empty() {
                                pending_details
                                    .insert((entry.agent_id, entry.pos), ExpectedDetail::Results);
                                client
                                    .send_agents(&AgentsClientFrame::Detail {
                                        agent_id: entry.agent_id,
                                        pos: entry.pos,
                                        // One position per request here; the
                                        // GUI batches a chunk's into one.
                                        more: Vec::new(),
                                    })
                                    .await?;
                            }
                        }
                        TranscriptEvent::Replied {
                            items,
                            compacted: did_compact,
                            at,
                            ..
                        } => {
                            let sent = sent_at
                                .remove(&entry.agent_id)
                                .context("Replied without preceding Sent")?;
                            latencies.push(at.saturating_duration_since(sent));
                            replies += 1;
                            calls += items
                                .iter()
                                .filter(|item| {
                                    matches!(
                                        item,
                                        rho_agent_host_proto::transcript::Item::ToolCall { .. }
                                    )
                                })
                                .count();
                            compacted += u64::from(did_compact);
                            clarifying += u64::from(
                                items
                                    .iter()
                                    .rev()
                                    .find_map(|item| match item {
                                        rho_agent_host_proto::transcript::Item::Text {
                                            text,
                                            ..
                                        } => Some(text),
                                        _ => None,
                                    })
                                    .is_some_and(|text| text.trim_end().ends_with('?')),
                            );
                            pending_details
                                .insert((entry.agent_id, entry.pos), ExpectedDetail::Response);
                            client
                                .send_agents(&AgentsClientFrame::Detail {
                                    agent_id: entry.agent_id,
                                    pos: entry.pos,
                                    // Exercise detail retrieval independently of the GUI,
                                    // which does not fetch tool output.
                                    more: Vec::new(),
                                })
                                .await?;
                        }
                        TranscriptEvent::Failed {
                            retrying: is_retrying,
                            ..
                        } => {
                            failed += 1;
                            retrying += u64::from(is_retrying);
                        }
                        TranscriptEvent::Turn {
                            edge: TurnEdge::Started,
                            ..
                        } => {
                            open_turns.insert(entry.agent_id);
                        }
                        TranscriptEvent::Turn {
                            edge: TurnEdge::Ended(_),
                            ..
                        } => {
                            open_turns.remove(&entry.agent_id);
                            if args.scenario == Scenario::Baseline && Instant::now() < deadline {
                                let cycle = cycles.entry(entry.agent_id).or_default();
                                *cycle += 1;
                                client.send(AgentCommand::Send {
                                    agent_id: entry.agent_id,
                                    content: prompt(0, *cycle),
                                    delivery: MessageDelivery::Immediate,
                                });
                            }
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
                    (ExpectedDetail::Response, DetailBody::Response(_)) => {}
                    (ExpectedDetail::Results, DetailBody::Results(results)) => {
                        result_sizes.extend(results.into_iter().map(|result| result.output.len()));
                    }
                    _ => bail!("daemon Detail body did not match its journal event"),
                }
            }
            Incoming::Reply(Reply::Failed { reason }) => {
                bail!("daemon refused proof action: {reason}")
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
        println!("TOOL_DURATIONS_MS {:?}", tool_durations_ms);
    }
    if args.scenario == Scenario::RealToolRounds {
        ensure!(
            metrics.idle_gaps_us.len() >= args.rounds,
            "missing model request gaps"
        );
        let gaps = &metrics.idle_gaps_us[metrics.idle_gaps_us.len() - args.rounds..];
        for start in (0..args.rounds).step_by((args.rounds / 10).max(20)) {
            let end = (start + (args.rounds / 10).max(20)).min(args.rounds);
            let first = start.max(1); // exclude the first, cold tool round
            if first >= end {
                continue;
            }
            let mut sorted = gaps[first..end].to_vec();
            sorted.sort_unstable();
            println!(
                "WARM_ROUNDS {}-{} gap_mean_us={} gap_p50_us={} gap_p95_us={} tool_mean_ms={:.2}",
                first + 1,
                end,
                sorted.iter().sum::<u64>() / sorted.len() as u64,
                percentile(&sorted, 50),
                percentile(&sorted, 95),
                tool_durations_ms[first..end].iter().sum::<u64>() as f64 / (end - first) as f64
            );
        }
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
            result_sizes: &result_sizes,
            latencies: &latencies,
            model_result_max: metrics.max_input_tool_output_bytes,
        },
    )
    .err();
    let mut sorted_result_sizes = result_sizes.clone();
    sorted_result_sizes.sort_unstable();
    let elapsed = started.elapsed().as_secs_f64().min(args.seconds as f64);
    println!(
        "scenario={} agents={} duration_s={} replies={} failures={} retrying_failures={} tool_calls={} compacted={} clarifying={} results={} result_bytes={} result_p50={} result_mean={} result_p90={} result_max={} model_result_max={} turns_per_sec={:.2} fake_completed_turns={} fake_bytes={} sent_replied_p50_ms={} sent_replied_p99_ms={} fake_vmrss_kib={} daemon_vmrss_kib={} journal_head={}",
        args.scenario.as_str(),
        agent_count,
        args.seconds,
        replies,
        failed,
        retrying,
        calls,
        compacted,
        clarifying,
        result_sizes.len(),
        result_sizes.iter().sum::<usize>(),
        usize_percentile(&sorted_result_sizes, 50),
        result_sizes.iter().sum::<usize>() / result_sizes.len().max(1),
        usize_percentile(&sorted_result_sizes, 90),
        sorted_result_sizes.last().copied().unwrap_or(0),
        metrics.max_input_tool_output_bytes,
        metrics.completed_turns as f64 / elapsed,
        metrics.completed_turns,
        metrics.bytes_streamed,
        percentile(&latencies, 50),
        percentile(&latencies, 99),
        vmrss_kib(fake_pid)?,
        vmrss_kib(daemon_pid)?,
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
    results: usize,
    latencies: &[u64],
) -> bool {
    match scenario {
        Scenario::RealToolRounds => calls == rounds && results == rounds && !latencies.is_empty(),
        Scenario::Baseline => false,
        Scenario::RateLimit => failed >= 2 && !latencies.is_empty(),
        Scenario::StreamCut => failed >= 1 && !latencies.is_empty(),
        Scenario::SlowTrickle => !latencies.is_empty(),
        Scenario::HugeToolOutput => results >= 100,
        Scenario::FortyToolCalls => calls >= 40 && results >= 40,
        Scenario::ReasoningCompaction => compacted >= 1,
        Scenario::ClarifyingQuestion => clarifying >= 1,
    }
}

struct ScenarioResults<'a> {
    failed: u64,
    retrying: u64,
    calls: usize,
    compacted: u64,
    clarifying: u64,
    result_sizes: &'a [usize],
    latencies: &'a [u64],
    model_result_max: u64,
}

fn verify_scenario(scenario: Scenario, rounds: usize, results: &ScenarioResults<'_>) -> Result<()> {
    match scenario {
        Scenario::RealToolRounds => {
            ensure!(
                results.calls == rounds
                    && results.result_sizes.len() == rounds
                    && results.failed == 0,
                "real-tool-rounds did not complete all expected successful tool exchanges"
            );
        }
        Scenario::Baseline => {}
        Scenario::RateLimit => {
            ensure!(
                results.failed >= 2,
                "daemon did not journal both provider failures"
            );
            ensure!(
                results.retrying >= 1,
                "daemon did not mark a provider failure retrying"
            );
        }
        Scenario::StreamCut => {
            ensure!(results.failed >= 1, "daemon did not journal the cut stream");
            ensure!(results.retrying >= 1, "daemon did not retry the cut stream");
        }
        Scenario::SlowTrickle => {
            ensure!(
                percentile(results.latencies, 50) >= 59_000,
                "daemon ended the one-minute trickle early"
            );
        }
        Scenario::HugeToolOutput => {
            ensure!(
                results.result_sizes.len() == 100,
                "expected 100 tool results"
            );
            let mut sorted = results.result_sizes.to_vec();
            sorted.sort_unstable();
            ensure!(usize_percentile(&sorted, 50) == 227, "result p50 drifted");
            ensure!(
                results.result_sizes.iter().sum::<usize>() / 100 == 4_047,
                "result mean drifted"
            );
            ensure!(
                usize_percentile(&sorted, 90) == 13_097,
                "result p90 drifted"
            );
            ensure!(sorted.last() == Some(&170_448), "result max drifted");
            ensure!(
                results.model_result_max <= 40_100,
                "model-facing result exceeded its 10,000-token budget"
            );
        }
        Scenario::FortyToolCalls => {
            ensure!(results.calls == 40, "daemon did not retain forty calls")
        }
        Scenario::ReasoningCompaction => {
            ensure!(
                results.compacted >= 1,
                "daemon did not retain the compaction item"
            );
        }
        Scenario::ClarifyingQuestion => {
            ensure!(
                results.clarifying == 1,
                "client did not see the clarifying question"
            );
            ensure!(
                results.calls == 0,
                "clarifying persona unexpectedly called a tool"
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
            Err(error) => return Err(error).context("connect to rho-daemon"),
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

fn usize_percentile(sorted: &[usize], percent: usize) -> usize {
    sorted
        .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
        .copied()
        .unwrap_or(0)
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
