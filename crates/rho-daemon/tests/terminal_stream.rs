//! End-to-end smoke test for terminal streams: a real daemon on a temp
//! socket, an agent joined to a plain temp checkout, a shell echoing through
//! the dedicated stream, and a second attach after detach proving the
//! terminal survived.

use std::time::Duration;

use rho_ui_proto::term::{ScrollbackItem, TermClientFrame, TermRow, TermServerFrame, WireScreen};
use rho_ui_proto::{
    AgentId, ClientMessage, JoinTarget, ServerMessage, StartMode, read_frame, write_frame,
};

#[tokio::test]
async fn terminal_survives_detach_and_echoes() -> anyhow::Result<()> {
    let state_dir = tempfile::tempdir()?;
    // Keep the daemon's state (redb, sockets) away from the user's real one.
    // SAFETY: this integration test binary has no other threads yet.
    unsafe { std::env::set_var("XDG_STATE_HOME", state_dir.path()) };
    let socket_path = state_dir.path().join("rho.sock");
    // Agent workdirs must be jj repos, even when joining the user checkout.
    let repo_dir = tempfile::tempdir()?;
    let jj_init = std::process::Command::new("jj")
        .args(["git", "init"])
        .current_dir(repo_dir.path())
        .output();
    if !jj_init.is_ok_and(|output| output.status.success()) {
        eprintln!("skipping: jj unavailable");
        return Ok(());
    }

    tokio::spawn(rho_daemon::run(rho_daemon::DaemonArgs {
        auth: "default".to_owned(),
        socket_path: Some(socket_path.clone()),
        die_on_detached: false,
        iroh: false,
        iroh_bbr3: false,
        cpu_profile: None,
        extra_before_path: None,
        extra_after_path: None,
    }));
    let mut control = loop {
        match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };

    // Create an agent working directly in the plain temp checkout: no jj
    // forking, no namespace mounts, nothing outside the temp dirs.
    write_frame(&mut control, &ClientMessage::Subscribe).await?;
    let ServerMessage::Ready { .. } =
        tokio::time::timeout(Duration::from_secs(30), read_frame(&mut control)).await??
    else {
        panic!("daemon did not greet with Ready");
    };
    write_frame(
        &mut control,
        &ClientMessage::NewAgent {
            tags: Vec::new(),
            role: Default::default(),
            start: StartMode::Join(JoinTarget::User {
                repo: camino::Utf8PathBuf::from_path_buf(repo_dir.path().to_owned()).unwrap(),
            }),
            content: None,
        },
    )
    .await?;
    let agent_id = loop {
        match tokio::time::timeout(Duration::from_secs(30), read_frame(&mut control)).await?? {
            ServerMessage::AgentCreated { agent_id, .. } => break agent_id,
            ServerMessage::Error { message } => panic!("agent creation failed: {message}"),
            _ => {}
        }
    };

    // Attaching before anything was created must be refused.
    let refused = open_terminal(&socket_path, agent_id, false).await;
    assert!(refused.is_err(), "attach without create must be refused");

    // Create + attach: run a command and watch its output (the needle appears
    // only after execution joins the quoted halves, in any shell).
    let mut stream = open_terminal(&socket_path, agent_id, true).await?;
    write_frame(
        &mut stream,
        &TermClientFrame::Input(b"echo \"e2\"\"e-done\"\r".to_vec()),
    )
    .await?;
    wait_for_line(&mut stream, "e2e-done").await?;

    // The listing sees the running terminal.
    let mut list = tokio::net::UnixStream::connect(&socket_path).await?;
    write_frame(
        &mut list,
        &ClientMessage::TerminalList {
            agent: Some(agent_id.encoded()),
        },
    )
    .await?;
    match tokio::time::timeout(Duration::from_secs(30), read_frame(&mut list)).await?? {
        ServerMessage::TerminalList { terminals } => {
            assert_eq!(terminals.len(), 1, "one terminal should be running");
            assert_eq!(terminals[0].terminal_id, 7);
            assert_eq!(terminals[0].clients, 1);
        }
        other => panic!("unexpected terminal list reply: {other:?}"),
    }
    drop(stream);

    // Second attach after detach: the shell kept running, and the snapshot
    // replays the earlier output.
    let mut stream = open_terminal(&socket_path, agent_id, false).await?;
    wait_for_line(&mut stream, "e2e-done").await?;

    // Creating the same id again must be refused.
    let refused = open_terminal(&socket_path, agent_id, true).await;
    assert!(refused.is_err(), "duplicate create must be refused");
    Ok(())
}

async fn open_terminal(
    socket_path: &std::path::Path,
    agent_id: AgentId,
    create: bool,
) -> anyhow::Result<tokio::net::UnixStream> {
    let mut stream = tokio::net::UnixStream::connect(socket_path).await?;
    let open = if create {
        ClientMessage::TerminalCreate {
            agent: agent_id.encoded(),
            terminal_id: 7,
            attach: true,
            cols: 80,
            rows: 24,
        }
    } else {
        ClientMessage::TerminalAttach {
            agent: agent_id.encoded(),
            terminal_id: 7,
            cols: 80,
            rows: 24,
        }
    };
    write_frame(&mut stream, &open).await?;
    match tokio::time::timeout(Duration::from_secs(30), read_frame(&mut stream)).await?? {
        ServerMessage::TerminalOpened { .. } => Ok(stream),
        ServerMessage::TerminalRefused { reason } => anyhow::bail!("refused: {reason}"),
        other => panic!("unexpected handshake reply: {other:?}"),
    }
}

fn frame_kind(frame: &TermServerFrame) -> String {
    match frame {
        TermServerFrame::Snapshot(screen) => format!(
            "Snapshot({} rows: {:?})",
            screen.rows.len(),
            screen
                .rows
                .iter()
                .map(TermRow::text)
                .filter(|row| !row.is_empty())
                .collect::<Vec<_>>()
        ),
        TermServerFrame::Screen { rows, .. } => format!(
            "Screen({:?})",
            rows.iter()
                .map(|(i, row)| (i, row.text()))
                .collect::<Vec<_>>()
        ),
        TermServerFrame::History { lines, lost } => {
            format!("History({} lines, lost {lost})", lines.len())
        }
        TermServerFrame::Title(title) => format!("Title({title})"),
        TermServerFrame::Exited { status } => format!("Exited({status:?})"),
    }
}

async fn wait_for_line(stream: &mut tokio::net::UnixStream, needle: &str) -> anyhow::Result<()> {
    let mut screen = WireScreen::new(usize::MAX);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let frame = read_frame::<_, TermServerFrame>(stream).await?;
            eprintln!("frame: {:?}", frame_kind(&frame));
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
