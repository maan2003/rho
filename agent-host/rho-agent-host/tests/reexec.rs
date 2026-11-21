//! A re-exec keeps the workset processes: a real agent host binary, an
//! agent's terminal holding shell state and an agent partway through a turn
//! of sequential tool calls, SIGUSR2, and then the same shell answering and
//! the same turn finishing through the successor, under the same pids.
//! A refusal instead must terminate the worker, its fork server and jobs.
//! Harness-free, as `terminal_stream`, and for the same reason.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use rho_agent_types::{AgentId, WorksetKind};
use rho_rpc::protocol::{Opened, read_frame, write_frame, write_open};
use rho_terminal::protocol as term;
use rho_terminal::protocol::{
    ScrollbackItem, TermClientFrame, TermRow, TermServerFrame, TerminalList, TerminalOpen,
    WireScreen,
};

const WORKER: &str = "rho-agent-worker";
const REJECTING_HOST: &str = "reject-worker-host";
const REJECTING_LEGACY_HOST: &str = "reject-legacy-worker-host";
const WORKER_RETURNED: &str = "worker-returned";
const HANDOFF_ENV: &str = "RHO_HANDOFF";

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

struct Host(Child);

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn main() -> anyhow::Result<()> {
    let argv0 = std::env::args_os().next().context("missing argv[0]")?;
    if Path::new(&argv0).ends_with(WORKER) {
        let result = rho_inference::worker_main();
        if let Some(root) = Path::new(&argv0).parent() {
            std::fs::write(root.join(WORKER_RETURNED), [])?;
        }
        return result;
    }
    if Path::new(&argv0).ends_with(REJECTING_HOST) {
        return reject_handoff(false);
    }
    if Path::new(&argv0).ends_with(REJECTING_LEGACY_HOST) {
        return reject_handoff(true);
    }
    if std::env::args().any(|arg| arg == "--list") {
        if !std::env::args().any(|arg| arg == "--ignored") {
            println!("e2e: test");
        }
        return Ok(());
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let unshare = Command::new("unshare").args(["-U", "true"]).status();
    if unshare.map(|status| status.success()).unwrap_or(false) {
        runtime.block_on(with_host(HandoffCase::Compatible))?;
    } else {
        eprintln!("skipping isolated reexec: kernel forbids unshare(CLONE_NEWUSER)");
    }
    runtime.block_on(with_host(HandoffCase::Refused))?;
    runtime.block_on(with_host(HandoffCase::Unresponsive))?;
    println!("reexec passed");
    Ok(())
}

/// The predecessor re-execs argv[0], so the test can change a single field
/// before entering the real successor. No production version override.
fn reject_handoff(unresponsive: bool) -> anyhow::Result<()> {
    #[derive(senax_encoder::Decode)]
    struct Handoff {
        workers: Vec<Worker>,
    }
    #[derive(senax_encoder::Decode)]
    struct Worker {
        pid: u32,
        version: u32,
    }
    #[derive(senax_encoder::Encode)]
    struct WorkerVersion {
        version: u32,
    }

    let hex = std::env::var(HANDOFF_ENV)?;
    anyhow::ensure!(hex.len().is_multiple_of(2), "invalid handoff hex");
    let mut bytes = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    let handoff: Handoff = senax_encoder::decode(&mut bytes.as_slice())?;
    let [worker] = handoff.workers.as_slice() else {
        anyhow::bail!("expected exactly one worker in the refusal fixture");
    };
    if unresponsive {
        // A stopped worker cannot run its own EOF handler. This models a
        // predecessor binary waiting indefinitely, independent of its code.
        signal(worker.pid, rustix::process::Signal::STOP)?;
    }
    let version = senax_encoder::encode(&WorkerVersion {
        version: worker.version,
    })?;
    // A named struct is its tag, its encoded fields, and the zero end
    // marker. Match the complete encoded field, not the bare integer.
    let field = &version[1..version.len() - 1];
    let mut matches = bytes
        .windows(field.len())
        .enumerate()
        .filter(|(_, candidate)| *candidate == field);
    let (offset, _) = matches
        .next()
        .context("worker version absent from handoff")?;
    anyhow::ensure!(matches.next().is_none(), "ambiguous worker version field");
    let rejected = senax_encoder::encode(&WorkerVersion { version: u32::MAX })?;
    bytes.splice(
        offset..offset + field.len(),
        rejected[1..rejected.len() - 1].iter().copied(),
    );
    let handoff: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Err(Command::new(env!("CARGO_BIN_EXE_rho-agent-host"))
        .args(std::env::args_os().skip(1))
        .env(HANDOFF_ENV, handoff)
        .exec()
        .into())
}

enum HandoffCase {
    Compatible,
    Refused,
    Unresponsive,
}

async fn with_host(case: HandoffCase) -> anyhow::Result<()> {
    let state_dir = tempfile::tempdir()?;
    let root = state_dir.path();
    let runtime_dir = root.join("runtime");
    // The rig's synthetic account: the fake model takes any token.
    let auth = root.join("rho").join("auth.d");
    std::fs::create_dir_all(&auth)?;
    std::fs::write(
        auth.join("default.json"),
        serde_json::to_vec(&serde_json::json!({
            "access_token": "rho-reexec-token",
            "expires_at_ms": u64::MAX,
            "account_id": "rho-reexec-account",
            "client_secret": vec![0u8; 32],
        }))?,
    )?;
    let mut config = rho_fake_model::FakeModelConfig::seeded(0);
    config.scenario = rho_fake_model::Scenario::RealToolRounds;
    config.real_tool_rounds = ROUNDS;
    config.distribution.rate_limit_bps = 0;
    config.distribution.usage_limit_bps = 0;
    config.distribution.overload_bps = 0;
    config.distribution.disconnect_bps = 0;
    if !matches!(case, HandoffCase::Compatible) {
        config.timing.mode = rho_fake_model::TimingMode::Timed;
        config.timing.first_token = Duration::from_millis(500);
    }
    let model = rho_fake_model::FakeModel::start(config).await?;
    let executable = std::env::current_exe()?;
    let worker = root.join(WORKER);
    std::os::unix::fs::symlink(&executable, &worker)?;
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rho-agent-host"));
    command
        .arg("--runtime-dir")
        .arg(&runtime_dir)
        .arg("--claude-config-dir")
        .arg(root.join("claude"))
        .args(["--openai-base-url", &model.openai_base_url()])
        .env("XDG_STATE_HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("RHO_AGENT_WORKER", worker)
        .env("SHELL", "bash");
    if let Some(wrapper) = match case {
        HandoffCase::Compatible => None,
        HandoffCase::Refused => Some(REJECTING_HOST),
        HandoffCase::Unresponsive => Some(REJECTING_LEGACY_HOST),
    } {
        let reexec = root.join(wrapper);
        std::os::unix::fs::symlink(&executable, &reexec)?;
        command.as_std_mut().arg0(reexec);
    }
    rho_fs_view::command_stdio_only(&mut command);
    let mut host = Host(command.as_std_mut().spawn()?);
    let result = match case {
        HandoffCase::Compatible => work_outlives_reexec(host.0.id(), root, &model).await,
        HandoffCase::Refused => refused_worker_exits(host.0.id(), root, &model, false).await,
        HandoffCase::Unresponsive => refused_worker_exits(host.0.id(), root, &model, true).await,
    };
    let shutdown = signal(host.0.id(), rustix::process::Signal::TERM)
        .map_err(anyhow::Error::from)
        .and_then(|()| host.0.wait().map_err(Into::into));
    result?;
    anyhow::ensure!(shutdown?.success(), "fixture host failed during shutdown");
    Ok(())
}

fn signal(pid: u32, signal: rustix::process::Signal) -> rustix::io::Result<()> {
    rustix::process::kill_process(rustix::process::Pid::from_raw(pid as i32).unwrap(), signal)
}

/// Tool calls in the turn, each checking the one before it ran exactly once.
const ROUNDS: usize = 60;

async fn work_outlives_reexec(
    host: u32,
    state_dir: &std::path::Path,
    model: &rho_fake_model::FakeModel,
) -> anyhow::Result<()> {
    let paths = &rho_rpc::protocol::RuntimePaths::new(Some(state_dir.join("runtime")))?;
    let repo_temp = tempfile::tempdir()?;
    let repo_dir = repo_temp.path().join("repo");
    std::fs::create_dir(&repo_dir)?;
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@localhost",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo_dir)
            .status()?;
        assert!(status.success());
    }
    wait_for_host(paths).await?;
    let agent_id = tokio::time::timeout(
        Duration::from_secs(60),
        create_agent(paths, &repo_dir, None),
    )
    .await??;

    // State only the running shell holds: a variable, and a job that is
    // still sleeping when the agent host re-executes.
    let mut stream = open_terminal(paths, agent_id, true).await?;
    // The terminal runs the user's login shell; speak bash whatever it is.
    write_frame(
        &mut stream,
        &TermClientFrame::Input(b"exec bash --norc --noprofile\r".to_vec()),
    )
    .await?;
    wait_for_line(&mut stream, "bash-").await?;
    write_frame(
        &mut stream,
        &TermClientFrame::Input(
            b"kept=sh\"ell-state\"; (sleep 3; echo \"job-\"\"done\") &\r".to_vec(),
        ),
    )
    .await?;
    write_frame(
        &mut stream,
        &TermClientFrame::Input(b"echo \"read\"\"y\"\r".to_vec()),
    )
    .await?;
    wait_for_line(&mut stream, "ready").await?;

    let turn_agent = tokio::time::timeout(
        Duration::from_secs(60),
        create_agent(paths, &repo_dir, Some("Run the rounds.")),
    )
    .await??;
    tokio::time::timeout(Duration::from_secs(60), async {
        while model.metrics().requests < 10 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let workers = children(host);
    assert!(!workers.is_empty(), "the agent host runs a workset process");

    let reexec_at = model.metrics().requests;
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(host as i32).unwrap(),
        rustix::process::Signal::USR2,
    )?;
    // The old listener goes with the exec; wait for the successor's to
    // answer. Our attachment went with it too.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if read_frame::<_, TermServerFrame>(&mut stream).await.is_err() {
                break;
            }
        }
    })
    .await?;
    wait_for_host(paths).await?;
    let terminals = tokio::time::timeout(
        Duration::from_secs(30),
        rho_rpc::protocol::client::call(
            paths,
            TerminalList {
                agent: Some(agent_id.encoded()),
            },
        ),
    )
    .await??;
    assert_eq!(terminals.len(), 1, "the terminal survived: {terminals:?}");
    assert_eq!(children(host), workers, "the same workset processes");

    // The job finished on either side of the exec; the replayed screen or
    // what follows shows it.
    let mut stream = open_terminal(paths, agent_id, false).await?;
    wait_for_line(&mut stream, "job-done").await?;
    write_frame(
        &mut stream,
        &TermClientFrame::Input(b"echo \"$kept\"-after\r".to_vec()),
    )
    .await?;
    wait_for_line(&mut stream, "shell-state-after").await?;

    // Each round's command checks the count the one before left and then
    // bumps it, so a tool call lost or run twice across the exec stops the
    // count short of `ROUNDS`.
    tokio::time::timeout(Duration::from_secs(120), async {
        while rounds(state_dir) != Some(ROUNDS) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    let failed = model
        .drain_observations()
        .into_iter()
        .filter(|observation| observation.event_type == "response.failed")
        .count();
    assert_eq!(failed, 0, "every round saw the one before it");
    assert!(
        reexec_at <= ROUNDS as u64,
        "the turn was still running at the re-exec"
    );
    assert_eq!(children(host), workers, "the same workset processes");
    println!("compatible reexec preserved the terminal and all {ROUNDS} tool rounds");

    // The individual agent must acknowledge Stop; otherwise the host will
    // time out and stop the whole workset instead.
    tokio::time::timeout(
        Duration::from_secs(4),
        rho_rpc::protocol::client::call(
            paths,
            rho_agents_client::protocol::worksets::StopAgent { agent: turn_agent },
        ),
    )
    .await??;
    assert_eq!(
        children(host),
        workers,
        "stopping one agent kept its worker"
    );
    println!("individual Stop completed without ending its workset");

    // An explicit workset restart has the opposite lifetime contract from
    // host re-exec: discard this workset's live shell, preserve its saved
    // agent, and leave the other workset's process and files alone.
    rho_rpc::protocol::client::call(
        paths,
        rho_agents_client::protocol::worksets::RestartWorkset { agent: agent_id },
    )
    .await?;
    let restarted = children(host);
    assert_eq!(restarted.len(), workers.len());
    assert_eq!(
        restarted.iter().filter(|pid| workers.contains(pid)).count(),
        workers.len() - 1,
        "restart replaces only the selected workset process"
    );
    let terminals = rho_rpc::protocol::client::call(
        paths,
        TerminalList {
            agent: Some(agent_id.encoded()),
        },
    )
    .await?;
    assert!(terminals.is_empty(), "restart ends live terminals");
    let mut stream = open_terminal(paths, agent_id, true).await?;
    write_frame(
        &mut stream,
        &TermClientFrame::Input(b"exec bash --norc --noprofile\r".to_vec()),
    )
    .await?;
    wait_for_line(&mut stream, "bash-").await?;
    write_frame(
        &mut stream,
        &TermClientFrame::Input(b"echo \"fresh-\"\"${kept-unset}\"\r".to_vec()),
    )
    .await?;
    wait_for_line(&mut stream, "fresh-unset").await?;
    assert_eq!(rounds(state_dir), Some(ROUNDS), "saved files survive");
    println!("workset restart replaced one worker and discarded its live shell");
    Ok(())
}

/// Unlike an accepted handoff, this takes away both connections while
/// the worker still has a turn and an evaluation that cannot finish on their
/// own. The evaluation starts a child and a grandchild in separate sessions.
async fn refused_worker_exits(
    host: u32,
    root: &Path,
    model: &rho_fake_model::FakeModel,
    unresponsive: bool,
) -> anyhow::Result<()> {
    use rho_agents_client::protocol::worksets::{CreateWorkset, Eval};

    let paths = &rho_rpc::protocol::RuntimePaths::new(Some(root.join("runtime")))?;
    wait_for_host(paths).await?;
    let created = tokio::time::timeout(
        Duration::from_secs(60),
        rho_rpc::protocol::client::call(
            paths,
            CreateWorkset {
                kind: WorksetKind::Host {
                    cwd: root.to_str().context("fixture path is not UTF-8")?.into(),
                },
                code: r#"new_agent(runtime={"kind":"native","model":"sol","effort":"medium"})"#
                    .into(),
            },
        ),
    )
    .await??;
    let agent = created
        .result
        .agent
        .context("missing agent in refusal fixture")?;
    brief(paths, agent, "Run the rounds.").await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while model.metrics().requests < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let [worker] = children(host)
        .try_into()
        .map_err(|children: Vec<u32>| anyhow::anyhow!("expected one worker, found {children:?}"))?;
    let mut observed = Watched::default();
    observed.add(worker)?;
    let fork_server = child_pids(worker)
        .find(|pid| {
            command_line(*pid)
                .split(|&byte| byte == 0)
                .any(|argument| argument == b"--rho-fork-server")
        })
        .context("the first tool command did not leave a Bash fork server")?;
    observed.add(fork_server)?;
    let mut terminal = open_terminal(paths, agent, true).await?;

    let child_pids = root.join("running-pids");
    let code = format!(
        "import subprocess, time\nsubprocess.Popen(['bash', '-c', {}, 'bash', {}], start_new_session=True)\ntime.sleep(600)",
        rho_agents_client::snippets::py_str(
            r#"setsid sleep 600 & printf '%s %s\n' "$$" "$!" > "$1"; wait"#
        ),
        rho_agents_client::snippets::py_str(child_pids.to_str().unwrap()),
    );
    let pending = rho_rpc::protocol::client::call(
        paths,
        Eval {
            workset: created.workset,
            code,
        },
    );
    tokio::pin!(pending);
    let pids = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                result = &mut pending => anyhow::bail!("the evaluation was not running: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if let Ok(words) = std::fs::read_to_string(&child_pids)
                        && let Ok(pids) = words.split_whitespace().map(str::parse::<u32>)
                            .collect::<Result<Vec<_>, _>>()
                        && let [child, job] = pids.as_slice()
                    {
                        break Ok([*child, *job]);
                    }
                }
            }
        }
    })
    .await??;
    for pid in pids {
        observed.add(pid)?;
    }

    anyhow::ensure!(
        model.metrics().requests < ROUNDS as u64,
        "the turn already finished"
    );
    let before = Instant::now();
    signal(host, rustix::process::Signal::USR2)?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while read_frame::<_, TermServerFrame>(&mut terminal)
            .await
            .is_ok()
        {}
    })
    .await
    .context("the predecessor never disconnected the terminal")?;
    wait_for_host(paths).await?;

    tokio::time::timeout(Duration::from_secs(5), async {
        while process_exists(worker) || observed.is_running() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "refused worker or jobs still present: {:?}",
            observed.running()
        )
    })?;
    anyhow::ensure!(
        root.join(WORKER_RETURNED).exists() != unresponsive,
        "a responsive worker must return on EOF; a stopped one needs the host fallback"
    );
    if !unresponsive {
        anyhow::ensure!(
            observed.0.iter().all(|(pid, _)| !process_exists(*pid)),
            "the worker must reap its children, as well as stopping them"
        );
    }
    let who = if unresponsive {
        "host fallback"
    } else {
        "worker shutdown"
    };
    println!(
        "reexec refused worker {worker}; {who} ended it, fork server {fork_server}, and separate-session jobs {pids:?} in {:?}",
        before.elapsed()
    );
    Ok(())
}

