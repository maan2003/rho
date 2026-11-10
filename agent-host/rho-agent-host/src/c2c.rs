//! C2C on the wire ([docs/c2c.md](../../../docs/c2c.md)): one CBOR map
//! on a stream of its own, answered on that stream. What the host does
//! with a message is the pool's
//! ([`rho_agent::host::pool::AgentPool::c2c_receive`]); this module only
//! carries.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures::future::BoxFuture;
use iroh::endpoint::Connection;
use rho_agent::host::pool::{AgentPool, Answer, Envelope, MAX_BODY_LEN, Transport};

/// The ALPN C2C is spoken on. A change old peers cannot read takes a new
/// number.
pub const ALPN: &[u8] = b"c2c/1";
/// How many harnesses may hold a connection at once. Nobody approved them,
/// so what they can make the host hold is bounded: this many connections,
/// each with as many streams as the endpoint allows, each one message.
pub const MAX_CONNECTIONS: usize = 64;
/// The longest message read: the longest body and room for the rest of
/// its map.
const MAX_MESSAGE_LEN: usize = MAX_BODY_LEN + 512;
const MAX_ANSWER_LEN: usize = 256;
/// How long a connection may carry nothing before the host closes it, so a
/// harness that went quiet does not keep one of the connections there are.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a peer has to say all of one message.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A message as its map. Fields this version does not know are ignored.
#[derive(serde::Serialize, serde::Deserialize)]
struct WireMessage {
    to: String,
    from: String,
    id: u64,
    body: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WireAnswer {
    answer: String,
}

fn encode<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).expect("a Vec takes every write");
    bytes
}

fn encode_message(envelope: &Envelope) -> Vec<u8> {
    encode(&WireMessage {
        to: envelope.to.to_string(),
        from: envelope.from.to_string(),
        id: envelope.id,
        body: envelope.body.clone(),
    })
}

fn decode_message(bytes: &[u8]) -> anyhow::Result<Envelope> {
    let WireMessage { to, from, id, body } =
        ciborium::from_reader(bytes).context("not a C2C message")?;
    anyhow::ensure!(body.len() <= MAX_BODY_LEN, "a C2C body is too long");
    Ok(Envelope {
        to: to.parse().map_err(anyhow::Error::msg)?,
        from: from.parse().map_err(anyhow::Error::msg)?,
        id,
        body,
    })
}

fn encode_answer(answer: Answer) -> Vec<u8> {
    encode(&WireAnswer {
        answer: match answer {
            Answer::Delivered => "delivered",
            Answer::Refused => "refused",
        }
        .to_owned(),
    })
}

fn decode_answer(bytes: &[u8]) -> anyhow::Result<Answer> {
    let WireAnswer { answer } = ciborium::from_reader(bytes).context("not a C2C answer")?;
    match answer.as_str() {
        "delivered" => Ok(Answer::Delivered),
        "refused" => Ok(Answer::Refused),
        other => anyhow::bail!("an answer this version does not know: {other:?}"),
    }
}

/// Answers every message another harness sends on `connection`, for as
/// long as it stays open. Whatever is not a message is refused like one
/// to nobody: a harness says no more than that.
pub async fn serve(pool: Arc<AgentPool>, connection: Connection) {
    answer_messages(connection, move |endpoint, envelope| {
        let pool = pool.clone();
        async move { pool.c2c_receive(endpoint, envelope).await }
    })
    .await
}

/// [`serve`], with what a message is given to left open.
async fn answer_messages<F, Fut>(connection: Connection, receive: F)
where
    F: Fn([u8; 32], Envelope) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Answer> + Send,
{
    let endpoint = *connection.remote_id().as_bytes();
    loop {
        let Ok(stream) = tokio::time::timeout(IDLE_TIMEOUT, connection.accept_bi()).await else {
            connection.close(0u32.into(), b"idle");
            return;
        };
        let Ok((mut send, mut recv)) = stream else {
            return;
        };
        let receive = receive.clone();
        tokio::spawn(async move {
            let Ok(read) =
                tokio::time::timeout(READ_TIMEOUT, recv.read_to_end(MAX_MESSAGE_LEN)).await
            else {
                return;
            };
            let answer = match read.map(|bytes| decode_message(&bytes)) {
                Ok(Ok(envelope)) => receive(endpoint, envelope).await,
                // Not a message, longer than one can be, or cut off.
                Ok(Err(_)) | Err(_) => Answer::Refused,
            };
            if send.write_all(&encode_answer(answer)).await.is_ok() {
                let _ = send.finish();
                // Hold the stream until the peer has the answer.
                let _ = send.stopped().await;
            }
        });
    }
}

