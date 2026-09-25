use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use rho_ledger::stream::{LedgerEvent, LedgerStreams};
use rho_ledger::{Channel, Ledger, Secret};

use super::*;

struct Device {
    streams: Arc<LedgerStreams>,
    events: futures::channel::mpsc::UnboundedReceiver<LedgerEvent>,
    _dir: tempfile::TempDir,
}
async fn device(secret: Option<Secret>) -> Device {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(RhoDb::open(dir.path().join("client.redb"))).await;
    let (streams, events) = LedgerStreams::new(ledger);
    if let Some(secret) = secret {
        streams.set_secret(secret).await.unwrap();
    }
    Device {
        streams,
        events,
        _dir: dir,
    }
}
async fn host() -> (Arc<LedgerServer>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (
        Arc::new(LedgerServer::open(RhoDb::open(dir.path().join("host.redb"))).await),
        dir,
    )
}
fn connect(server: &Arc<LedgerServer>, device: &Device) -> tokio::task::JoinHandle<()> {
    let (client, host) = tokio::io::duplex(1 << 20);
    let server = Arc::clone(server);
    let streams = Arc::clone(&device.streams);
    tokio::spawn(async move {
        let (host_read, host_write) = tokio::io::split(host);
        let (client_read, client_write) = tokio::io::split(client);
        let (server, client) = tokio::join!(
            server.serve(host_read, host_write),
            streams.speak(client_read, client_write)
        );
        if !server
            .as_ref()
            .is_err_and(|error| error.to_string().contains("early eof"))
        {
            assert!(server.is_ok(), "server: {server:?}");
        }
        if !client
            .as_ref()
            .is_err_and(|error| error.to_string().contains("early eof"))
        {
            assert!(client.is_ok(), "client: {client:?}");
        }
    })
}
async fn next(device: &mut Device) -> LedgerEvent {
    tokio::time::timeout(Duration::from_secs(5), device.events.next())
        .await
        .expect("event in time")
        .unwrap()
}
async fn wait_for(device: &Device, channel: Channel, count: usize) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while device.streams.ledger().items(channel).len() < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("synced in time");
}
#[tokio::test]
async fn two_devices_sync_and_host_stores_only_ciphertext() {
    let secret = Secret::generate();
    let (server, _dir) = host().await;
    let laptop = device(Some(secret)).await;
    let mut phone = device(Some(secret)).await;
    let a = connect(&server, &laptop);
    let b = connect(&server, &phone);
    laptop
        .streams
        .append(Channel::Facts, vec![b"plans".to_vec(), b"second".to_vec()])
        .await;
    assert!(
        matches!(next(&mut phone).await, LedgerEvent::Appended(items) if items.iter().map(|i| &i.bytes).collect::<Vec<_>>() == vec![b"plans".as_slice(), b"second".as_slice()])
    );
    assert_eq!(phone.streams.ledger().items(Channel::Facts).len(), 2);
    assert!(phone.streams.ledger().items(Channel::Notes).is_empty());
    let log = *server.lengths().keys().next().unwrap();
    assert!(
        !rho_ledger::store::read(&server.db.read(), log, 0, usize::MAX)
            .windows(5)
            .any(|window| window == b"plans")
    );
    assert_eq!(
        server.lengths()[&log],
        laptop.streams.ledger().lengths()[&log]
    );
    a.abort();
    b.abort();
}
#[tokio::test]
async fn forward_between_two_hosts() {
    let secret = Secret::generate();
    let (h1, _one) = host().await;
    let (h2, _two) = host().await;
    let a = device(Some(secret)).await;
    let b = device(Some(secret)).await;
    let c = device(Some(secret)).await;
    a.streams
        .append(Channel::Facts, vec![b"from A".to_vec()])
        .await;
    b.streams
        .append(Channel::Notes, vec![b"from B".to_vec()])
        .await;
    let links = [
        connect(&h1, &a),
        connect(&h2, &b),
        connect(&h1, &c),
        connect(&h2, &c),
    ];
    wait_for(&c, Channel::Facts, 1).await;
    wait_for(&c, Channel::Notes, 1).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while h1.lengths().len() != 2 || h2.lengths().len() != 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("forwarded in time");
    wait_for(&a, Channel::Notes, 1).await;
    wait_for(&b, Channel::Facts, 1).await;
    assert_eq!(a.streams.ledger().items(Channel::Notes)[0].bytes, b"from B");
    assert_eq!(b.streams.ledger().items(Channel::Facts)[0].bytes, b"from A");
    for link in links {
        link.abort();
    }
}
#[tokio::test]
async fn conditional_append_ignores_retry_and_wrong_offset() {
    let (server, _dir) = host().await;
    let log = LogId([4; 16]);
    server.append(log, 0, vec![1, 2]).await;
    server.append(log, 0, vec![9]).await;
    server.append(log, 4, vec![9]).await;
    server.append(log, 2, vec![3]).await;
    assert_eq!(
        rho_ledger::store::read(&server.db.read(), log, 0, usize::MAX),
        vec![1, 2, 3]
    );
}
#[tokio::test]
async fn holds_records_until_key_arrives() {
    let secret = Secret::generate();
    let (server, _dir) = host().await;
    let writer = device(Some(secret)).await;
    let mut reader = device(None).await;
    writer
        .streams
        .append(Channel::Facts, vec![b"pending".to_vec()])
        .await;
    let a = connect(&server, &writer);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let b = connect(&server, &reader);
    assert_eq!(next(&mut reader).await, LedgerEvent::NeedsKey);
    assert!(reader.streams.ledger().items(Channel::Facts).is_empty());
    reader.streams.set_secret(secret).await.unwrap();
    assert!(
        matches!(next(&mut reader).await, LedgerEvent::Appended(items) if items[0].bytes == b"pending")
    );
    a.abort();
    b.abort();
}

#[tokio::test]
async fn a_record_larger_than_one_wire_chunk_is_read_once_complete() {
    let secret = Secret::generate();
    let (server, _dir) = host().await;
    let writer = device(Some(secret)).await;
    let reader = device(Some(secret)).await;
    let payload = vec![42; 2 * 1024 * 1024 + 17];
    writer
        .streams
        .append(Channel::Notes, vec![payload.clone()])
        .await;
    let a = connect(&server, &writer);
    let b = connect(&server, &reader);
    wait_for(&reader, Channel::Notes, 1).await;
    assert_eq!(
        reader.streams.ledger().items(Channel::Notes)[0].bytes,
        payload
    );
    a.abort();
    b.abort();
}
