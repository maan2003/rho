//! Protocol regression for the intentionally unsupported Join-User mode.

use std::time::Duration;

use rho_ui_proto::{ClientMessage, JoinTarget, ServerMessage, StartMode, read_frame, write_frame};

#[tokio::test]
async fn joining_the_user_checkout_is_rejected_honestly() -> anyhow::Result<()> {
    let state_dir = tempfile::tempdir()?;
    // SAFETY: this integration test owns its process-local state directory.
    unsafe { std::env::set_var("XDG_STATE_HOME", state_dir.path()) };
    let socket_path = state_dir.path().join("rho.sock");

    tokio::spawn(rho_daemon::run(rho_daemon::DaemonArgs {
        socket_path: Some(socket_path.clone()),
        iroh: false,
        cpu_profile: None,
        extra_before_path: None,
        extra_after_path: None,
    }));
    let mut control = loop {
        match rho_rpc::connect_unix(&socket_path).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };

    write_frame(&mut control, &ClientMessage::Subscribe).await?;
    let ServerMessage::Ready { .. } =
        tokio::time::timeout(Duration::from_secs(30), read_frame(&mut control)).await??
    else {
        panic!("daemon did not greet with Ready");
    };
    write_frame(
        &mut control,
        &ClientMessage::NewAgent {
            role: Default::default(),
            start: StartMode::Join(JoinTarget::User {
                repo: "/tmp/user-checkout".into(),
            }),
            content: None,
            desk_anchor: None,
        },
    )
    .await?;

    loop {
        match tokio::time::timeout(Duration::from_secs(30), read_frame(&mut control)).await?? {
            ServerMessage::Error { message } => {
                assert_eq!(
                    message,
                    "joining the user's live checkout is not supported in the workset model yet"
                );
                return Ok(());
            }
            ServerMessage::AgentCreated { .. } => panic!("Join User silently created an agent"),
            _ => {}
        }
    }
}
