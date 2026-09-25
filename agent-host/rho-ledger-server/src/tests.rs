use std::sync::Arc;
use std::time::Duration;

use rho_ledger::notes::Arrived;
use rho_ledger::protocol::SlotId;
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
    // Here the greater note is the newer.
    let (streams, events) = LedgerStreams::new(ledger, Box::new(|theirs, mine| theirs > mine));
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
    let payload = vec![42; 64 * 1024 + 17];
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

async fn wait_for_note(device: &Device, plain: &[u8]) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !device
            .streams
            .ledger()
            .notes()
            .iter()
            .any(|note| note == plain)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("note synced in time");
}
#[tokio::test]
async fn a_note_is_one_sealed_slot_holding_its_latest_revision() {
    let secret = Secret::generate();
    let (server, _dir) = host().await;
    let laptop = device(Some(secret)).await;
    let mut phone = device(Some(secret)).await;
    let links = [connect(&server, &laptop), connect(&server, &phone)];
    laptop.streams.put_note([1; 16], b"buy milk".to_vec()).await;
    laptop
        .streams
        .put_note([1; 16], b"buy oat milk".to_vec())
        .await;
    wait_for_note(&phone, b"buy oat milk").await;
    assert_eq!(
        phone.streams.ledger().notes(),
        vec![b"buy oat milk".to_vec()]
    );
    assert!(matches!(
        next(&mut phone).await,
        LedgerEvent::Note(Arrived { replaced: None, .. })
    ));
    let read = server.db.read();
    let mut version = 0;
    let mut held = Vec::new();
    while let Some((_, next, blob)) = slots::next_after(&read, version) {
        version = next;
        held.push(blob);
    }
    assert_eq!(held.len(), 1, "the host keeps one blob per note");
    assert!(!held[0].windows(4).any(|window| window == b"milk"));
    for link in links {
        link.abort();
    }
}
#[tokio::test]
async fn notes_said_apart_meet_on_the_newer_and_reach_every_host() {
    let secret = Secret::generate();
    let (h1, _one) = host().await;
    let (h2, _two) = host().await;
    let laptop = device(Some(secret)).await;
    let mut phone = device(Some(secret)).await;
    let both = device(Some(secret)).await;
    // Written apart, each on its own host.
    laptop.streams.put_note([1; 16], b"a: older".to_vec()).await;
    phone.streams.put_note([1; 16], b"b: newer".to_vec()).await;
    let links = [
        connect(&h1, &laptop),
        connect(&h2, &phone),
        connect(&h1, &both),
        connect(&h2, &both),
    ];
    wait_for_note(&laptop, b"b: newer").await;
    wait_for_note(&both, b"b: newer").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(phone.streams.ledger().notes(), vec![b"b: newer".to_vec()]);
    let slots = |server: &LedgerServer| {
        let read = server.db.read();
        std::iter::successors(slots::next_after(&read, 0), |(_, version, _)| {
            slots::next_after(&read, *version)
        })
        .map(|(_, _, blob)| blob)
        .collect::<Vec<_>>()
    };
    assert_eq!(slots(&h1), slots(&h2), "both hosts hold the same blob");
    while let Ok(event) = phone.events.try_recv() {
        assert!(
            !matches!(event, LedgerEvent::Note(_)),
            "the newer note stays"
        );
    }
    for link in links {
        link.abort();
    }
}
#[tokio::test]
async fn a_put_that_names_another_blob_is_refused_with_what_is_held() {
    let (server, _dir) = host().await;
    let slot = SlotId([7; 16]);
    assert_eq!(server.put(slot, None, vec![1]).await, None);
    let held = Some(ServerFrame::Slot {
        slot,
        version: 1,
        blob: vec![1],
    });
    assert_eq!(server.put(slot, None, vec![2]).await, held);
    assert_eq!(server.put(slot, Some([0; 32]), vec![2]).await, held);
    assert_eq!(
        server.put(slot, Some(slots::hash(&[1])), vec![2]).await,
        None
    );
    assert_eq!(
        server.put(SlotId([8; 16]), Some([0; 32]), vec![3]).await,
        Some(ServerFrame::Slot {
            slot: SlotId([8; 16]),
            version: 0,
            blob: Vec::new(),
        })
    );
}

#[tokio::test]
async fn a_device_putting_many_notes_while_catching_up_keeps_its_stream() {
    let secret = Secret::generate();
    let (server, _dir) = host().await;
    let laptop = device(Some(secret)).await;
    let phone = device(Some(secret)).await;
    for note in 0..200u8 {
        laptop.streams.put_note([note; 16], vec![b'l'; 300]).await;
        let mut id = [note; 16];
        id[0] = 0xff;
        phone.streams.put_note(id, vec![b'p'; 300]).await;
    }
    let a = connect(&server, &laptop);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !laptop.streams.ledger().note_puts(server.store).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("laptop's notes put");
    // The phone puts its notes while the host streams it the laptop's.
    let b = connect(&server, &phone);
    tokio::time::timeout(Duration::from_secs(10), async {
        while phone.streams.ledger().notes().len() < 400
            || laptop.streams.ledger().notes().len() < 400
        {
            eprintln!(
                "progress phone {} laptop {} puts {}",
                phone.streams.ledger().notes().len(),
                laptop.streams.ledger().notes().len(),
                phone.streams.ledger().note_puts(server.store).len()
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("every note reaches both");
    assert!(!a.is_finished() && !b.is_finished(), "no stream broke");
    a.abort();
    b.abort();
}
