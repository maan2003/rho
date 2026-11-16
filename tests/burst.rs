//! Burst proof: a large multi-megabyte transfer over the IPC transport **only**
//! completes promptly.
//!
//! This is the regression guard for the co-located-`put` stall: QUIC bursts
//! many datagrams for a large transfer, and a `poll_recv` that drained only one
//! datagram per wakeup (or a socket whose receive buffer overflowed mid-burst)
//! turned that into multi-second stalls. With the batched drain + enlarged
//! buffers, 8 MiB rides the Unix socket well under a generous deadline.

mod common;

use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{Endpoint, presets};
use iroh::{EndpointAddr, SecretKey, TransportAddr};
use iroh_ipc_transport::{IpcTransport, LocalDiscovery, PreferIpcTransport};

const BURST_ALPN: &[u8] = b"iroh-ipc-transport/burst";
const PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
/// Generous upper bound: local IPC should move 8 MiB in well under a second;
/// the pre-fix one-datagram-per-poll drain turned this into multi-second
/// stalls. A 20 s ceiling fails loudly on a regression without flaking on a
/// loaded CI box.
const DEADLINE: Duration = Duration::from_secs(20);

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
async fn large_transfer_over_ipc_completes_promptly() {
    let dir = common::TestDir::new().await;
    let path_a = dir.path().join("a.ipc.sock");
    let path_b = dir.path().join("b.ipc.sock");

    let secret_b = SecretKey::from([2u8; 32]);
    let b_id = secret_b.public();

    let (ep_a, _ta) = ipc_only_endpoint(SecretKey::from([1u8; 32]), &path_a, vec![]).await;
    let (ep_b, tb) = ipc_only_endpoint(secret_b, &path_b, vec![BURST_ALPN.to_vec()]).await;

    let server_endpoint = ep_b.clone();
    // Server: drain the whole stream, reply with the byte count it received.
    let server = tokio::spawn(async move {
        let incoming = ep_b.accept().await.expect("incoming");
        let conn = incoming.await.expect("accept connection");
        let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi");
        let received = recv
            .read_to_end(PAYLOAD_BYTES + 1)
            .await
            .expect("read payload");
        send.write_all(&(received.len() as u64).to_le_bytes())
            .await
            .expect("ack write");
        send.finish().expect("finish");
        conn.closed().await;
        server_endpoint.close().await;
        received.len()
    });

    let b_addr = EndpointAddr::from_parts(b_id, [TransportAddr::Custom(tb.local_addr())]);
    let conn = ep_a
        .connect(b_addr, BURST_ALPN)
        .await
        .expect("connect over ipc");

    let transfer = async {
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
        let payload = vec![0xA5u8; PAYLOAD_BYTES];
        send.write_all(&payload).await.expect("write payload");
        send.finish().expect("finish");
        let mut ack = [0u8; 8];
        recv.read_exact(&mut ack).await.expect("read ack");
        u64::from_le_bytes(ack) as usize
    };

    let acked = tokio::time::timeout(DEADLINE, transfer)
        .await
        .expect("8 MiB transfer must complete before the deadline (no burst stall)");
    assert_eq!(acked, PAYLOAD_BYTES, "server received the whole payload");

    conn.close(0u32.into(), b"done");
    let received = server.await.expect("server task");
    assert_eq!(received, PAYLOAD_BYTES, "server drained the whole payload");
    ep_a.close().await;
    _ta.close().await.expect("close client transport");
    tb.close().await.expect("close server transport");
    dir.close().await;
}
