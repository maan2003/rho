//! Proof: two iroh endpoints connect and echo over the IPC transport **only**
//! — no IP, no relay — so a successful round-trip means the bytes rode the
//! Unix socket as a native iroh path.

mod common;

use std::sync::Arc;

use iroh::endpoint::{Endpoint, presets};
use iroh::{EndpointAddr, SecretKey};
use iroh_ipc_transport::{IpcTransport, LocalDiscovery, PreferIpcTransport};

const ECHO_ALPN: &[u8] = b"iroh-ipc-transport/echo";

/// Build a custom-transport-**only** endpoint (IP and relay cleared) bound to
/// `socket_path`. If a connection works at all, it can only be the IPC path.
async fn ipc_only_endpoint(
    secret: SecretKey,
    socket_path: &std::path::Path,
    alpns: Vec<Vec<u8>>,
) -> (Endpoint, IpcTransport) {
    let transport = IpcTransport::prepare(
        LocalDiscovery::new(socket_path.parent().expect("namespace")),
        secret.public(),
    )
    .await
    .expect("prepare IPC");
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .address_lookup(LocalDiscovery::new(
            socket_path.parent().expect("namespace"),
        ))
        .alpns(alpns)
        .add_custom_transport(Arc::new(transport.clone()))
        .path_selector(Arc::new(PreferIpcTransport))
        .clear_ip_transports()
        .clear_relay_transports()
        .bind()
        .await
        .expect("bind ipc-only endpoint");
    (endpoint, transport)
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_round_trips_over_the_ipc_transport_only() {
    let dir = common::TestDir::new().await;
    let path_a = dir.path().join("a.ipc.sock");
    let path_b = dir.path().join("b.ipc.sock");

    let secret_b = SecretKey::from([2u8; 32]);
    let b_id = secret_b.public();

    let (ep_a, _ta) = ipc_only_endpoint(SecretKey::from([1u8; 32]), &path_a, vec![]).await;
    let (ep_b, tb) = ipc_only_endpoint(secret_b, &path_b, vec![ECHO_ALPN.to_vec()]).await;

    let server_endpoint = ep_b.clone();
    // Server: accept one connection, echo one bidi stream.
    let server = tokio::spawn(async move {
        let incoming = ep_b.accept().await.expect("incoming");
        let conn = incoming.await.expect("accept connection");
        let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi");
        let msg = recv.read_to_end(64).await.expect("read");
        send.write_all(&msg).await.expect("echo write");
        send.finish().expect("finish");
        conn.closed().await;
        server_endpoint.close().await;
    });

    // Resolve B through the crate-owned local discovery; IP and relay are disabled.
    let b_addr = EndpointAddr::from(b_id);
    let conn = ep_a
        .connect(b_addr, ECHO_ALPN)
        .await
        .expect("connect over ipc");

    let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
    send.write_all(b"ping").await.expect("write");
    send.finish().expect("finish");
    let echoed = recv.read_to_end(64).await.expect("read echo");
    assert_eq!(echoed, b"ping", "echo round-trips over the IPC socket");

    conn.close(0u32.into(), b"done");
    server.await.expect("server task");
    ep_a.close().await;
    _ta.close().await.expect("close client transport");
    tb.close().await.expect("close server transport");
    dir.close().await;
}