/// Hold identities so even a failing test can clean up only its own jobs.
#[derive(Default)]
struct Watched(Vec<(u32, std::os::fd::OwnedFd)>);

impl Watched {
    fn add(&mut self, pid: u32) -> anyhow::Result<()> {
        let id = rustix::process::Pid::from_raw(pid as i32).context("invalid test pid")?;
        self.0.push((
            pid,
            rustix::process::pidfd_open(id, rustix::process::PidfdFlags::empty())?,
        ));
        Ok(())
    }

    fn running(&self) -> Vec<u32> {
        self.0
            .iter()
            .filter_map(|(pid, _)| {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                let (_, state) = stat.rsplit_once(") ")?;
                (!state.starts_with(['Z', 'X'])).then_some(*pid)
            })
            .collect()
    }

    fn is_running(&self) -> bool {
        !self.running().is_empty()
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        for (_, process) in &self.0 {
            let _ = rustix::process::pidfd_send_signal(process, rustix::process::Signal::KILL);
        }
    }
}

/// The count the fake model's rounds keep in the agent's checkout.
fn rounds(dir: &std::path::Path) -> Option<usize> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if entry.file_name() == ".rho-fake-rounds-0" {
            return std::fs::read_to_string(path).ok()?.trim().parse().ok();
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && let Some(count) = rounds(&path)
        {
            return Some(count);
        }
    }
    None
}