/// Sends one message on `connection` and reads its answer.
async fn exchange(connection: &Connection, envelope: &Envelope) -> anyhow::Result<Answer> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&encode_message(envelope)).await?;
    send.finish()?;
    decode_answer(&recv.read_to_end(MAX_ANSWER_LEN).await?)
}

/// Carries messages over the host's own iroh endpoint, so a harness that
/// receives one sees this host's id. One connection to a harness carries
/// every message to it, until it closes: the other side closes one that
/// goes quiet.
pub struct IrohTransport {
    endpoint: iroh::Endpoint,
    connections: std::sync::Mutex<HashMap<[u8; 32], Connection>>,
}

impl IrohTransport {
    pub fn new(endpoint: iroh::Endpoint) -> Self {
        Self {
            endpoint,
            connections: std::sync::Mutex::default(),
        }
    }

    /// The connection held to `endpoint`, if it has not closed.
    fn held(&self, endpoint: [u8; 32]) -> Option<Connection> {
        let mut connections = self.connections.lock().expect("poison");
        let held = connections.remove(&endpoint)?;
        if held.close_reason().is_some() {
            return None;
        }
        connections.insert(endpoint, held.clone());
        Some(held)
    }

    async fn connect(&self, endpoint: [u8; 32]) -> anyhow::Result<Connection> {
        let id = iroh::EndpointId::from_bytes(&endpoint).context("not an endpoint id")?;
        let connection = self.endpoint.connect(id, ALPN).await?;
        self.connections
            .lock()
            .expect("poison")
            .insert(endpoint, connection.clone());
        Ok(connection)
    }
}

impl Transport for IrohTransport {
    fn send<'a>(
        &'a self,
        endpoint: [u8; 32],
        envelope: &'a Envelope,
    ) -> BoxFuture<'a, anyhow::Result<Answer>> {
        Box::pin(async move {
            if let Some(connection) = self.held(endpoint) {
                // A held connection may have closed on the way; a message
                // sent twice arrives once, so it is tried on a new one.
                if let Ok(answer) = exchange(&connection, envelope).await {
                    return Ok(answer);
                }
                connection.close(0u32.into(), b"no answer");
            }
            exchange(&self.connect(endpoint).await?, envelope).await
        })
    }
}

#[cfg(test)]
mod tests {
    use ciborium::Value;

    use super::*;

    fn envelope(body: &str) -> Envelope {
        Envelope {
            to: "to-1".parse().unwrap(),
            from: "from.2".parse().unwrap(),
            id: 7,
            body: body.to_owned(),
        }
    }

    fn map(entries: &[(&str, Value)]) -> Vec<u8> {
        encode(&Value::Map(
            entries
                .iter()
                .map(|(key, value)| (Value::Text((*key).to_owned()), value.clone()))
                .collect(),
        ))
    }

    /// The map is the one docs/c2c.md gives, whatever the Rust types are
    /// called.
    #[test]
    fn a_message_is_the_map_the_protocol_names() {
        let value: Value = ciborium::from_reader(&encode_message(&envelope("hi"))[..]).unwrap();
        assert_eq!(
            value,
            Value::Map(vec![
                (Value::Text("to".into()), Value::Text("to-1".into())),
                (Value::Text("from".into()), Value::Text("from.2".into())),
                (Value::Text("id".into()), Value::Integer(7.into())),
                (Value::Text("body".into()), Value::Text("hi".into())),
            ])
        );
        let answer: Value = ciborium::from_reader(&encode_answer(Answer::Refused)[..]).unwrap();
        assert_eq!(
            answer,
            Value::Map(vec![(
                Value::Text("answer".into()),
                Value::Text("refused".into())
            )])
        );
    }

