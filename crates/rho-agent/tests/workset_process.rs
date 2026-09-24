use std::sync::Arc;
use std::time::Duration;

use rho_agent::pool::AgentPool;
use rho_agent::{StartPlace, WorksetAction, WorksetAttach, WorksetReply};
use rho_terminal::protocol::{TermClientFrame, TermServerFrame};

#[tokio::test]
async fn agents_and_terminal_share_workset_and_mode_change_drains_all_agents() {
    let (_directory, pool, workset, view) = fixture("http://127.0.0.1:1").await;
    let (first, a) = pool
        .create(
            Default::default(),
            Some("first".into()),
            StartPlace::new(view.clone(), None),
        )
        .await
        .unwrap();
    let (second, b) = pool
        .create(
            Default::default(),
            Some("second".into()),
            StartPlace::new(view, None),
        )
        .await
        .unwrap();
    let process = pool.execution(first).await.unwrap();
    assert!(Arc::ptr_eq(
        &process,
        &pool.execution(second).await.unwrap()
    ));
    let client = process
        .attach(WorksetAttach::Terminal {
            agent: first,
            terminal: 1,
            create: true,
            cols: 80,
            rows: 24,
            cwd: "/src".into(),
            shell: "bash".into(),
        })
        .await
        .unwrap();
    let (mut ui, bridge) = tokio::io::duplex(128 * 1024);
    let (reader, writer) = tokio::io::split(bridge);
    let relay =
        tokio::spawn(client.relay::<_, _, TermClientFrame, TermServerFrame>(reader, writer));
    rho_rpc::write_frame(&mut ui, &TermClientFrame::Input(
        b"printf '%s' $$ > terminal-pid; stat -Lc '%i' /proc/self/ns/mnt > terminal-ns; touch terminal-ready\n".to_vec()
    ), rho_rpc::parts::MAX_FRAME_LEN).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !workset.root().join("terminal-ready").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let terminal_pid = std::fs::read_to_string(workset.root().join("terminal-pid")).unwrap();
    let own_mount = std::fs::metadata("/proc/self/ns/mnt").unwrap();
    use std::os::unix::fs::MetadataExt as _;
    assert_ne!(
        std::fs::read_to_string(workset.root().join("terminal-ns"))
            .unwrap()
            .trim(),
        own_mount.ino().to_string()
    );
    assert!(std::path::Path::new(&format!("/proc/{terminal_pid}")).exists());

    let error = pool
        .change_mode(first, rho_agent_types::WorksetMode::Exposed)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("terminal"));
    rho_rpc::write_frame(
        &mut ui,
        &TermClientFrame::Input(b"exit\n".to_vec()),
        rho_rpc::parts::MAX_FRAME_LEN,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let WorksetReply::Terminals(entries) =
                process.action(WorksetAction::TerminalList).await.unwrap()
            else {
                panic!("terminal list")
            };
            if entries.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let changed = pool
        .change_mode(first, rho_agent_types::WorksetMode::Exposed)
        .await
        .unwrap();
    assert!(changed.contains(&first) && changed.contains(&second));
    assert!(pool.get(first).await.is_none());
    assert!(pool.get(second).await.is_none());
    let replacement = pool.execution(second).await.unwrap();
    assert!(!Arc::ptr_eq(&process, &replacement));
    let (_, reloaded, _) = pool.load(first).await.unwrap();
    assert_eq!(
        reloaded.view().await.unwrap().workset_mode(),
        rho_agent_types::WorksetMode::Exposed
    );
    // Development integration checks require both companions built first:
    // cargo build -p rho-agent -p rho-shell --bins
    use rho_shell_view::protocol::{ShellClientFrame, ShellServerFrame};
    let shell = std::path::Path::new(env!("CARGO_BIN_EXE_rho-agent-worker"))
        .ancestors()
        .map(|path| path.join("rho-shell"))
        .find(|path| path.is_file())
        .expect("build rho-shell companion before workset integration tests");
    replacement
        .action(WorksetAction::ShellStart {
            agent: first,
            cwd: "/src".into(),
            program: shell.clone(),
            pager: shell,
        })
        .await
        .unwrap();
    let shell_client = replacement
        .attach(WorksetAttach::Shell { agent: first })
        .await
        .unwrap();
    let (mut shell_ui, bridge) = tokio::io::duplex(128 * 1024);
    let (reader, writer) = tokio::io::split(bridge);
    let shell_relay = tokio::spawn(
        shell_client.relay::<_, _, ShellClientFrame, ShellServerFrame>(reader, writer),
    );
    rho_rpc::write_frame(
        &mut shell_ui,
        &ShellClientFrame::Submit {
            submission: 7,
            command: "printf multiplexed; printf written > shell-effect".into(),
        },
        rho_rpc::parts::MAX_FRAME_LEN,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (frame, _): (ShellServerFrame, _) =
                rho_rpc::read_frame(&mut shell_ui, rho_rpc::parts::MAX_FRAME_LEN)
                    .await
                    .unwrap();
            if matches!(frame, ShellServerFrame::Accepted { submission: 7, .. }) {
                break;
            }
        }
        while !workset.root().join("shell-effect").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(workset.root().join("shell-effect")).unwrap(),
        "written"
    );
    drop(shell_ui);
    let _ = shell_relay.await.unwrap();
    drop(reloaded);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let WorksetReply::Shells(shells) =
                replacement.action(WorksetAction::ShellList).await.unwrap()
            else {
                panic!("shell list")
            };
            assert_eq!(shells.len(), 1, "GUI detach closed retained shell");
            if shells[0].clients == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    replacement
        .action(WorksetAction::ShellClose { agent: first })
        .await
        .unwrap();
    drop(ui);
    let _ = relay.await.unwrap();
    drop(a);
    drop(b);
    replacement.shutdown().await;
    assert!(!std::path::Path::new(&format!("/proc/{terminal_pid}")).exists());
}

