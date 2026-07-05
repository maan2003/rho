//! End-to-end smoke test for zed channels: a real daemon on a temp socket,
//! a dedicated stream opened onto this repo's user checkout, and a proto
//! Ping answered by the in-process HeadlessProject session.

use std::time::Duration;

use prost::Message as _;
use rho_ui_proto::{
    ClientMessage, ServerMessage, WorkspaceInfo, read_frame, read_raw_frame, write_frame,
    write_raw_frame,
};

#[tokio::test]
async fn zed_channel_ping_round_trip() -> anyhow::Result<()> {
    let state_dir = tempfile::tempdir()?;
    // Keep the daemon's state (redb, sockets) away from the user's real one.
    // SAFETY: this integration test binary has no other threads yet.
    unsafe { std::env::set_var("XDG_STATE_HOME", state_dir.path()) };
    let socket_path = state_dir.path().join("rho.sock");

    tokio::spawn(rho_daemon::run(rho_daemon::DaemonArgs {
        auth: "default".to_owned(),
        socket_path: Some(socket_path.clone()),
        die_on_detached: false,
        iroh: false,
        cpu_profile: None,
        extra_before_path: None,
        extra_after_path: None,
    }));
    let mut stream = loop {
        match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };

    // The rho repo itself doubles as the workspace: a user checkout needs no
    // jj workspace forking. The whole stream is dedicated to the channel:
    // ChannelOpen is its first frame.
    let repo = camino::Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    write_frame(
        &mut stream,
        &ClientMessage::ChannelOpen {
            workspace: WorkspaceInfo::UserCheckout { repo },
        },
    )
    .await?;

    let reply: ServerMessage =
        tokio::time::timeout(Duration::from_secs(30), read_frame(&mut stream)).await??;
    let root = match reply {
        ServerMessage::ChannelOpened { root } => root,
        ServerMessage::ChannelClosed { reason } => panic!("daemon refused channel: {reason}"),
        other => panic!("unexpected handshake reply: {other:?}"),
    };
    // The daemon reports its own canonical view of the checkout (which may
    // be the ws-parent bind mount rather than the origin path); all that
    // matters is that the checkout is reachable there.
    assert!(root.join("Cargo.toml").exists(), "bad root: {root}");

    let ping = rpc::proto::Envelope {
        id: 1,
        payload: Some(rpc::proto::envelope::Payload::Ping(rpc::proto::Ping {})),
        ..Default::default()
    };
    write_raw_frame(&mut stream, &ping.encode_to_vec()).await?;

    loop {
        let payload =
            tokio::time::timeout(Duration::from_secs(30), read_raw_frame(&mut stream))
                .await??
                .expect("channel stream closed while waiting for ack");
        let envelope = rpc::proto::Envelope::decode(payload.as_slice())?;
        if envelope.responding_to == Some(1) {
            assert!(matches!(
                envelope.payload,
                Some(rpc::proto::envelope::Payload::Ack(_))
            ));
            return Ok(());
        }
    }
}