    #[test]
    fn a_message_is_read_strictly_but_for_fields_it_does_not_know() {
        let fields = [
            ("to", Value::Text("to-1".into())),
            ("from", Value::Text("from.2".into())),
            ("id", Value::Integer(7.into())),
            ("body", Value::Text("hi".into())),
        ];
        let mut later = fields.to_vec();
        later.push(("thread", Value::Integer(3.into())));
        assert_eq!(decode_message(&map(&later)).unwrap(), envelope("hi"));

        for missing in 0..fields.len() {
            let mut rest = fields.to_vec();
            rest.remove(missing);
            assert!(decode_message(&map(&rest)).is_err(), "without {missing}");
        }
        let mut bad_local = fields.to_vec();
        bad_local[0].1 = Value::Text("To/1".into());
        assert!(decode_message(&map(&bad_local)).is_err());
        let mut negative = fields.to_vec();
        negative[2].1 = Value::Integer((-1).into());
        assert!(decode_message(&map(&negative)).is_err());

        let longest = "x".repeat(MAX_BODY_LEN);
        assert_eq!(
            decode_message(&encode_message(&envelope(&longest))).unwrap(),
            envelope(&longest)
        );
        assert!(encode_message(&envelope(&longest)).len() <= MAX_MESSAGE_LEN);
        assert!(decode_message(&encode_message(&envelope(&(longest + "x")))).is_err());
        assert!(decode_message(b"not cbor").is_err());
    }

    #[test]
    fn an_answer_this_version_does_not_know_is_an_error() {
        for answer in [Answer::Delivered, Answer::Refused] {
            assert_eq!(decode_answer(&encode_answer(answer)).unwrap(), answer);
        }
        let later = map(&[("answer", Value::Text("later".into()))]);
        assert!(decode_answer(&later).is_err());
    }

    /// Two endpoints: what one sends reaches the other with the sender's
    /// endpoint id, and its answer comes back, on one connection for as
    /// long as it lasts; what is not a message is refused without reaching
    /// anything.
    #[tokio::test]
    async fn a_message_crosses_between_two_endpoints() -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let server = iroh::Endpoint::builder(iroh::endpoint::presets::N0DisableRelay)
                .alpns(vec![ALPN.to_vec()])
                .bind()
                .await?;
            let lookup = iroh::address_lookup::memory::MemoryLookup::new();
            let client = iroh::Endpoint::builder(iroh::endpoint::presets::N0DisableRelay)
                .address_lookup(lookup.clone())
                .bind()
                .await?;
            lookup.add_endpoint_info(server.addr());
            let transport = IrohTransport::new(client.clone());
            let to = *server.id().as_bytes();

            let (seen, mut received) = tokio::sync::mpsc::unbounded_channel();
            let (connected, mut connections) = tokio::sync::mpsc::unbounded_channel();
            let serving = tokio::spawn({
                let server = server.clone();
                async move {
                    while let Some(incoming) = server.accept().await {
                        let Ok(connection) = incoming.await else {
                            continue;
                        };
                        connected.send(connection.clone()).unwrap();
                        let seen = seen.clone();
                        tokio::spawn(answer_messages(connection, move |endpoint, envelope| {
                            let answer = if envelope.body == "knock" {
                                Answer::Refused
                            } else {
                                Answer::Delivered
                            };
                            seen.send((endpoint, envelope)).unwrap();
                            async move { answer }
                        }));
                    }
                }
            });

            assert_eq!(
                transport.send(to, &envelope("hello")).await?,
                Answer::Delivered
            );
            assert_eq!(
                received.recv().await.unwrap(),
                (*client.id().as_bytes(), envelope("hello"))
            );
            assert_eq!(
                transport.send(to, &envelope("knock")).await?,
                Answer::Refused
            );
            received.recv().await.unwrap();
            let first = connections.recv().await.unwrap();
            assert!(connections.try_recv().is_err(), "one connection for both");

            let (mut send, mut recv) = transport.held(to).unwrap().open_bi().await?;
            send.write_all(b"not a message").await?;
            send.finish()?;
            assert_eq!(
                decode_answer(&recv.read_to_end(MAX_ANSWER_LEN).await?)?,
                Answer::Refused
            );
            assert!(received.try_recv().is_err());

            let (mut send, mut recv) = transport.held(to).unwrap().open_bi().await?;
            send.write_all(&vec![b'x'; MAX_MESSAGE_LEN + 1]).await?;
            send.finish()?;
            assert_eq!(
                decode_answer(&recv.read_to_end(MAX_ANSWER_LEN).await?)?,
                Answer::Refused,
                "too long to be a message"
            );
            assert!(received.try_recv().is_err());

            // The other side closed the connection: the next message takes a
            // new one, and nobody hears of it.
            first.close(0u32.into(), b"idle");
            assert_eq!(
                transport.send(to, &envelope("again")).await?,
                Answer::Delivered
            );
            assert_eq!(received.recv().await.unwrap().1, envelope("again"));
            connections.recv().await.unwrap();

            serving.abort();
            Ok(())
        })
        .await?
    }
}
