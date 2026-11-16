//! Rendezvous directory creation sets its mode explicitly, never via umask.
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;

use iroh::SecretKey;
use iroh_ipc_transport::{IpcTransport, LocalDiscovery};

async fn mode_after_bind(discovery: LocalDiscovery) -> u32 {
    let id = SecretKey::generate().public();
    let owner = IpcTransport::prepare(discovery.clone(), id).await.unwrap();
    owner.close().await.unwrap();
    let root = discovery.root().to_path_buf();
    tokio::fs::metadata(root)
        .await
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

#[tokio::test]
async fn shared_root_is_created_world_writable_and_sticky() {
    let dir = common::TestDir::new().await;
    let discovery = LocalDiscovery::shared(dir.path().join("shared"));
    assert_eq!(mode_after_bind(discovery).await, 0o1777);
    dir.close().await;
}

#[tokio::test]
async fn private_root_is_created_owner_only() {
    let dir = common::TestDir::new().await;
    let discovery = LocalDiscovery::new(dir.path().join("private"));
    assert_eq!(mode_after_bind(discovery).await, 0o700);
    dir.close().await;
}

#[tokio::test]
async fn existing_root_mode_is_never_changed() {
    let dir = common::TestDir::new().await;
    let root = dir.path().join("existing");
    tokio::fs::create_dir(&root).await.unwrap();
    tokio::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750))
        .await
        .unwrap();
    assert_eq!(mode_after_bind(LocalDiscovery::shared(&root)).await, 0o750);
    dir.close().await;
}
