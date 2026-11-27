use std::sync::Arc;
use std::time::Duration;

use rho_ledger::stream::{LedgerEvent, LedgerStreams};
use rho_ledger::{Channel, Ledger, Pairing, Secret};

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
    // A device that already holds the phrase, as one paired earlier does.
    if let Some(secret) = secret {
        streams.ledger().set_secret(secret).await.unwrap();
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

#[tokio::test]
async fn an_empty_first_host_makes_no_phrase() {
    let (server, _dir) = host().await;
    let mut first = device(None).await;
    first
        .streams
        .append(Channel::Facts, vec![b"before pairing".to_vec()])
        .await;
    assert!(
        matches!(next(&mut first).await, LedgerEvent::Appended(_)),
        "its own write"
    );
    let a = connect(&server, &first);
    // Long enough for the hello and the answer to have gone both ways.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        first.events.try_recv().is_err(),
        "no event: nothing was made"
    );
    assert_eq!(first.streams.ledger().pairing(), Pairing::Unasked);
    assert!(first.streams.ledger().secret().is_none());
    assert!(server.lengths().is_empty(), "what it said stays on it");
    a.abort();
}

#[tokio::test]
async fn a_device_paired_later_brings_what_it_said_before() {
    let (server, _dir) = host().await;
    let first = device(None).await;
    let secret = first.streams.make_secret().await.unwrap();
    assert_eq!(first.streams.ledger().pairing(), Pairing::Paired);
    let a = connect(&server, &first);
    // It has said nothing yet, and the host holds its log all the same.
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.lengths().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("announced in time");
    first
        .streams
        .append(Channel::Facts, vec![b"from the first".to_vec()])
        .await;

    let mut later = device(None).await;
    later.streams.ledger().decline().await;
    later
        .streams
        .append(Channel::Facts, vec![b"verdict before pairing".to_vec()])
        .await;
    later
        .streams
        .append(Channel::Notes, vec![b"note before pairing".to_vec()])
        .await;
    let b = connect(&server, &later);
    for _ in 0..2 {
        assert!(
            matches!(next(&mut later).await, LedgerEvent::Appended(_)),
            "its own writes"
        );
    }
    assert_eq!(next(&mut later).await, LedgerEvent::NeedsKey);
    assert_eq!(later.streams.ledger().pairing(), Pairing::Unpaired);

    // A new phrase would split the user's devices in two.
    let fresh = later.streams.make_secret().await;
    assert_eq!(
        fresh.unwrap_err().to_string(),
        "your other devices hold a phrase already: type it"
    );
    // Real words with a good checksum, but not the phrase.
    let wrong = later.streams.set_secret(Secret::generate()).await;
    assert_eq!(
        wrong.unwrap_err().to_string(),
        "this phrase does not open what your other devices wrote"
    );
    assert_eq!(later.streams.ledger().pairing(), Pairing::Unpaired);

    later.streams.set_secret(secret).await.unwrap();
    assert_eq!(later.streams.ledger().pairing(), Pairing::Paired);
    let said = |device: &Device, channel| {
        let mut said: Vec<Vec<u8>> = device
            .streams
            .ledger()
            .items(channel)
            .into_iter()
            .map(|item| item.bytes)
            .collect();
        said.sort();
        said
    };
    let facts = vec![
        b"from the first".to_vec(),
        b"verdict before pairing".to_vec(),
    ];
    let notes = vec![b"note before pairing".to_vec()];
    assert_eq!(said(&later, Channel::Facts), facts);
    assert_eq!(said(&later, Channel::Notes), notes);
    wait_for(&first, Channel::Facts, 2).await;
    wait_for(&first, Channel::Notes, 1).await;
    assert_eq!(said(&first, Channel::Facts), facts);
    assert_eq!(said(&first, Channel::Notes), notes);
    a.abort();
    b.abort();
}

/// A phrase typed before any host answered is held, sealing nothing: a
/// typo that opens nothing the first host holds is refused then, and the
/// device never wrote a log under it. The right phrase is kept when the
/// host answers, and what was said before reaches the other device.
#[tokio::test]
async fn a_phrase_typed_before_any_host_answers_waits_for_its_ledger() {
    let secret = Secret::generate();
    let (server, _dir) = host().await;
    let first = device(Some(secret)).await;
    first
        .streams
        .append(Channel::Facts, vec![b"from the first".to_vec()])
        .await;
    let a = connect(&server, &first);
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.lengths().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("announced in time");

    let mut typo = device(None).await;
    typo.streams
        .append(Channel::Facts, vec![b"said before".to_vec()])
        .await;
    assert!(matches!(next(&mut typo).await, LedgerEvent::Appended(_)));
    let wrong = Secret::generate();
    assert_eq!(
        typo.streams.set_secret(wrong).await.unwrap(),
        Pairing::Unverified
    );
    assert!(typo.streams.ledger().secret().is_none());
    assert!(typo.streams.ledger().lengths().is_empty(), "nothing sealed");
    let b = connect(&server, &typo);
    assert!(
        next(&mut typo).await == LedgerEvent::PhraseRefused(wrong),
        "the first answer refuses it"
    );
    assert_eq!(typo.streams.ledger().pairing(), Pairing::Unasked);
    let firsts: Vec<_> = first.streams.ledger().lengths().into_keys().collect();
    let held = |lengths: std::collections::BTreeMap<_, _>| lengths.into_keys().collect::<Vec<_>>();
    assert_eq!(
        held(typo.streams.ledger().lengths()),
        firsts,
        "it sealed nothing"
    );
    assert_eq!(held(server.lengths()), firsts, "no second ledger");
    b.abort();

    let mut right = device(None).await;
    right
        .streams
        .append(Channel::Facts, vec![b"said before".to_vec()])
        .await;
    assert!(matches!(next(&mut right).await, LedgerEvent::Appended(_)));
    assert_eq!(
        right.streams.set_secret(secret).await.unwrap(),
        Pairing::Unverified
    );
    let c = connect(&server, &right);
    let mut events = Vec::new();
    while !events.contains(&LedgerEvent::Paired) {
        events.push(next(&mut right).await);
    }
    assert!(!events.contains(&LedgerEvent::NeedsKey), "{events:?}");
    assert_eq!(right.streams.ledger().pairing(), Pairing::Paired);
    wait_for(&right, Channel::Facts, 2).await;
    wait_for(&first, Channel::Facts, 2).await;
    a.abort();
    c.abort();
}
