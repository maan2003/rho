//! Prove leases are process-owned and recover after an ungraceful exit.

mod common;

use std::time::Duration;

use iroh::SecretKey;
use iroh_ipc_transport::{IpcTransport, LocalDiscovery};

#[tokio::test]
async fn lease_child() {
    let Some(root) = std::env::var_os("IPC_LEASE_TEST_DIRECTORY") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let _transport = IpcTransport::prepare(
        LocalDiscovery::new(&root),
        SecretKey::from([42; 32]).public(),
    )
    .await
    .unwrap();
    tokio::fs::write(root.join("ready"), b"ready")
        .await
        .unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn killed_owner_is_undiscoverable_and_identity_can_restart() {
    let dir = common::TestDir::new().await;
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_child", "--nocapture"])
        .env("IPC_LEASE_TEST_DIRECTORY", dir.path())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !tokio::fs::try_exists(dir.path().join("ready"))
            .await
            .unwrap()
        {
            assert!(
                child.try_wait().unwrap().is_none(),
                "lease child exited before ready"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child published lease");
    let id = SecretKey::from([42; 32]).public();
    let discovery = LocalDiscovery::new(dir.path());
    assert!(discovery.resolve(id).await.unwrap().is_some());
    let killed = discovery.watch(id).await.unwrap().expect("the child owns the id");
    assert!(killed.lives().await.unwrap());
    assert_eq!(
        IpcTransport::prepare(discovery.clone(), id)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    assert!(discovery.resolve(id).await.unwrap().is_none());
    assert!(!killed.lives().await.unwrap(), "the killed owner is gone");
    assert!(discovery.watch(id).await.unwrap().is_none());
    // No sweep: the successor finds the killed owner's lease file where it
    // left it, and still holds a lease of its own.
    let restarted = IpcTransport::prepare(discovery.clone(), id).await.unwrap();
    assert!(discovery.resolve(id).await.unwrap().is_some());
    assert!(
        !killed.lives().await.unwrap(),
        "the successor is not the owner watched"
    );
    let successor = discovery.watch(id).await.unwrap().expect("the successor owns the id");
    assert!(successor.lives().await.unwrap());
    restarted.close().await.unwrap();
    assert!(!successor.lives().await.unwrap(), "a closed owner is gone");
    dir.close().await;
}