async fn fixture(
    endpoint: &str,
) -> (
    tempfile::TempDir,
    Arc<AgentPool>,
    rho_fs_view::Workset,
    Arc<rho_agent::View>,
) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_rho-agent-worker"));
    let mut environment = std::env::vars_os()
        .filter(|(key, _)| key != "PATH")
        .collect::<Vec<_>>();
    environment.push((
        "PATH".into(),
        std::env::join_paths(
            std::iter::once(binary.parent().unwrap().to_owned())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap(),
    ));
    let worksets = rho_fs_view::Worksets::open(
        root.join("state"),
        rho_fs_view::UserEnvironment::new(environment),
        Default::default(),
        rho_fs_view::StoreService::None,
    )
    .await
    .unwrap();
    let workset = worksets.create().await.unwrap();
    let view = workset
        .enter(
            rho_fs_view::Mode::View {
                home_skeleton: None,
            },
            camino::Utf8Path::new("/src"),
        )
        .unwrap();
    let db = rho_db::RhoDb::open(root.join("agents.redb"));
    let inference = rho_inference::Inference::new_with_config(
        db.clone(),
        rho_inference::InferenceConfig::with_responses_base_url(endpoint).unwrap(),
    )
    .await
    .unwrap();
    let pool = AgentPool::new(
        db,
        inference,
        worksets,
        rho_claude::accounts::ClaudePaths::at(
            camino::Utf8PathBuf::from_path_buf(root.join("claude")).unwrap(),
        ),
    )
    .await;
    (directory, pool, workset, view)
}

