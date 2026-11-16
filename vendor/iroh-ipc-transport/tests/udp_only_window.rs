//! Diagnostic: what does a co-located pair look like during the window when
//! the dialer does NOT yet know the peer's IPC address (discovery hasn't
//! delivered it)? The dial address carries only UDP candidates — first the
//! peer's real interface IP (what mDNS/DHT advertise live), then loopback —
//! and we time connect + first round-trip + an 8 MiB transfer on each.
//!
//! This models iroh#4292 territory: same-host pairs over UDP. If this is
//! where the seconds live, the fix is making the IPC address available *at*
//! dial time, not tuning the datagram path.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use iroh::endpoint::{Endpoint, presets};
use iroh::{EndpointAddr, SecretKey, TransportAddr};
use iroh_ipc_transport::{IpcTransport, LocalDiscovery, PreferIpcTransport};

const ALPN: &[u8] = b"iroh-ipc-transport/udp-window";
const PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

async fn coexist_endpoint(
    secret: SecretKey,
    socket_path: &std::path::Path,
    alpns: Vec<Vec<u8>>,
) -> Endpoint {
    let transport = IpcTransport::prepare(
        LocalDiscovery::new(socket_path.parent().expect("namespace")),
        secret.public(),
    )
    .await
    .expect("prepare IPC");
    Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(alpns)
        .add_custom_transport(Arc::new(transport))
        .path_selector(Arc::new(PreferIpcTransport))
        .clear_relay_transports()
        .bind()
        .await
        .expect("bind endpoint")
}

fn ipv4_port(ep: &Endpoint) -> u16 {
    ep.bound_sockets()
        .iter()
        .find(|a| a.is_ipv4())
        .expect("ipv4 bound socket")
        .port()
}

/// The machine's primary non-loopback IPv4 (what discovery would advertise).
fn lan_ipv4() -> Option<Ipv4Addr> {
    // Route-based trick: connect a UDP socket outward, read the local addr.
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
        _ => None,
    }
}

async fn run_case(label: &str, dial_ip: Ipv4Addr) {
    let dir = common::TestDir::new().await;
    let secret_b = SecretKey::from([2u8; 32]);
    let b_id = secret_b.public();

    let ep_a = coexist_endpoint(
        SecretKey::from([1u8; 32]),
        &dir.path().join("a.ipc.sock"),
        vec![],
    )
    .await;
    let ep_b = coexist_endpoint(
        secret_b,
        &dir.path().join("b.ipc.sock"),
        vec![ALPN.to_vec()],
    )
    .await;

    let b_port = ipv4_port(&ep_b);

    let server = tokio::spawn(async move {
        let incoming = ep_b.accept().await.expect("incoming");
        let conn = incoming.await.expect("accept connection");
        let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi small");
        let msg = recv.read_to_end(64).await.expect("read small");
        send.write_all(&msg).await.expect("echo");
        send.finish().expect("finish");
        let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi large");
        let received = recv
            .read_to_end(PAYLOAD_BYTES + 1)
            .await
            .expect("read large");
        send.write_all(&(received.len() as u64).to_le_bytes())
            .await
            .expect("ack");
        send.finish().expect("finish");
        conn.closed().await;
    });

    // UDP-only dial: the IPC addr is NOT known to the dialer.
    let b_addr = EndpointAddr::from_parts(
        b_id,
        [TransportAddr::Ip(SocketAddr::from((dial_ip, b_port)))],
    );

    let t0 = Instant::now();
    let conn = ep_a.connect(b_addr, ALPN).await.expect("connect");
    let t_connect = t0.elapsed();

    let t1 = Instant::now();
    let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
    send.write_all(b"ping").await.expect("write");
    send.finish().expect("finish");
    let echoed = recv.read_to_end(64).await.expect("read echo");
    let t_first = t1.elapsed();
    assert_eq!(echoed, b"ping");

    let t2 = Instant::now();
    let (mut send, mut recv) = conn.open_bi().await.expect("open_bi large");
    send.write_all(&vec![0xA5u8; PAYLOAD_BYTES])
        .await
        .expect("write large");
    send.finish().expect("finish");
    let mut ack = [0u8; 8];
    recv.read_exact(&mut ack).await.expect("read ack");
    let t_large = t2.elapsed();

    conn.close(0u32.into(), b"done");
    let _ = server.await;

    eprintln!("[{label}] connect: {t_connect:?}   first rtt: {t_first:?}   8 MiB: {t_large:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_only_loopback() {
    run_case("udp-only loopback", Ipv4Addr::LOCALHOST).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_only_lan_ip() {
    let Some(ip) = lan_ipv4() else {
        eprintln!("[udp-only lan] no non-loopback IPv4; skipping");
        return;
    };
    run_case("udp-only lan", ip).await;
}
