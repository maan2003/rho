//! A re-exec keeps the workset processes: a real agent host binary, an
//! agent's terminal holding shell state and an agent partway through a turn
//! of sequential tool calls, SIGUSR2, and then the same shell answering and
//! the same turn finishing through the successor, under the same pids.
//! Harness-free, as `terminal_stream`, and for the same reason.

use std::time::Duration;

use rho_agent_types::AgentId;
use rho_agents_client::protocol::{NewAgent, StartMode};
use rho_rpc::protocol::{Opened, read_frame, write_frame, write_open};
use rho_terminal::protocol as term;
use rho_terminal::protocol::{
    ScrollbackItem, TermClientFrame, TermRow, TermServerFrame, TerminalList, TerminalOpen,
    WireScreen,
};

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--list") {
        if !std::env::args().any(|arg| arg == "--ignored") {
            println!("e2e: test");
        }
        return Ok(());
    }
    let unshare = std::process::Command::new("unshare")
        .args(["-U", "true"])
        .status();
    if !unshare.map(|status| status.success()).unwrap_or(false) {
        eprintln!("skipping reexec: kernel forbids unshare(CLONE_NEWUSER)");
        return Ok(());
    }
    let state_dir = tempfile::tempdir()?;
    let socket_path = state_dir.path().join("rho.sock");
    // The rig's synthetic account: the fake model takes any token.
    let auth = state_dir.path().join("rho").join("auth.d");
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
    let runtime = tokio::runtime::Runtime::new()?;
    let mut config = rho_fake_model::FakeModelConfig::seeded(0);
    config.scenario = rho_fake_model::Scenario::RealToolRounds;
    config.real_tool_rounds = ROUNDS;
    config.distribution.rate_limit_bps = 0;
    config.distribution.usage_limit_bps = 0;
    config.distribution.overload_bps = 0;
    config.distribution.disconnect_bps = 0;
    let model = runtime.block_on(rho_fake_model::FakeModel::start(config))?;
    let mut host = std::process::Command::new(env!("CARGO_BIN_EXE_rho-agent-host"))
        .arg("--socket-path")
        .arg(&socket_path)
        .arg("--claude-config-dir")
        .arg(state_dir.path().join("claude"))
        .args(["--openai-base-url", &model.openai_base_url()])
        .env("XDG_STATE_HOME", state_dir.path())
        .env("SHELL", "bash")
        .spawn()?;
    let result = runtime.block_on(work_outlives_reexec(host.id(), state_dir.path(), &model));
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(host.id() as i32).unwrap(),
        rustix::process::Signal::TERM,
    );
    let _ = host.wait();
    if result.is_ok() {
        println!("reexec passed");
    }
    result
}

/// Tool calls in the turn, each checking the one before it ran exactly once.
const ROUNDS: usize = 60;

async fn work_outlives_reexec(
    host: u32,
    state_dir: &std::path::Path,
    model: &rho_fake_model::FakeModel,
) -> anyhow::Result<()> {
    let socket_path = &state_dir.join("rho.sock");
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
    wait_for_socket(socket_path).await?;
    let new_agent = |content| {
        tokio::time::timeout(
            Duration::from_secs(60),
            rho_rpc::protocol::client::call(
                socket_path,
                NewAgent {
                    role: Default::default(),
                    start: StartMode::NewOn {
                        repo: camino::Utf8PathBuf::from_path_buf(repo_dir.clone()).unwrap(),
                        revset: "@".to_owned(),
                    },
                    content,
                },
            ),
        )
    };
    let agent_id = new_agent(None).await??;

    // State only the running shell holds: a variable, and a job that is
    // still sleeping when the agent host re-executes.
    let mut stream = open_terminal(socket_path, agent_id, true).await?;
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

    new_agent(Some(vec![rho_agent_types::ContentPart::Text {
        text: "Run the rounds.".to_owned(),
    }]))
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
    wait_for_socket(socket_path).await?;
    let terminals = tokio::time::timeout(
        Duration::from_secs(30),
        rho_rpc::protocol::client::call(
            socket_path,
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
    let mut stream = open_terminal(socket_path, agent_id, false).await?;
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
    Ok(())
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

async fn wait_for_socket(socket_path: &std::path::Path) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match rho_rpc::connect_unix(socket_path).await {
                Ok(_) => return,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await?;
    Ok(())
}

/// The direct children of `pid` that are workset processes.
fn children(pid: u32) -> Vec<u32> {
    let mut children = Vec::new();
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let listed = std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
        for child in listed
            .split_whitespace()
            .filter_map(|child| child.parse::<u32>().ok())
        {
            let exe = std::fs::read_link(format!("/proc/{child}/exe")).unwrap_or_default();
            if exe
                .file_name()
                .is_some_and(|name| name == "rho-agent-worker")
            {
                children.push(child);
            }
        }
    }
    children.sort();
    children
}

async fn open_terminal(
    socket_path: &std::path::Path,
    agent_id: AgentId,
    create: bool,
) -> anyhow::Result<rho_rpc::Stream> {
    let mut stream = rho_rpc::connect_unix(socket_path).await?;
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
    .await?
}
