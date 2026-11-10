//! GCCP on the wire ([docs/gccp.md](../../../docs/gccp.md)): a stream says
//! its kind first. A message stream then carries one CBOR map and brings
//! back its result. A byte stream carries a header that names a service,
//! brings back its result, and after a `true` belongs to the two ends.
//! What the host does with either is the pool's
//! ([`rho_agent::host::pool::AgentPool::gccp_receive`] and
//! [`rho_agent::host::pool::AgentPool::gccp_join`]); this module only
//! carries.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Context as _;
use futures::future::BoxFuture;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use rho_agent::host::pool::{
    AgentPool, ByteStream, Envelope, MAX_BODY_LEN, MessageResult, StreamOpen, Transport,
};
use rho_agent_types::EndpointId;

/// The ALPN GCCP is spoken on. A change old peers cannot read takes a new
/// number.
pub const ALPN: &[u8] = b"gccp/1";
/// The kind of a stream that carries one message and its result. A stream
/// opens with its kind, so that the kinds share the connection; how a
/// stream's payload ends is its kind's business.
const KIND_MESSAGE: u16 = 1;
/// The kind of a byte stream to a service.
const KIND_STREAM: u16 = 2;
/// The code a stream of a kind this host does not know is reset with.
const UNKNOWN_KIND: u32 = 1;
/// The longest message read: the longest body and room for the rest of
/// its map.
const MAX_MESSAGE_LEN: usize = MAX_BODY_LEN + 512;
const MAX_RESULT_LEN: usize = 256;
/// The longest header of a byte stream read: three short names.
const MAX_OPEN_LEN: usize = 512;
/// The code a byte stream that broke is reset with. The protocol names
/// none: past the result a stream is its two ends' own.
const BROKEN: u32 = 0;
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
struct WireResult {
    success: bool,
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
        ciborium::from_reader(bytes).context("not a GCCP message")?;
    anyhow::ensure!(!body.trim().is_empty(), "a GCCP body is blank");
    anyhow::ensure!(body.len() <= MAX_BODY_LEN, "a GCCP body is too long");
    Ok(Envelope {
        to: to.parse().map_err(anyhow::Error::msg)?,
        from: from.parse().map_err(anyhow::Error::msg)?,
        id,
        body,
    })
}

/// The header of a byte stream as its map. Fields this version does not
/// know are ignored.
#[derive(serde::Serialize, serde::Deserialize)]
struct WireOpen {
    to: String,
    from: String,
    service: String,
}

fn encode_open(open: &StreamOpen) -> Vec<u8> {
    encode(&WireOpen {
        to: open.to.to_string(),
        from: open.from.to_string(),
        service: open.service.to_string(),
    })
}

fn decode_open(bytes: &[u8]) -> anyhow::Result<StreamOpen> {
    let WireOpen { to, from, service } =
        ciborium::from_reader(bytes).context("not a GCCP stream header")?;
    Ok(StreamOpen {
        to: to.parse().map_err(anyhow::Error::msg)?,
        from: from.parse().map_err(anyhow::Error::msg)?,
        service: service.parse().map_err(anyhow::Error::msg)?,
    })
}

/// Writes a map behind its length. A byte stream goes on after its maps,
/// so the end of the stream cannot end them as it ends a message.
async fn write_framed(send: &mut SendStream, map: &[u8]) -> anyhow::Result<()> {
    let len = u16::try_from(map.len()).context("a map longer than its length can say")?;
    send.write_all(&[&len.to_be_bytes()[..], map].concat())
        .await?;
    Ok(())
}

/// Reads a map of at most `max` bytes from behind its length.
async fn read_framed(recv: &mut RecvStream, max: usize) -> anyhow::Result<Vec<u8>> {
    let mut len = [0; 2];
    recv.read_exact(&mut len).await?;
    let len = usize::from(u16::from_be_bytes(len));
    anyhow::ensure!(len <= max, "a map of {len} bytes where at most {max} fit");
    let mut map = vec![0; len];
    recv.read_exact(&mut map).await?;
    Ok(map)
}

/// The sending half of a byte stream. One that is dropped before it was
/// shut down resets the stream, so that the other harness sees a stream
/// that broke, and not bytes that ended.
struct Sending {
    stream: SendStream,
    ended: bool,
}

