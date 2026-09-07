//! Reproducible full-stack throughput and journal proof against rho-fake-model.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use camino::Utf8PathBuf;
use clap::Args as ClapArgs;
use rho_core::{AgentId, AgentRole, ContentPart, MessageDelivery};
use rho_ui_proto::client::Client;
use rho_ui_proto::mirror::{AgentPos, DetailBody, MirrorEvent, Seq, TurnEdge};
use rho_ui_proto::{ClientMessage, JoinTarget, ServerMessage, StartMode};
use serde::Deserialize;
use serde_json::json;

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
}

struct Children(Vec<Child>);

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

    let mut fake = isolated_command(&fake_bin, root.path());
    fake.args(["--seed", &args.seed.to_string(), "--no-faults"])
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
    client
        .send(&ClientMessage::Follow { since: Seq(0) })
        .await?;

    let repo = Utf8PathBuf::try_from(workspace).context("workspace path is not UTF-8")?;
    for index in 0..AGENTS {
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
    let mut pending_details = HashSet::new();
    let mut latencies = Vec::new();
    let mut replies = 0u64;
    let mut cycles: HashMap<AgentId, u64> = HashMap::new();
    let mut last_seq = Seq(0);
    let quiesce_deadline = deadline + QUIESCE_TIMEOUT;

    loop {
        if Instant::now() >= deadline
            && agents.len() == AGENTS
            && open_turns.is_empty()
            && pending_details.is_empty()
        {
            break;
        }
        ensure!(
            Instant::now() < quiesce_deadline,
            "agents did not quiesce after the drive window"
        );
        let message = tokio::time::timeout(Duration::from_secs(10), client.recv())
            .await
            .context("daemon journal stalled")??;
        match message {
            ServerMessage::AgentCreated { agent_id } => {
                agents.insert(agent_id);
                ensure!(
                    agents.len() <= AGENTS,
                    "daemon created more than {AGENTS} agents"
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
                        MirrorEvent::Sent { at, .. } => {
                            sent_at.insert(entry.agent_id, at);
                        }
                        MirrorEvent::Replied { at, .. } => {
                            let sent = sent_at
                                .remove(&entry.agent_id)
                                .context("Replied without preceding Sent")?;
                            latencies.push(at.saturating_duration_since(sent));
                            replies += 1;
                            pending_details.insert((entry.agent_id, entry.pos));
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
                            if Instant::now() < deadline {
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
                ensure!(
                    pending_details.remove(&(agent_id, pos)),
                    "unexpected Detail response"
                );
                ensure!(
                    matches!(body, DetailBody::Response(_)),
                    "Replied detail was not a response"
                );
            }
            ServerMessage::Error { message } => bail!("daemon refused proof action: {message}"),
            _ => {}
        }
    }
    ensure!(
        agents.len() == AGENTS,
        "created {} agents, expected {AGENTS}",
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
    let elapsed = started.elapsed().as_secs_f64().min(args.seconds as f64);
    println!(
        "agents={AGENTS} duration_s={} replies={} turns_per_sec={:.2} fake_completed_turns={} fake_bytes={} sent_replied_p50_ms={} sent_replied_p99_ms={} fake_vmrss_kib={} daemon_vmrss_kib={} journal_head={}",
        args.seconds,
        replies,
        metrics.completed_turns as f64 / elapsed,
        metrics.completed_turns,
        metrics.bytes_streamed,
        percentile(&latencies, 50),
        percentile(&latencies, 99),
        vmrss_kib(fake_pid)?,
        vmrss_kib(daemon_pid)?,
        final_head.0
    );
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
    let path = dir.join("default.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "access_token": "rho-qa-synthetic-token",
            "expires_at_ms": u64::MAX,
            "account_id": "rho-qa-synthetic-account",
            "client_secret": vec![0u8; 32],
        }))?,
    )?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
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
