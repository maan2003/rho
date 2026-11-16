//! Faithful repro of the live co-located deployment: IP (loopback UDP) and
//! IPC transports **coexist**, the dial address carries both candidates, and
//! `PreferIpcTransport` is installed — exactly what a wwkg host↔custodian
//! pair looks like. The IPC-only echo/burst tests can never see the
//! seconds-scale first-`put` stall observed live, because they remove the
//! competing path; this test times what actually happens when iroh must
//! *choose*. A live local lease selects a local-only dial target before
//! connecting, while both endpoints retain their IP transports for remote peers.

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use iroh::endpoint::{Endpoint, presets};
use iroh::{EndpointAddr, SecretKey, TransportAddr};
use iroh_ipc_transport::{IpcTransport, LocalDiscovery, PreferIpcTransport};

const ALPN: &[u8] = b"iroh-ipc-transport/coexist";
const PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Endpoint with BOTH its default IP transport (loopback UDP candidates) and
/// the IPC transport; relay cleared so the test is hermetic.
async fn coexist_endpoint(
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
        .clear_relay_transports()
        .bind()
        .await
        .expect("bind coexist endpoint");
    (endpoint, transport)
}

fn loopback_udp_addr(ep: &Endpoint) -> SocketAddr {
    let port = ep
        .bound_sockets()
        .iter()
        .find(|a| a.is_ipv4())
        .expect("ipv4 bound socket")
        .port();
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

#[tokio::test(flavor = "multi_thread")]
async fn first_rtt_and_large_transfer_with_coexisting_paths() {
    let dir = common::TestDir::new().await;
    let path_a = dir.path().join("a.ipc.sock");
    let path_b = dir.path().join("b.ipc.sock");

    let secret_b = SecretKey::from([2u8; 32]);
    let b_id = secret_b.public();

    let (ep_a, _ta) = coexist_endpoint(SecretKey::from([1u8; 32]), &path_a, vec![]).await;
    let (ep_b, tb) = coexist_endpoint(secret_b, &path_b, vec![ALPN.to_vec()]).await;

    let b_udp = loopback_udp_addr(&ep_b);

    let server_endpoint = ep_b.clone();
    // Server: echo one small stream, then drain one large stream and ack.
    let server = tokio::spawn(async move {
        let incoming = ep_b.accept().await.expect("incoming");
        let conn = incoming.await.expect("accept connection");

        let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi small");
        let msg = recv.read_to_end(64).await.expect("read small");
        send.write_all(&msg).await.expect("echo small");
        send.finish().expect("finish small");

        let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi large");
        let received = recv
            .read_to_end(PAYLOAD_BYTES + 1)
            .await
            .expect("read large");
        send.write_all(&(received.len() as u64).to_le_bytes())
            .await
            .expect("ack large");
        send.finish().expect("finish large");

        conn.closed().await;
        server_endpoint.close().await;
        received.len()
    });

    // Dial with BOTH candidates, like discovery provides live.
    let b_addr = EndpointAddr::from_parts(
        b_id,
        [
            TransportAddr::Ip(b_udp),
            TransportAddr::Custom(tb.local_addr()),
        ],
    );

    let b_addr = LocalDiscovery::new(dir.path())
        .resolve_target(b_addr)
        .await
        .expect("local route");

    let t0 = Instant::now();
    let conn = ep_a.connect(b_addr, ALPN).await.expect("connect");
    let t_connect = t0.elapsed();

    let t1 = Instant::now();
    let (mut send, mut recv) = conn.open_bi().await.expect("open_bi small");
    send.write_all(b"ping").await.expect("write small");
    send.finish().expect("finish small");
    let echoed = recv.read_to_end(64).await.expect("read echo");
    let t_first_rtt = t1.elapsed();
    assert_eq!(echoed, b"ping");

    let t2 = Instant::now();
    let (mut send, mut recv) = conn.open_bi().await.expect("open_bi large");
    send.write_all(&vec![0xA5u8; PAYLOAD_BYTES])
        .await
        .expect("write large");
    send.finish().expect("finish large");
    let mut ack = [0u8; 8];
    recv.read_exact(&mut ack).await.expect("read ack");
    let t_large = t2.elapsed();
    assert_eq!(u64::from_le_bytes(ack) as usize, PAYLOAD_BYTES);
    assert!(
        conn.paths().iter().any(|path| path.is_selected()
            && matches!(path.remote_addr(), TransportAddr::Custom(addr)
            if addr.id() == iroh_ipc_transport::IPC_TRANSPORT_ID)),
        "the local custom path must carry the transfer"
    );

    conn.close(0u32.into(), b"done");
    let received = server.await.expect("server task");
    assert_eq!(received, PAYLOAD_BYTES);

    // Print the timings; thresholds come after we see the numbers.
    eprintln!("connect:        {t_connect:?}");
    eprintln!("first rtt:      {t_first_rtt:?}");
    eprintln!("8 MiB transfer: {t_large:?}");
    ep_a.close().await;
    _ta.close().await.expect("close client transport");
    tb.close().await.expect("close server transport");
    dir.close().await;
}