impl tokio::io::AsyncWrite for Sending {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        tokio::io::AsyncWrite::poll_write(Pin::new(&mut self.stream), cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_flush(Pin::new(&mut self.stream), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let ended = tokio::io::AsyncWrite::poll_shutdown(Pin::new(&mut self.stream), cx);
        self.ended |= matches!(ended, Poll::Ready(Ok(())));
        ended
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.stream.reset(BROKEN.into());
        }
    }
}

fn byte_stream(send: SendStream, recv: RecvStream) -> ByteStream {
    ByteStream {
        reader: Box::new(recv),
        writer: Box::new(Sending {
            stream: send,
            ended: false,
        }),
    }
}

fn encode_result(result: MessageResult) -> Vec<u8> {
    encode(&WireResult {
        success: result == MessageResult::Delivered,
    })
}

fn decode_result(bytes: &[u8]) -> anyhow::Result<MessageResult> {
    let WireResult { success } = ciborium::from_reader(bytes).context("not a GCCP result")?;
    Ok(if success {
        MessageResult::Delivered
    } else {
        MessageResult::Refused
    })
}

/// What takes in what another harness sends: the pool, or a test.
trait Inbox: Clone + Send + Sync + 'static {
    /// A message: its result.
    fn message(&self, endpoint: EndpointId, envelope: Envelope) -> BoxFuture<'_, MessageResult>;

    /// A byte stream: what carries it, if it is joined to its service.
    fn join(&self, endpoint: EndpointId, open: StreamOpen) -> BoxFuture<'_, Option<Carry>>;
}

/// Carries a byte stream that was joined, to its end.
type Carry = Box<dyn FnOnce(ByteStream) -> BoxFuture<'static, ()> + Send>;

impl Inbox for Arc<AgentPool> {
    fn message(&self, endpoint: EndpointId, envelope: Envelope) -> BoxFuture<'_, MessageResult> {
        Box::pin(self.gccp_receive(endpoint, envelope))
    }

    fn join(&self, endpoint: EndpointId, open: StreamOpen) -> BoxFuture<'_, Option<Carry>> {
        Box::pin(async move {
            let joined = self.gccp_join(endpoint, open).await?;
            Some(
                Box::new(move |stream| Box::pin(joined.carry(stream)) as BoxFuture<'static, ()>)
                    as Carry,
            )
        })
    }
}

/// Serves every stream the approved harness on `connection` opens, for as
/// long as the connection stays open: a message gets its result, and a
/// byte stream its result and then, if that is `true`, its service.
/// Whatever is neither is refused like a message to nobody: a harness says
/// no more than that.
pub async fn serve(pool: Arc<AgentPool>, connection: Connection) {
    serve_streams(pool, connection).await
}

/// [`serve`], with what takes the streams in left open.
async fn serve_streams(inbox: impl Inbox, connection: Connection) {
    let endpoint = EndpointId(*connection.remote_id().as_bytes());
    while let Ok((send, recv)) = connection.accept_bi().await {
        tokio::spawn(serve_stream(inbox.clone(), endpoint, send, recv));
    }
}

/// What a stream opened with. `None` is one whose opening could not be
/// read: not what its kind carries, longer than that can be, or cut off.
enum Opening {
    Message(Option<Envelope>),
    Stream(Option<StreamOpen>),
    /// A kind from a later version.
    Unknown,
}

