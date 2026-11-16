//! macOS must not create AF_UNIX sockets, including for discovery or cleanup.

#![cfg(target_os = "macos")]

mod common;

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::Ipv4Addr;
use std::time::Duration;

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_does_not_advertise_a_unix_socket_path() {
    use iroh::endpoint::transports::{CustomTransport, RecvInfo};
    use iroh_ipc_transport::{IpcTransport, LocalDiscovery};
    let dir = common::TestDir::new().await;
    let transport = IpcTransport::prepare(
        LocalDiscovery::new(dir.path()),
        iroh::SecretKey::generate().public(),
    )
    .await
    .unwrap();
    let address = transport.local_addr();
    assert_eq!(
        address.data().len(),
        3,
        "UDP address contains only a backend tag and port"
    );
    assert_eq!(address.data()[0], 1, "macOS must use the UDP backend");
    let port = u16::from_be_bytes([address.data()[1], address.data()[2]]);
    let mut endpoint = transport.bind().unwrap();
    let sender = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    sender
        .send_to(b"loopback proof", (Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let mut storage = [0; 64];
    let mut bufs = [IoSliceMut::new(&mut storage)];
    let mut metas = [noq_udp::RecvMeta::default()];
    let mut infos = [RecvInfo::new(address, None)];
    let count = tokio::time::timeout(
        Duration::from_secs(2),
        poll_fn(|cx| endpoint.poll_recv(cx, &mut bufs, &mut metas, &mut infos)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(&storage[..metas[0].len], b"loopback proof");
    let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        assert!(
            entry.file_type().await.unwrap().is_file(),
            "only regular discovery files on macOS"
        );
    }
    transport.close().await.unwrap();
    dir.close().await;
}
