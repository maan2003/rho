//! Reproducible full-stack throughput and journal proof against rho-fake-model.

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
use rho_core::{AgentId, AgentRole, ContentPart, MessageDelivery};
use rho_fake_model::Scenario;
use rho_ui_proto::client::Client;
use rho_ui_proto::mirror::{AgentPos, DetailBody, MirrorEvent, Seq, TurnEdge};
use rho_ui_proto::{ClientMessage, JoinTarget, ServerMessage, StartMode};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};

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

    let mut client = connect(&socket).await?;
    client.send(&ClientMessage::Subscribe).await?;
    let initial_head = recv_ready(&mut client).await?;
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
        .send(&ClientMessage::Follow { since: Seq(0) })
        .await?;

    let repo = Utf8PathBuf::try_from(workspace).context("workspace path is not UTF-8")?;
    let agent_count = if args.scenario == Scenario::Baseline {
        AGENTS
    } else {
        1
    };
    for index in 0..agent_count {
        client
            .send(&ClientMessage::NewAgent {
                role: AgentRole::default(),
                start: StartMode::Join(JoinTarget::User { repo: repo.clone() }),
                content: Some(prompt(index, 0)),
            })
            .await?;
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
            .context("daemon journal stalled")??;
        match message {
            ServerMessage::AgentCreated { agent_id } => {
                agents.insert(agent_id);
                ensure!(
                    agents.len() <= agent_count,
                    "daemon created more than {agent_count} agents"
                );
            }
            ServerMessage::Log { entries } => {
                for entry in entries {
                    ensure!(
                        entry.seq == last_seq.next(),
                        "global journal sequence skipped: {:?} after {:?}",
                        entry.seq,
                        last_seq
                    );
                    last_seq = entry.seq;
                    let expected = expected_pos.entry(entry.agent_id).or_insert(AgentPos::ZERO);
                    ensure!(
                        entry.pos == *expected,
                        "agent {:?} position skipped: {:?} expected {:?}",
                        entry.agent_id,
                        entry.pos,
                        expected
                    );
                    *expected = expected.next();
                    match entry.event {
                        MirrorEvent::Created { runtime, .. } => {
                            ensure!(
                                runtime == rho_ui_proto::mirror::RuntimeKind::Rho,
                                "created a non-native agent"
                            );
                            agents.insert(entry.agent_id);
                        }
                        MirrorEvent::Sent { results, at, .. } => {
                            sent_at.insert(entry.agent_id, at);
                            if !results.is_empty() {
                                pending_details
                                    .insert((entry.agent_id, entry.pos), ExpectedDetail::Results);
                                client
                                    .send(&ClientMessage::Detail {
                                        agent_id: entry.agent_id,
                                        pos: entry.pos,
                                        // One position per request here; the
                                        // GUI batches a chunk's into one.
                                        more: Vec::new(),
                                    })
                                    .await?;
                            }
                        }
                        MirrorEvent::Replied {
                            text,
                            calls: reply_calls,
                            compacted: did_compact,
                            at,
                            ..
                        } => {
                            let sent = sent_at
                                .remove(&entry.agent_id)
                                .context("Replied without preceding Sent")?;
                            latencies.push(at.saturating_duration_since(sent));
                            replies += 1;
                            calls += reply_calls.len();
                            compacted += u64::from(did_compact);
                            clarifying += u64::from(text.trim_end().ends_with('?'));
                            pending_details
                                .insert((entry.agent_id, entry.pos), ExpectedDetail::Response);
                            client
                                .send(&ClientMessage::Detail {
                                    agent_id: entry.agent_id,
                                    pos: entry.pos,
                                    // One position per request here; the GUI
                                    // batches a chunk's positions into one.
                                    more: Vec::new(),
                                })
                                .await?;
                        }
                        MirrorEvent::Failed {
                            retrying: is_retrying,
                            ..
                        } => {
                            failed += 1;
                            retrying += u64::from(is_retrying);
                        }
                        MirrorEvent::Turn {
                            edge: TurnEdge::Started,
                            ..
                        } => {
                            open_turns.insert(entry.agent_id);
                        }
                        MirrorEvent::Turn {
                            edge: TurnEdge::Ended(_),
                            ..
                        } => {
                            open_turns.remove(&entry.agent_id);
                            if args.scenario == Scenario::Baseline && Instant::now() < deadline {
                                let cycle = cycles.entry(entry.agent_id).or_default();
                                *cycle += 1;
                                client
                                    .send(&ClientMessage::SendUserMessage {
                                        agent_id: entry.agent_id,
                                        content: prompt(0, *cycle),
                                        delivery: MessageDelivery::Immediate,
                                    })
                                    .await?;
                            }
                        }
                        _ => {}
                    }
                }
            }
            ServerMessage::Detail {
                agent_id,
                pos,
                body,
            } => {
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
            ServerMessage::Error { message } => bail!("daemon refused proof action: {message}"),
            _ => {}
        }
    }
    ensure!(
        agents.len() == agent_count,
        "created {} agents, expected {agent_count}",
        agents.len()
    );
    ensure!(!latencies.is_empty(), "no model replies completed");

    let mut head_client = connect(&socket).await?;
    head_client.send(&ClientMessage::Subscribe).await?;
    let final_head = recv_ready(&mut head_client).await?;
    while last_seq < final_head {
        match client.recv().await? {
            ServerMessage::Log { entries } => {
                for entry in entries {
                    ensure!(
                        entry.seq == last_seq.next(),
                        "global journal sequence skipped at final head"
                    );
                    last_seq = entry.seq;
                    let expected = expected_pos.entry(entry.agent_id).or_insert(AgentPos::ZERO);
                    ensure!(
                        entry.pos == *expected,
                        "agent {:?} position skipped at final head",
                        entry.agent_id
                    );
                    *expected = expected.next();
                }
            }
            ServerMessage::Error { message } => bail!("daemon journal error: {message}"),
            _ => {}
        }
    }
    ensure!(
        last_seq == final_head,
        "wire journal head {:?} did not match daemon {:?}",
        last_seq,
        final_head
    );

    let metrics: FakeMetrics = reqwest::get(format!(
        "{}/metrics",
        ready.anthropic_base_url.trim_end_matches('/')
    ))
    .await?
    .error_for_status()?
    .json()
    .await?;
    latencies.sort_unstable();
    let scenario_error = verify_scenario(
        args.scenario,
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
    failed: u64,
    calls: usize,
    compacted: u64,
    clarifying: u64,
    results: usize,
    latencies: &[u64],
) -> bool {
    match scenario {
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

fn verify_scenario(scenario: Scenario, results: &ScenarioResults<'_>) -> Result<()> {
    match scenario {
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
    let status = Command::new("jj")
        .args(["git", "init", "--colocate"])
        .arg(path)
        .status()
        .context("run jj git init")?;
    ensure!(status.success(), "jj git init failed");
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

async fn recv_ready(client: &mut Client) -> Result<Seq> {
    loop {
        match client.recv().await? {
            ServerMessage::Ready { journal_head, .. } => return Ok(journal_head),
            ServerMessage::Error { message } => bail!("daemon readiness error: {message}"),
            _ => {}
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
    let output = Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "commit_id"])
        .output()
        .context("read proof tree commit")?;
    ensure!(
        output.status.success(),
        "jj could not read proof tree commit"
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{only_loopback, percentile};

    #[test]
    fn network_namespace_must_contain_exactly_loopback() {
        assert!(only_loopback(["lo"]));
        assert!(!only_loopback(["eth0", "lo"]));
        assert!(!only_loopback([]));
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 99), 4);
    }
}