async fn wait_for_host(paths: &rho_rpc::protocol::RuntimePaths) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match rho_rpc::protocol::client::connect(paths).await {
                Ok(_) => return,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await?;
    Ok(())
}

fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn command_line(pid: u32) -> Vec<u8> {
    std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default()
}

fn child_pids(pid: u32) -> impl Iterator<Item = u32> {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|listed| {
            listed
                .split_whitespace()
                .filter_map(|child| child.parse().ok())
                .collect::<Vec<_>>()
        })
}

/// The direct children of `pid` that are workset processes.
fn children(pid: u32) -> Vec<u32> {
    use std::os::unix::ffi::OsStrExt as _;

    let mut workers = child_pids(pid)
        .filter(|child| {
            command_line(*child)
                .split(|&byte| byte == 0)
                .next()
                .is_some_and(|program| {
                    Path::new(std::ffi::OsStr::from_bytes(program)).ends_with(WORKER)
                })
        })
        .collect::<Vec<_>>();
    workers.sort();
    workers
}

async fn open_terminal(
    paths: &rho_rpc::protocol::RuntimePaths,
    agent_id: AgentId,
    create: bool,
) -> anyhow::Result<rho_rpc::Stream> {
    let mut stream = rho_rpc::protocol::client::connect(paths).await?;
    let open = term::Open::Terminal {
        agent: agent_id.encoded(),
        terminal_id: 7,
        open: if create {
            TerminalOpen::Create { attach: true }
        } else {
            TerminalOpen::Attach
        },
        cols: 80,
        rows: 24,
    };
    write_open(&mut stream, &open).await?;
    match tokio::time::timeout(Duration::from_secs(30), read_frame(&mut stream)).await?? {
        Opened::Ready => Ok(stream),
        Opened::Refused { reason } => anyhow::bail!("refused: {reason}"),
    }
}

