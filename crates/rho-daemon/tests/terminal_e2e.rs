//! The terminal registry end to end: a shell under `direnv exec` inside a
//! view-mode namespace over a temporary workset. Harness-free because the
//! identity user namespace must precede every thread.

use std::sync::Arc;

use rho_daemon::terminal::{ClientInput, TerminalClient, TerminalRegistry, TerminalSpawn};
use rho_ui_proto::AgentId;
use rho_ui_proto::term::{ScrollbackItem, TermRow, TermServerFrame, WireScreen};

fn main() {
    let unshare = std::process::Command::new("unshare")
        .args(["-U", "true"])
        .status();
    if !unshare.map(|status| status.success()).unwrap_or(false) {
        eprintln!("skipping terminal_e2e: kernel forbids unshare(CLONE_NEWUSER)");
        return;
    }
    if !std::process::Command::new("direnv")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping terminal_e2e: direnv unavailable");
        return;
    }
    // SAFETY: top of main, before the runtime: no threads exist yet.
    unsafe { rho_fs_view::init_daemon_namespace() }.unwrap();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(terminal_end_to_end_over_registry());
    println!("terminal e2e passed");
}

async fn terminal_end_to_end_over_registry() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let worksets = rho_fs_view::Worksets::open(
        temp.path().join("state"),
        rho_fs_view::UserEnvironment::new(std::env::vars_os().collect()),
        Default::default(),
        rho_fs_view::StoreService::None,
    )
    .await
    .unwrap();
    let view = worksets
        .adopt(&work)
        .unwrap()
        .enter(
            rho_fs_view::Mode::View {
                home_skeleton: None,
            },
            camino::Utf8Path::new(rho_fs_view::MOUNT_ROOT),
        )
        .unwrap();

    let registry = Arc::new(TerminalRegistry::default());
    let agent_id =
        AgentId::from_counter(1, &rho_agent::db::AgentIdDomain(42)).expect("counter 1 encodes");
    let mut client = registry
        .create(
            agent_id,
            0,
            80,
            24,
            TerminalSpawn {
                view: Arc::clone(&view),
                shell: "sh".to_owned(),
            },
        )
        .await
        .unwrap();
    client
        .input
        .send(ClientInput::Bytes(b"echo term-e2e-$((20+3))\r".to_vec()))
        .unwrap();
    wait_for_line(&mut client, "term-e2e-23").await;

    // A second client attaching sees the same screen from its snapshot,
    // and the listing shows the one running terminal.
    let mut second = registry.attach(agent_id, 0, 80, 24).await.unwrap();
    wait_for_line(&mut second, "term-e2e-23").await;
    let listed = registry.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].terminal_id, 0);
    assert_eq!(listed[0].clients, 2);
    assert!(
        registry.attach(agent_id, 7, 80, 24).await.is_err(),
        "attach must refuse ids that are not running"
    );

    client
        .input
        .send(ClientInput::Bytes(b"exit\r".to_vec()))
        .unwrap();
    let exited = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match client.frames.recv().await {
                Some(TermServerFrame::Exited { .. }) | None => break,
                Some(_) => {}
            }
        }
    })
    .await;
    assert!(exited.is_ok(), "terminal exit must reach the client");
}

/// Receives frames until some screen row or history line contains
/// `needle` (panics after 20s).
async fn wait_for_line(client: &mut TerminalClient, needle: &str) {
    let mut screen = WireScreen::new(usize::MAX);
    let found = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let frame = client.frames.recv().await.expect("terminal stream ended");
            if matches!(&frame, TermServerFrame::Exited { .. }) {
                panic!("terminal exited early");
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
                break;
            }
        }
    })
    .await;
    assert!(found.is_ok(), "expected {needle:?} on the terminal");
}