#[test]
fn streamed_side_effect_survives_workset_death_without_execution_replay() {
    if std::env::var_os("RHO_TEST_ISOLATED_AUTH").is_some() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(45), streaming_crash())
                .await
                .unwrap();
        });
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    let auth = home.path().join("state/rho/auth.d");
    std::fs::create_dir_all(&auth).unwrap();
    let file = auth.join("default.json");
    std::fs::write(
        &file,
        serde_json::to_vec(&serde_json::json!({
            "access_token": "synthetic-process-test-token", "expires_at_ms": u64::MAX,
            "account_id": "synthetic-process-test-account", "client_secret": vec![0u8; 32],
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "streamed_side_effect_survives_workset_death_without_execution_replay",
            "--nocapture",
        ])
        .env("RHO_TEST_ISOLATED_AUTH", "1")
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn streaming_crash() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::{SinkExt as _, StreamExt as _};
    use rho_agent::db::AgentReadTxnExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_directory, pool, workset, view) =
        fixture(&format!("http://{}", listener.local_addr().unwrap())).await;
    let db = pool.db().clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn({
        let requests = requests.clone();
        async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let requests = requests.clone();
                connections.spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
                    while let Some(Ok(message)) = socket.next().await {
                        if !message.is_text() { continue; }
                        let request: serde_json::Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                        assert_eq!(request["type"], "response.create");
                        let turn = requests.fetch_add(1, Ordering::SeqCst);
                        let source = if turn == 0 {
                            "import os\nfor entry in Path('/proc/self/fd').iterdir():\n    try:\n        assert 'agents.redb' not in os.readlink(str(entry))\n    except OSError:\n        pass\n\ncounter = 42\nPath('/src/worker-pid').write_text(str(os.getpid()))\nPath('/src/side-effect').write_text('written')\n"
                        } else {
                            "Path('/src/recovered').write_text(str(globals().get('counter')))\n"
                        };
                        for event in [
                            serde_json::json!({"type":"response.created","response":{"id":format!("resp_{turn}")}}),
                            serde_json::json!({"type":"response.output_item.added","output_index":0,
                                "item":{"type":"custom_tool_call","id":format!("ctc_{turn}"),"call_id":format!("call_{turn}"),"name":"exec"}}),
                            serde_json::json!({"type":"response.custom_tool_call_input.delta","output_index":0,"delta":source}),
                        ] {
                            socket.send(tokio_tungstenite::tungstenite::Message::Text(event.to_string().into())).await.unwrap();
                        }
                    }
                });
            }
        }
    });
    let (id, agent) = pool
        .create(
            Default::default(),
            Some("recovery-test".into()),
            StartPlace::new(view, None),
        )
        .await
        .unwrap();
    agent.send_user_message("run".into(), rho_agent_types::MessageDelivery::Immediate);
    tokio::time::timeout(Duration::from_secs(15), async {
        while !workset.root().join("side-effect").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("stream did not execute: {:?}", agent.status()));
    let worker: i32 = std::fs::read_to_string(workset.root().join("worker-pid"))
        .unwrap()
        .parse()
        .unwrap();
    let locked = db.write().await;
    let pending = tokio::spawn({
        let agent = agent.clone();
        async move {
            agent
                .send_user_content_accepted(
                    vec![rho_agent_types::ContentPart::Text {
                        text: "blocked".into(),
                    }],
                    rho_agent_types::MessageDelivery::NextRequest,
                )
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!pending.is_finished());
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(worker).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    drop(locked);
    let (_, replacement, _) = pool.load(id).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(workset.root().join("side-effect")).unwrap(),
        "written"
    );
    assert!(
        !db.read()
            .agent_event_records(id)
            .1
            .iter()
            .any(|(_, event)| matches!(
                event,
                rho_agent::AgentEvent::Native(
                    rho_agent::native::NativeEvent::ResponseFinished { .. }
                )
            ))
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "load replayed interrupted execution"
    );
    replacement.send_user_message(
        "inspect fresh globals".into(),
        rho_agent_types::MessageDelivery::Immediate,
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        while !workset.root().join("recovered").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("replacement did not execute: {:?}", replacement.status()));
    assert_eq!(
        std::fs::read_to_string(workset.root().join("recovered")).unwrap(),
        "None"
    );
    pool.execution(id).await.unwrap().shutdown().await;
    server.abort();
}
