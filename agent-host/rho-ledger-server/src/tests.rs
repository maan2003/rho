use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use rho_ledger::stream::{LedgerEvent, LedgerStreams};
use rho_ledger::{Ledger, LedgerKey};

use super::*;

struct Device {
    streams: Arc<LedgerStreams>,
    events: futures::channel::mpsc::UnboundedReceiver<LedgerEvent>,
    _dir: tempfile::TempDir,
}

async fn device(key: Option<LedgerKey>) -> Device {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(RhoDb::open(dir.path().join("client.redb"))).await;
    let (streams, events) = LedgerStreams::new(ledger);
    if let Some(key) = key {
        streams.set_key(key).await.unwrap();
    }
    Device {
        streams,
        events,
        _dir: dir,
    }
}

async fn host() -> (Arc<LedgerServer>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let server = LedgerServer::open(RhoDb::open(dir.path().join("rho.redb"))).await;
    (Arc::new(server), dir)
}

/// Connects a device to a host until the returned task is aborted.
fn connect(server: &Arc<LedgerServer>, device: &Device) -> tokio::task::JoinHandle<()> {
    let (client, host) = tokio::io::duplex(1 << 16);
    let server = Arc::clone(server);
    let streams = Arc::clone(&device.streams);
    tokio::spawn(async move {
        let (host_read, host_write) = tokio::io::split(host);
        let (client_read, client_write) = tokio::io::split(client);
        let _ = tokio::join!(
            server.serve(host_read, host_write),
            streams.speak(client_read, client_write),
        );
    })
}

async fn next(device: &mut Device) -> LedgerEvent {
    tokio::time::timeout(Duration::from_secs(5), device.events.next())
        .await
        .expect("an event in time")
        .unwrap()
}

fn put(key: &str, value: &str) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    vec![(key.as_bytes().to_vec(), Some(value.as_bytes().to_vec()))]
}

#[tokio::test]
async fn a_write_reaches_a_device_connected_to_the_same_host() {
    let key = LedgerKey::generate();
    let (server, _dir) = host().await;
    let laptop = device(Some(key)).await;
    let mut phone = device(Some(key)).await;
    let _laptop = connect(&server, &laptop);
    let _phone = connect(&server, &phone);
    tokio::time::sleep(Duration::from_millis(50)).await;
    laptop.streams.write(put("a", "1")).await;
    let LedgerEvent::Changed(changes) = next(&mut phone).await else {
        panic!("expected changes");
    };
    assert_eq!(changes[0].value.as_deref(), Some(&b"1"[..]));
}

#[tokio::test]
async fn a_device_that_was_away_hears_what_it_missed() {
    let key = LedgerKey::generate();
    let (server, _dir) = host().await;
    let laptop = device(Some(key)).await;
    let mut phone = device(Some(key)).await;
    // Written with no host at all: the first connection hands it over.
    laptop.streams.write(put("a", "1")).await;
    let laptop_link = connect(&server, &laptop);
    tokio::time::sleep(Duration::from_millis(50)).await;
    laptop_link.abort();
    let _phone = connect(&server, &phone);
    let LedgerEvent::Changed(_) = next(&mut phone).await else {
        panic!("expected changes");
    };
    assert_eq!(phone.streams.ledger().get(b"a").as_deref(), Some(&b"1"[..]));
}

#[tokio::test]
async fn the_host_holds_nothing_it_can_read() {
    let key = LedgerKey::generate();
    let (server, _dir) = host().await;
    let laptop = device(Some(key)).await;
    laptop.streams.write(put("secret", "plans")).await;
    let _laptop = connect(&server, &laptop);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let held = server.after(&BTreeMap::new());
    let sealed = &held[&laptop.streams.ledger().device()][0].sealed;
    assert!(!sealed.windows(5).any(|window| window == b"plans"));
}

#[tokio::test]
async fn a_base_replaces_what_the_host_held_before_it() {
    let (server, _dir) = host().await;
    let device = DeviceId([1; 16]);
    let segment = |seq, base| Segment {
        seq,
        base,
        sealed: vec![],
    };
    server.put(device, segment(1, false)).await;
    server.put(device, segment(2, false)).await;
    // Old or repeated segments are not taken.
    server.put(device, segment(2, true)).await;
    server.put(device, segment(3, true)).await;
    server.put(device, segment(4, false)).await;
    let seqs: Vec<_> = server.after(&BTreeMap::new())[&device]
        .iter()
        .map(|segment| segment.seq)
        .collect();
    assert_eq!(seqs, [3, 4]);
    assert_eq!(server.heads()[&device], 4);
}

#[tokio::test]
async fn a_device_without_the_key_is_told_it_needs_one() {
    let (server, _dir) = host().await;
    let laptop = device(Some(LedgerKey::generate())).await;
    let mut phone = device(None).await;
    laptop.streams.write(put("a", "1")).await;
    let _laptop = connect(&server, &laptop);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _phone = connect(&server, &phone);
    assert_eq!(next(&mut phone).await, LedgerEvent::NeedsKey);
}