async fn serve_stream(
    inbox: impl Inbox,
    endpoint: EndpointId,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let opening = async {
        let mut kind = [0; 2];
        recv.read_exact(&mut kind).await.ok()?;
        Some(match u16::from_be_bytes(kind) {
            KIND_MESSAGE => {
                let read = recv.read_to_end(MAX_MESSAGE_LEN).await.ok();
                Opening::Message(read.and_then(|bytes| decode_message(&bytes).ok()))
            }
            KIND_STREAM => {
                let read = read_framed(&mut recv, MAX_OPEN_LEN).await.ok();
                Opening::Stream(read.and_then(|bytes| decode_open(&bytes).ok()))
            }
            _ => Opening::Unknown,
        })
    };
    let Ok(Some(opening)) = tokio::time::timeout(READ_TIMEOUT, opening).await else {
        return;
    };
    match opening {
        Opening::Unknown => {
            // Only this stream ends.
            let _ = recv.stop(UNKNOWN_KIND.into());
            let _ = send.reset(UNKNOWN_KIND.into());
        }
        Opening::Message(envelope) => {
            let result = match envelope {
                Some(envelope) => inbox.message(endpoint, envelope).await,
                None => MessageResult::Refused,
            };
            if send.write_all(&encode_result(result)).await.is_ok() {
                let _ = send.finish();
                // Hold the stream until the peer has the result.
                let _ = send.stopped().await;
            }
        }
        Opening::Stream(open) => {
            let carry = match open {
                Some(open) => inbox.join(endpoint, open).await,
                None => None,
            };
            let result = encode(&WireResult {
                success: carry.is_some(),
            });
            if write_framed(&mut send, &result).await.is_err() {
                return;
            }
            match carry {
                Some(carry) => carry(byte_stream(send, recv)).await,
                None => {
                    let _ = send.finish();
                    let _ = send.stopped().await;
                }
            }
        }
    }
}

/// Sends one message on `connection` and reads its result.
async fn exchange(connection: &Connection, envelope: &Envelope) -> anyhow::Result<MessageResult> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&KIND_MESSAGE.to_be_bytes()).await?;
    send.write_all(&encode_message(envelope)).await?;
    send.finish()?;
    decode_result(&recv.read_to_end(MAX_RESULT_LEN).await?)
}

/// Opens a byte stream on `connection` and reads its result: the stream if
/// the other harness joined it, and `None` if it refused. Nothing but the
/// header is written before the result.
async fn open_stream(
    connection: &Connection,
    open: &StreamOpen,
) -> anyhow::Result<Option<ByteStream>> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&KIND_STREAM.to_be_bytes()).await?;
    write_framed(&mut send, &encode_open(open)).await?;
    let result = read_framed(&mut recv, MAX_RESULT_LEN).await?;
    let WireResult { success } = ciborium::from_reader(&result[..]).context("not a GCCP result")?;
    Ok(success.then(|| byte_stream(send, recv)))
}

/// Carries messages and byte streams over the host's own iroh endpoint, so
/// a harness that receives one sees this host's id. One connection to a
/// harness carries everything to it, until it closes.
pub struct IrohTransport {
    endpoint: iroh::Endpoint,
    connections: std::sync::Mutex<HashMap<EndpointId, Connection>>,
}

impl IrohTransport {
    pub fn new(endpoint: iroh::Endpoint) -> Self {
        Self {
            endpoint,
            connections: std::sync::Mutex::default(),
        }
    }

    /// The connection held to `endpoint`, if it has not closed.
    fn held(&self, endpoint: EndpointId) -> Option<Connection> {
        let mut connections = self.connections.lock().expect("poison");
        let held = connections.remove(&endpoint)?;
        if held.close_reason().is_some() {
            return None;
        }
        connections.insert(endpoint, held.clone());
        Some(held)
    }

    async fn connect(&self, endpoint: EndpointId) -> anyhow::Result<Connection> {
        let id = iroh::EndpointId::from_bytes(&endpoint.0).context("not an endpoint id")?;
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
        endpoint: EndpointId,
        envelope: &'a Envelope,
    ) -> BoxFuture<'a, anyhow::Result<MessageResult>> {
        Box::pin(async move {
            if let Some(connection) = self.held(endpoint) {
                // A held connection may have closed on the way; a message
                // sent twice arrives once, so it is tried on a new one. The
                // old one is let go and not closed: byte streams may be
                // open on it.
                if let Ok(result) = exchange(&connection, envelope).await {
                    return Ok(result);
                }
            }
            exchange(&self.connect(endpoint).await?, envelope).await
        })
    }

    fn open<'a>(
        &'a self,
        endpoint: EndpointId,
        open: &'a StreamOpen,
    ) -> BoxFuture<'a, anyhow::Result<Option<ByteStream>>> {
        Box::pin(async move {
            if let Some(connection) = self.held(endpoint)
                && let Ok(opened) = open_stream(&connection, open).await
            {
                return Ok(opened);
            }
            open_stream(&self.connect(endpoint).await?, open).await
        })
    }
}