async fn wait_for_line(stream: &mut rho_rpc::Stream, needle: &str) -> anyhow::Result<()> {
    let mut screen = WireScreen::new(usize::MAX);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let frame = read_frame::<_, TermServerFrame>(stream).await?;
            if let TermServerFrame::Exited { status } = &frame {
                panic!("terminal exited early: {status:?}");
            }
            screen.apply(frame);
            let history = screen.scrollback.iter().filter_map(|item| match item {
                ScrollbackItem::Line(row) => Some(row.text()),
                ScrollbackItem::Gap(_) => None,
            });
            if history
                .chain(screen.rows.iter().map(TermRow::text))
                .any(|line| line.contains(needle))
            {
                return Ok(());
            }
        }
    })
    .await
    .with_context(|| format!("terminal did not show {needle:?}"))?
}

/// A new isolated workset with one agent on a clone of `repo`, made by
/// the default policy's `new_agent`, which prints its id. `message` is
/// the user's first message to it, a new root thread.
async fn create_agent(
    paths: &rho_rpc::protocol::RuntimePaths,
    repo: &std::path::Path,
    message: Option<&str>,
) -> anyhow::Result<rho_agent_types::AgentId> {
    let created = rho_rpc::protocol::client::call(
        paths,
        rho_agents_client::protocol::worksets::CreateWorkset {
            kind: rho_agent_types::WorksetKind::Isolated,
            code: format!(
                "print(new_agent(repo={}, runtime={{\"kind\": \"native\", \"model\": \"sol\", \"effort\": \"medium\"}}), end=\"\")",
                rho_agents_client::snippets::py_str(repo.to_str().unwrap())
            ),
        },
    )
    .await?;
    anyhow::ensure!(
        created.result.error.is_none(),
        "creation code failed: {:?}",
        created.result.error
    );
    let agent = rho_agent_types::AgentId::from_encoded(created.result.output.trim())?;
    if let Some(text) = message {
        brief(paths, agent, text).await?;
    }
    Ok(agent)
}

async fn brief(
    paths: &rho_rpc::protocol::RuntimePaths,
    agent: AgentId,
    text: &str,
) -> anyhow::Result<()> {
    rho_rpc::protocol::client::call(
        paths,
        rho_chat::protocol::Post {
            client_id: uuid::Uuid::new_v4().as_u64_pair().0,
            parent: None,
            settles: None,
            mentions: vec![agent],
            content: vec![rho_agent_types::ContentPart::Text {
                text: text.to_owned(),
            }],
        },
    )
    .await?;
    Ok(())
}
