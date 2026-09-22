//! Stream-scoped MoQ sessions on an application-owned Iroh connection.
//! The application routes bidirectional streams marked with PREFIX here;
//! ordinary RPC streams retain their existing framing and compression.
use bytes::Bytes;
use std::{
	collections::HashMap,
	sync::{Arc, Mutex, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, mpsc, watch};
use web_transport_iroh::iroh::endpoint::{Connection, RecvStream, SendStream};
use web_transport_trait as wt;

/// Prefix distinct from the Zstandard magic used by Rho RPC streams.
pub const PREFIX: u8 = 0xff;
/// Connection-wide demultiplexer. Dropping it does not close the connection.
#[derive(Clone)]
pub struct Mux(Arc<Inner>);
struct Inner {
	connection: Connection,
	routes: Mutex<HashMap<u64, Weak<Route>>>,
}
struct Route {
	id: u64,
	owner: Weak<Inner>,
	bi: mpsc::Sender<(SendStream, RecvStream)>,
	uni: mpsc::Sender<RecvStream>,
	incoming_bi: AsyncMutex<mpsc::Receiver<(SendStream, RecvStream)>>,
	incoming_uni: AsyncMutex<mpsc::Receiver<RecvStream>>,
	closed: watch::Sender<Option<Error>>,
}
impl Drop for Route {
	fn drop(&mut self) {
		if let Some(owner) = self.owner.upgrade() {
			owner.routes.lock().unwrap().remove(&self.id);
		}
	}
}
/// A media-local transport failure.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct Error(String, Option<u32>);
impl wt::Error for Error {
	fn stream_error(&self) -> Option<u32> {
		self.1
	}
	fn session_error(&self) -> Option<(u32, String)> {
		if self.1.is_none() {
			Some((1, self.0.clone()))
		} else {
			None
		}
	}
}
fn error(e: impl std::fmt::Display) -> Error {
	Error(e.to_string(), None)
}

fn reset(code: web_transport_iroh::iroh::endpoint::VarInt) -> Error {
	Error(format!("stream reset: {code}"), u32::try_from(code.into_inner()).ok())
}
fn read_error(e: web_transport_iroh::iroh::endpoint::ReadError) -> Error {
	match e {
		web_transport_iroh::iroh::endpoint::ReadError::Reset(code) => reset(code),
		other => error(other),
	}
}
fn write_error(e: web_transport_iroh::iroh::endpoint::WriteError) -> Error {
	match e {
		web_transport_iroh::iroh::endpoint::WriteError::Stopped(code) => reset(code),
		other => error(other),
	}
}
impl Mux {
	/// Attach to an already authenticated connection, without accepting RPC streams.
	pub fn new(connection: Connection) -> Self {
		Self(Arc::new(Inner {
			connection,
			routes: Mutex::new(HashMap::new()),
		}))
	}
	/// Register a session before the peer is allowed to open its streams.
	pub fn session(&self, id: u64) -> Result<Session, Error> {
		let mut routes = self.0.routes.lock().unwrap();
		if routes.get(&id).and_then(Weak::upgrade).is_some() {
			return Err(error("duplicate media session"));
		}
		if routes.len() >= 32 {
			return Err(error("too many media sessions"));
		}
		let (bi, incoming_bi) = mpsc::channel(32);
		let (uni, incoming_uni) = mpsc::channel(32);
		let (closed, _) = watch::channel(None);
		let route = Arc::new(Route {
			id,
			owner: Arc::downgrade(&self.0),
			bi,
			uni,
			incoming_bi: AsyncMutex::new(incoming_bi),
			incoming_uni: AsyncMutex::new(incoming_uni),
			closed,
		});
		routes.insert(id, Arc::downgrade(&route));
		Ok(Session {
			connection: self.0.connection.clone(),
			route,
		})
	}
	/// Route a stream whose PREFIX byte has already been consumed.
	pub async fn route_bi(&self, send: SendStream, mut recv: RecvStream) -> Result<(), Error> {
		let id = read_id(&mut recv).await?;
		let route = self
			.0
			.routes
			.lock()
			.unwrap()
			.get(&id)
			.and_then(Weak::upgrade)
			.ok_or_else(|| error("unknown media session"))?;
		route.bi.try_send((send, recv)).map_err(error)
	}
	/// Accept all peer-created unidirectional media streams until disconnect.
	pub async fn receive_uni(&self) -> Result<(), Error> {
		while let Ok(mut recv) = self.0.connection.accept_uni().await {
			let this = self.clone();
			tokio::spawn(async move {
				let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
					let mut prefix = [0];
					recv.read_exact(&mut prefix).await.map_err(error)?;
					if prefix[0] != PREFIX {
						return Err(error("invalid media prefix"));
					}
					let id = read_id(&mut recv).await?;
					let route = this
						.0
						.routes
						.lock()
						.unwrap()
						.get(&id)
						.and_then(Weak::upgrade)
						.ok_or_else(|| error("unknown media session"))?;
					route.uni.try_send(recv).map_err(error)
				})
				.await;
				if !matches!(result, Ok(Ok(()))) {
					tracing::debug!("rejected media stream");
				}
			});
		}
		Err(error("connection closed"))
	}
	/// Client-side receiver; Rho servers initiate only media bidirectional streams.
	pub async fn receive_bi(&self) -> Result<(), Error> {
		while let Ok((send, mut recv)) = self.0.connection.accept_bi().await {
			let this = self.clone();
			tokio::spawn(async move {
				let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
					let mut prefix = [0];
					recv.read_exact(&mut prefix).await.map_err(error)?;
					if prefix[0] != PREFIX {
						return Err(error("invalid media prefix"));
					}
					this.route_bi(send, recv).await
				})
				.await;
			});
		}
		Err(error("connection closed"))
	}
}
async fn read_id(recv: &mut RecvStream) -> Result<u64, Error> {
	let mut id = [0; 8];
	recv.read_exact(&mut id).await.map_err(error)?;
	Ok(u64::from_be_bytes(id))
}
/// A single viewer's transport, sharing congestion control with all Rho traffic.
#[derive(Clone)]
pub struct Session {
	connection: Connection,
	route: Arc<Route>,
}
impl Session {
	async fn header(&self, send: &mut SendStream) -> Result<(), Error> {
		if self.route.closed.borrow().is_some() {
			return Err(error("media session closed"));
		}
		let mut header = [PREFIX; 9];
		header[1..].copy_from_slice(&self.route.id.to_be_bytes());
		send.set_priority(-100).map_err(error)?;
		send.write_all(&header).await.map_err(error)
	}
}
impl wt::Session for Session {
	type SendStream = Send;
	type RecvStream = Recv;
	type Error = Error;
	async fn accept_uni(&self) -> Result<Recv, Error> {
		tokio::select! {
			stream=async { self.route.incoming_uni.lock().await.recv().await } => stream.map(Recv).ok_or_else(||error("media closed")),
			error=self.closed()=>Err(error),
		}
	}
	async fn accept_bi(&self) -> Result<(Send, Recv), Error> {
		tokio::select! {
			stream=async { self.route.incoming_bi.lock().await.recv().await } => stream.map(|(s,r)|(Send(s,false),Recv(r))).ok_or_else(||error("media closed")),
			error=self.closed()=>Err(error),
		}
	}
	async fn open_uni(&self) -> Result<Send, Error> {
		let mut send = self.connection.open_uni().await.map_err(error)?;
		self.header(&mut send).await?;
		Ok(Send(send, false))
	}
	async fn open_bi(&self) -> Result<(Send, Recv), Error> {
		let (mut send, recv) = self.connection.open_bi().await.map_err(error)?;
		self.header(&mut send).await?;
		Ok((Send(send, false), Recv(recv)))
	}
	fn send_datagram(&self, _: Bytes) -> Result<(), Error> {
		Err(error("media uses QUIC streams only"))
	}
	async fn recv_datagram(&self) -> Result<Bytes, Error> {
		Err(self.closed().await)
	}
	fn max_datagram_size(&self) -> usize {
		0
	}
	fn protocol(&self) -> Option<&str> {
		Some("moq-lite-05")
	}
	fn close(&self, _: u32, reason: &str) {
		self.route.closed.send_replace(Some(error(reason)));
		if let Some(owner) = self.route.owner.upgrade() {
			owner.routes.lock().unwrap().remove(&self.route.id);
		}
	}
	async fn closed(&self) -> Error {
		let mut closed = self.route.closed.subscribe();
		tokio::select! {
			_=closed.wait_for(Option::is_some)=>error("media session closed"),
			reason=self.connection.closed()=>error(reason),
		}
	}
}
/// An outgoing media stream.
pub struct Send(SendStream, bool);
impl Drop for Send {
	fn drop(&mut self) {
		if !self.1 {
			let _ = self.0.reset(0u8.into());
		}
	}
}
impl wt::SendStream for Send {
	type Error = Error;
	async fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
		self.0.write(buf).await.map_err(write_error)
	}
	fn set_priority(&mut self, priority: u8) {
		let _ = self.0.set_priority(-256 + i32::from(priority));
	}
	fn finish(&mut self) -> Result<(), Error> {
		self.0.finish().map_err(error)?;
		self.1 = true;
		Ok(())
	}
	fn reset(&mut self, code: u32) {
		let _ = self.0.reset(code.into());
	}
	async fn closed(&mut self) -> Result<(), Error> {
		match self.0.stopped().await.map_err(error)? {
			Some(code) => Err(reset(code)),
			None => Ok(()),
		}
	}
}
/// An incoming media stream.
pub struct Recv(RecvStream);
impl wt::RecvStream for Recv {
	type Error = Error;
	async fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, Error> {
		self.0.read(buf).await.map_err(read_error)
	}
	async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>, Error> {
		self.0.read_chunk(max).await.map_err(read_error)
	}
	fn stop(&mut self, code: u32) {
		let _ = self.0.stop(code.into());
	}
	async fn closed(&mut self) -> Result<(), Error> {
		match self.0.received_reset().await.map_err(error)? {
			Some(code) => Err(reset(code)),
			None => Ok(()),
		}
	}
}
