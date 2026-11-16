//! Ownership is an OS lease, independent of backend and socket probing.

mod common;

use iroh::endpoint::transports::CustomTransport;
use iroh::{SecretKey, TransportAddr};
use iroh_ipc_transport::{IPC_TRANSPORT_ID, IpcTransport, LocalDiscovery};

#[tokio::test]
async fn live_local_target_excludes_competing_addresses_before_connect() {
    let dir = common::TestDir::new().await;
    let discovery = LocalDiscovery::new(dir.path());
    let id = SecretKey::generate().public();
    let owner = IpcTransport::prepare(discovery.clone(), id).await.unwrap();
    let remote =
        iroh::EndpointAddr::from_parts(id, [TransportAddr::Ip("127.0.0.1:12345".parse().unwrap())]);
    let local = discovery.resolve_target(remote.clone()).await.unwrap();
    assert_eq!(local.id, id);
    assert_eq!(
        local.addrs,
        [TransportAddr::Custom(owner.local_addr())].into()
    );
    owner.close().await.unwrap();
    assert_eq!(
        discovery.resolve_target(remote.clone()).await.unwrap(),
        remote
    );
    dir.close().await;
}
use std::io;

#[tokio::test]
async fn competing_bind_preserves_live_owner_and_close_releases_identity() {
    let dir = common::TestDir::new().await;
    let discovery = LocalDiscovery::new(dir.path());
    let id = SecretKey::generate().public();
    let owner = IpcTransport::prepare(discovery.clone(), id).await.unwrap();
    let address = Some(TransportAddr::Custom(owner.local_addr()));
    assert_eq!(discovery.resolve(id).await.unwrap(), address);
    let contender = IpcTransport::prepare(discovery.clone(), id)
        .await
        .unwrap_err();
    assert_eq!(contender.kind(), io::ErrorKind::AddrInUse);
    assert_eq!(discovery.resolve(id).await.unwrap(), address);
    let endpoint = owner.bind().unwrap();
    assert_eq!(owner.bind().unwrap_err().kind(), io::ErrorKind::AddrInUse);
    owner.close().await.unwrap();
    assert_eq!(discovery.resolve(id).await.unwrap(), None);
    // Retaining the old iroh sender cannot retain a live OS resource after close.
    let _sender = endpoint.create_sender();
    let replacement = IpcTransport::prepare(discovery.clone(), id).await.unwrap();
    assert!(discovery.resolve(id).await.unwrap().is_some());
    owner.close().await.unwrap();
    assert!(
        discovery.resolve(id).await.unwrap().is_some(),
        "old close cannot remove new lease"
    );
    replacement.close().await.unwrap();
    dir.close().await;
}

#[tokio::test]
async fn unlocked_stale_records_are_not_discovered_and_can_be_reclaimed() {
    let dir = common::TestDir::new().await;
    let discovery = LocalDiscovery::new(dir.path());
    let id = SecretKey::generate().public();
    tokio::fs::write(
        dir.path().join(format!("{id}.ipc")),
        b"abandoned incomplete record",
    )
    .await
    .unwrap();
    assert_eq!(discovery.resolve(id).await.unwrap(), None);
    let transport = IpcTransport::prepare(discovery.clone(), id).await.unwrap();
    assert!(discovery.resolve(id).await.unwrap().is_some());
    transport.close().await.unwrap();
    dir.close().await;
}

#[tokio::test]
async fn sender_rejects_foreign_and_malformed_addresses() {
    let dir = common::TestDir::new().await;
    let transport = IpcTransport::prepare(
        LocalDiscovery::new(dir.path()),
        SecretKey::generate().public(),
    )
    .await
    .unwrap();
    let endpoint = transport.bind().unwrap();
    let sender = endpoint.create_sender();
    assert!(sender.is_valid_send_addr(&transport.local_addr()));
    for addr in [
        iroh_base::CustomAddr::from_parts(42, b"foreign"),
        iroh_base::CustomAddr::from_parts(IPC_TRANSPORT_ID, b""),
        iroh_base::CustomAddr::from_parts(IPC_TRANSPORT_ID, &[1, 0, 0]),
    ] {
        assert!(!sender.is_valid_send_addr(&addr));
    }
    transport.close().await.unwrap();
    dir.close().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bind_failure_preserves_an_unrelated_filesystem_entry() {
    let dir = common::TestDir::new().await;
    let id = SecretKey::generate().public();
    let path = dir.path().join(format!("{id}.sock"));
    tokio::fs::write(&path, b"unrelated file").await.unwrap();
    assert!(
        IpcTransport::prepare(LocalDiscovery::new(dir.path()), id)
            .await
            .is_err()
    );
    assert_eq!(tokio::fs::read(&path).await.unwrap(), b"unrelated file");
    dir.close().await;
}