#[cfg(test)]
mod tests {
    use ciborium::Value;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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

    /// The map is the one docs/gccp.md gives, whatever the Rust types are
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
        let result: Value =
            ciborium::from_reader(&encode_result(MessageResult::Refused)[..]).unwrap();
        assert_eq!(
            result,
            Value::Map(vec![(Value::Text("success".into()), Value::Bool(false))])
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

        for blank in ["", " \n\t"] {
            assert!(decode_message(&encode_message(&envelope(blank))).is_err());
        }
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
    fn a_result_is_a_boolean_and_nothing_else() {
        for (result, success) in [
            (MessageResult::Delivered, true),
            (MessageResult::Refused, false),
        ] {
            assert_eq!(decode_result(&encode_result(result)).unwrap(), result);
            let said = map(&[("success", Value::Bool(success))]);
            assert_eq!(decode_result(&said).unwrap(), result);
        }
        // What is not a result is no result: the message may be stored.
        for not_a_result in [
            map(&[("success", Value::Text("delivered".into()))]),
            map(&[("success", Value::Integer(1.into()))]),
            map(&[("result", Value::Text("delivered".into()))]),
            map(&[]),
        ] {
            assert!(decode_result(&not_a_result).is_err());
        }
    }

    /// What a test's harness takes in: it tells the test, refuses the
    /// message "knock", and has the services "echo", which sends back what
    /// it gets, and "cut", which drops its stream at the first byte.
    #[derive(Clone)]
    struct Seen {
        messages: tokio::sync::mpsc::UnboundedSender<(EndpointId, Envelope)>,
        streams: tokio::sync::mpsc::UnboundedSender<(EndpointId, StreamOpen)>,
    }

    impl Inbox for Seen {
        fn message(
            &self,
            endpoint: EndpointId,
            envelope: Envelope,
        ) -> BoxFuture<'_, MessageResult> {
            let result = if envelope.body == "knock" {
                MessageResult::Refused
            } else {
                MessageResult::Delivered
            };
            self.messages.send((endpoint, envelope)).unwrap();
            Box::pin(async move { result })
        }

        fn join(&self, endpoint: EndpointId, open: StreamOpen) -> BoxFuture<'_, Option<Carry>> {
            let carry: Option<Carry> = match open.service.as_str() {
                "echo" => Some(Box::new(|mut stream| {
                    Box::pin(async move {
                        tokio::io::copy(&mut stream.reader, &mut stream.writer)
                            .await
                            .unwrap();
                        stream.writer.shutdown().await.unwrap();
                    })
                })),
                "cut" => Some(Box::new(|mut stream| {
                    Box::pin(async move {
                        stream.reader.read_u8().await.unwrap();
                        drop(stream);
                    })
                })),
                _ => None,
            };
            self.streams.send((endpoint, open)).unwrap();
            Box::pin(async move { carry })
        }
    }

    /// Two endpoints, one of which serves GCCP to the other.
    struct Pair {
        /// What the opening endpoint sends by, the serving one's id, and
        /// its own.
        transport: IrohTransport,
        to: EndpointId,
        from: EndpointId,
        messages: tokio::sync::mpsc::UnboundedReceiver<(EndpointId, Envelope)>,
        streams: tokio::sync::mpsc::UnboundedReceiver<(EndpointId, StreamOpen)>,
        /// Each connection the serving endpoint accepted.
        connections: tokio::sync::mpsc::UnboundedReceiver<Connection>,
    }

    async fn pair() -> anyhow::Result<Pair> {
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
        let (messages, received) = tokio::sync::mpsc::unbounded_channel();
        let (streams, opened) = tokio::sync::mpsc::unbounded_channel();
        let (connected, connections) = tokio::sync::mpsc::unbounded_channel();
        let seen = Seen { messages, streams };
        tokio::spawn({
            let server = server.clone();
            async move {
                while let Some(incoming) = server.accept().await {
                    let Ok(connection) = incoming.await else {
                        continue;
                    };
                    connected.send(connection.clone()).unwrap();
                    tokio::spawn(serve_streams(seen.clone(), connection));
                }
            }
        });
        Ok(Pair {
            transport: IrohTransport::new(client.clone()),
            to: EndpointId(*server.id().as_bytes()),
            from: EndpointId(*client.id().as_bytes()),
            messages: received,
            streams: opened,
            connections,
        })
    }

    /// Two endpoints: what one sends reaches the other with the sender's
    /// endpoint id, and its result comes back, on one connection for as
    /// long as it lasts; what is not a message is refused without reaching
    /// anything.
    #[tokio::test]
    async fn a_message_crosses_between_two_endpoints() -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let Pair {
                transport,
                to,
                from,
                messages: mut received,
                mut connections,
                ..
            } = pair().await?;

            assert_eq!(
                transport.send(to, &envelope("hello")).await?,
                MessageResult::Delivered
            );
            assert_eq!(received.recv().await.unwrap(), (from, envelope("hello")));
            assert_eq!(
                transport.send(to, &envelope("knock")).await?,
                MessageResult::Refused
            );
            received.recv().await.unwrap();
            let first = connections.recv().await.unwrap();
            assert!(connections.try_recv().is_err(), "one connection for both");

            // The bytes themselves: the kind, most significant byte first,
            // then the map to the end of the stream.
            let connection = transport.held(to).unwrap();
            let said = async |bytes: Vec<u8>| {
                let (mut send, mut recv) = connection.open_bi().await?;
                send.write_all(&bytes).await?;
                send.finish()?;
                anyhow::Ok(recv.read_to_end(MAX_RESULT_LEN).await)
            };
            let message = |payload: &[u8]| [&[0, 1][..], payload].concat();
            assert_eq!(
                decode_result(&said(message(&encode_message(&envelope("raw")))).await??)?,
                MessageResult::Delivered
            );
            assert_eq!(received.recv().await.unwrap().1, envelope("raw"));
            assert_eq!(
                decode_result(&said(message(b"not a message")).await??)?,
                MessageResult::Refused
            );
            assert_eq!(
                decode_result(&said(message(&vec![b'x'; MAX_MESSAGE_LEN + 1])).await??)?,
                MessageResult::Refused,
                "too long to be a message"
            );
            // A kind this version does not know ends its stream, with the
            // code that says so, and nothing else.
            for kind in [[0, 0], [0, 3], [1, 0], [2, 0], [0x80, 1]] {
                let later = [&kind[..], &encode_message(&envelope("later"))].concat();
                let Err(iroh::endpoint::ReadToEndError::Read(iroh::endpoint::ReadError::Reset(
                    code,
                ))) = said(later).await?
                else {
                    panic!("a stream of kind {kind:?} was not reset");
                };
                assert_eq!(code, UNKNOWN_KIND.into());
            }
            assert!(received.try_recv().is_err());

            // The other side closed the connection: the next message takes a
            // new one, and nobody hears of it.
            first.close(0u32.into(), b"");
            assert_eq!(
                transport.send(to, &envelope("again")).await?,
                MessageResult::Delivered
            );
            assert_eq!(received.recv().await.unwrap().1, envelope("again"));
            connections.recv().await.unwrap();
            Ok(())
        })
        .await?
    }

    fn open(service: &str) -> StreamOpen {
        StreamOpen {
            to: "to-1".parse().unwrap(),
            from: "from.2".parse().unwrap(),
            service: service.parse().unwrap(),
        }
    }

    /// The header is the map docs/gccp.md gives, and is read as strictly as
    /// a message is.
    #[test]
    fn a_stream_header_is_the_map_the_protocol_names() {
        let fields = [
            ("to", Value::Text("to-1".into())),
            ("from", Value::Text("from.2".into())),
            ("service", Value::Text("web".into())),
        ];
        let value: Value = ciborium::from_reader(&encode_open(&open("web"))[..]).unwrap();
        let named = fields
            .iter()
            .map(|(key, value)| (Value::Text((*key).to_owned()), value.clone()))
            .collect();
        assert_eq!(value, Value::Map(named));
        assert!(encode_open(&open(&"s".repeat(64))).len() <= MAX_OPEN_LEN);

        let mut later = fields.to_vec();
        later.push(("window", Value::Integer(3.into())));
        assert_eq!(decode_open(&map(&later)).unwrap(), open("web"));
        for missing in 0..fields.len() {
            let mut rest = fields.to_vec();
            rest.remove(missing);
            assert!(decode_open(&map(&rest)).is_err(), "without {missing}");
        }
        // A service is a name, in the form of a local id: not a port, and
        // not a path.
        for not_a_name in [
            Value::Integer(8080.into()),
            Value::Text("../host.sock".into()),
            Value::Text("Web".into()),
            Value::Text(String::new()),
        ] {
            let mut bad = fields.to_vec();
            bad[2].1 = not_a_name.clone();
            assert!(decode_open(&map(&bad)).is_err(), "{not_a_name:?}");
        }
    }

    /// A byte stream: its header reaches the other harness with the
    /// opener's endpoint id, the result comes back before any other byte
    /// is sent, and after a `true` the bytes pass both ways unchanged and
    /// each way ends on its own. A refusal is a `false` and no more, and
    /// so is a header that cannot be read. A stream whose end was dropped
    /// is seen to have broken, not to have ended.
    #[tokio::test]
    async fn a_byte_stream_crosses_once_the_other_harness_joined_it() -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let Pair {
                transport,
                to,
                from,
                mut streams,
                mut connections,
                ..
            } = pair().await?;

            let mut stream = transport.open(to, &open("echo")).await?.expect("joined");
            assert_eq!(streams.recv().await.unwrap(), (from, open("echo")));
            // Both ways are open at once: an answer comes while this side
            // can still ask.
            let mut back = [0; 4];
            for ask in [b"ping", b"pong"] {
                stream.writer.write_all(ask).await?;
                stream.reader.read_exact(&mut back).await?;
                assert_eq!(&back, ask);
            }
            let much: Vec<u8> = (0..1_000_000u32).map(|n| (n % 251) as u8).collect();
            let mut echoed = Vec::new();
            let (sent, read) = tokio::join!(
                async {
                    stream.writer.write_all(&much).await?;
                    // This side's bytes end; the other side's still come.
                    stream.writer.shutdown().await
                },
                stream.reader.read_to_end(&mut echoed),
            );
            sent?;
            read?;
            assert!(echoed == much);

            assert!(transport.open(to, &open("closed")).await?.is_none());
            assert_eq!(streams.recv().await.unwrap(), (from, open("closed")));
            connections.recv().await.unwrap();
            assert!(connections.try_recv().is_err(), "one connection for all");

            // The other end dropped the stream: this end's read fails,
            // where the end of the bytes would have read as nothing.
            let mut cut = transport.open(to, &open("cut")).await?.expect("joined");
            streams.recv().await.unwrap();
            cut.writer.write_all(b"x").await?;
            assert!(cut.reader.read(&mut back).await.is_err());

            // The bytes themselves: the kind, then the header behind its
            // length, both most significant byte first. The result comes
            // the same way, with nothing sent past the header.
            let connection = transport.held(to).unwrap();
            let header = |map: &[u8]| [&[0, 2, 0, map.len() as u8][..], map].concat();
            let result = |success| {
                let said = map(&[("success", Value::Bool(success))]);
                [&[0, said.len() as u8][..], &said].concat()
            };
            let (mut send, mut recv) = connection.open_bi().await?;
            send.write_all(&header(&encode_open(&open("echo")))).await?;
            let mut said = vec![0; result(true).len()];
            recv.read_exact(&mut said).await?;
            assert_eq!(said, result(true));
            send.write_all(b"raw").await?;
            send.finish()?;
            assert_eq!(recv.read_to_end(16).await?, b"raw");

            // Refused, or not a header: `false`, and the stream ends.
            let longer_than_said = [&[0, 2, 0, 200][..], b"cut off"].concat();
            for refused in [
                header(&encode_open(&open("closed"))),
                header(b"not a header"),
                header(&map(&[("to", Value::Text("to-1".into()))])),
                longer_than_said,
                vec![0, 2, 0xff, 0xff],
            ] {
                let (mut send, mut recv) = connection.open_bi().await?;
                send.write_all(&refused).await?;
                send.finish()?;
                assert_eq!(recv.read_to_end(64).await?, result(false), "{refused:?}");
            }
            Ok(())
        })
        .await?
    }
}
