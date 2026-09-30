//! Stream-scoped MoQ sessions on an application-owned Iroh connection.
//! The application routes bidirectional streams marked with PREFIX here;
//! ordinary RPC streams retain their existing framing and compression.
use bytes::Bytes;
use std::{
	collections::HashMap,
	future::{Future, poll_fn},
	pin::Pin,
	sync::{Arc, Mutex, Weak},
	task::{Context, Poll, ready},
	time::Duration,
};
use tokio::{
	sync::{Mutex as AsyncMutex, mpsc, watch},
	time::Instant,
};
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
	pacer: Arc<Pacer>,
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
			pacer: Arc::new(Pacer::default()),
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
// Application submission scheduling, not a replacement for QUIC congestion
// control. At most one small burst is outstanding; idle time earns no credit.
const SEND_BURST: usize = 8 * 1024;
struct Pacer {
	state: Mutex<Budget>,
	changed: watch::Sender<()>,
}
struct Budget {
	rate: u64,
	debt: f64,
	updated: Instant,
}
impl Budget {
	fn update(&mut self, now: Instant) {
		self.debt = (self.debt - now.duration_since(self.updated).as_secs_f64() * self.rate as f64).max(0.0);
		self.updated = now;
	}
}
impl Default for Pacer {
	fn default() -> Self {
		Self {
			state: Mutex::new(Budget {
				rate: 0,
				debt: 0.0,
				updated: Instant::now(),
			}),
			changed: watch::channel(()).0,
		}
	}
}
impl Pacer {
	fn set_rate(&self, rate: u64) {
		let mut state = self.state.lock().unwrap();
		if rate == state.rate {
			return;
		}
		// Retire elapsed debt at the old rate before applying the new one.
		state.update(Instant::now());
		state.rate = rate;
		if rate == 0 {
			state.debt = 0.0;
		}
		drop(state);
		self.changed.send_replace(());
	}
	async fn write<E>(
		&self,
		size: usize,
		mut write: impl FnMut(&mut Context<'_>, usize) -> Poll<Result<usize, E>>,
	) -> Result<usize, E> {
		if size == 0 {
			return poll_fn(|cx| write(cx, 0)).await;
		}
		let mut changed = self.changed.subscribe();
		loop {
			let mut sleep = Box::pin(tokio::time::sleep(Duration::ZERO));
			let attempt = poll_fn(|cx| {
				loop {
					let mut state = self.state.lock().unwrap();
					let now = Instant::now();
					state.update(now);
					if state.rate != 0 && state.debt > 0.0 {
						let delay =
							Duration::from_secs_f64(state.debt / state.rate as f64).max(Duration::from_nanos(1));
						drop(state);
						sleep.as_mut().reset(now + delay);
						ready!(sleep.as_mut().poll(cx));
						continue;
					}
					let limit = if state.rate == 0 { size } else { size.min(SEND_BURST) };
					// Poll, never await, under the lock. A flow-control-blocked
					// stream neither owns future credit nor blocks another stream.
					let written = ready!(write(cx, limit))?;
					if state.rate != 0 {
						state.debt = written as f64;
					}
					return Poll::Ready(Ok(written));
				}
			});
			tokio::select! {
				result = attempt => return result,
				_ = changed.changed() => {},
			}
		}
	}
}
/// A single viewer's transport, sharing congestion control with all Rho traffic.
#[derive(Clone)]
pub struct Session {
	connection: Connection,
	route: Arc<Route>,
}
impl Session {
	/// RTT of the currently selected network path, excluding unused probe paths.
	pub fn rtt(&self) -> std::time::Duration {
		self.connection
			.paths()
			.iter()
			.filter(|p| p.is_selected())
			.map(|p| p.rtt())
			.max()
			.unwrap_or(std::time::Duration::from_millis(100))
	}
	/// Limit aggregate outbound unidirectional media submission in bytes/sec.
	/// Zero disables pacing (the default). Bidirectional control is never paced.
	/// Nonzero updates preserve outstanding byte debt and wake waiting writers.
	pub fn set_send_rate(&self, bytes_per_second: u64) {
		self.route.pacer.set_rate(bytes_per_second);
	}
	async fn header(&self, send: &mut Send) -> Result<(), Error> {
		if self.route.closed.borrow().is_some() {
			return Err(error("media session closed"));
		}
		let mut header = [PREFIX; 9];
		header[1..].copy_from_slice(&self.route.id.to_be_bytes());
		send.0.set_priority(-100).map_err(error)?;
		wt::SendStream::write_all(send, &header).await
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
			stream=async { self.route.incoming_bi.lock().await.recv().await } => stream.map(|(s,r)|(Send(s,false,None),Recv(r))).ok_or_else(||error("media closed")),
			error=self.closed()=>Err(error),
		}
	}
	async fn open_uni(&self) -> Result<Send, Error> {
		let send = self.connection.open_uni().await.map_err(error)?;
		let mut send = Send(send, false, Some(self.route.pacer.clone()));
		self.header(&mut send).await?;
		Ok(send)
	}
	async fn open_bi(&self) -> Result<(Send, Recv), Error> {
		let (send, recv) = self.connection.open_bi().await.map_err(error)?;
		let mut send = Send(send, false, None);
		self.header(&mut send).await?;
		Ok((send, Recv(recv)))
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
pub struct Send(SendStream, bool, Option<Arc<Pacer>>);
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
		match &self.2 {
			Some(pacer) => pacer
				.write(buf.len(), |cx, limit| {
					Pin::new(&mut self.0).poll_write(cx, &buf[..limit])
				})
				.await
				.map_err(write_error),
			None => self.0.write(buf).await.map_err(write_error),
		}
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

#[cfg(test)]
mod tests {
	use super::*;
	use futures::FutureExt;

	#[tokio::test]
	async fn shared_budget_caps_bursts_and_never_banks_idle_credit() {
		tokio::time::pause();
		let pacer = Arc::new(Pacer::default());
		pacer.set_rate(8192);
		let stream_a = pacer.clone();
		let stream_b = pacer.clone();
		let write = |_: &mut Context<'_>, limit| Poll::Ready(Ok::<_, ()>(limit));
		assert_eq!(stream_a.write(30_000, write).await.unwrap(), 8192);
		// Another stream shares the debt, even if its predecessor finished.
		assert!(stream_b.write(30_000, write).now_or_never().is_none());
		tokio::time::advance(Duration::from_millis(999)).await;
		assert!(stream_b.write(30_000, write).now_or_never().is_none());
		tokio::time::advance(Duration::from_millis(1)).await;
		assert_eq!(stream_b.write(30_000, write).await.unwrap(), 8192);
		tokio::time::advance(Duration::from_secs(60)).await;
		assert_eq!(stream_a.write(30_000, write).await.unwrap(), 8192);
		assert!(stream_b.write(30_000, write).now_or_never().is_none());
	}

	#[tokio::test]
	async fn rate_updates_preserve_debt_and_wake_an_existing_write() {
		tokio::time::pause();
		let pacer = Arc::new(Pacer::default());
		let write = |_: &mut Context<'_>, limit| Poll::Ready(Ok::<_, ()>(limit));
		pacer.set_rate(8192);
		assert_eq!(pacer.write(8192, write).await.unwrap(), 8192);
		let other = pacer.clone();
		let waiting = tokio::spawn(async move { other.write(8192, write).await });
		tokio::task::yield_now().await;
		assert!(!waiting.is_finished());
		tokio::time::advance(Duration::from_millis(250)).await;
		// 6144 bytes remain. Doubling the rate drains them in 375ms.
		pacer.set_rate(16384);
		tokio::task::yield_now().await;
		assert!(!waiting.is_finished());
		tokio::time::advance(Duration::from_millis(374)).await;
		// Repeated feedback must not reset debt or create fresh burst credit.
		pacer.set_rate(16384);
		tokio::task::yield_now().await;
		assert!(!waiting.is_finished());
		// Sleep deadlines round up to Tokio's millisecond timer tick.
		// Completion by 626ms (not the original 1000ms) verifies the wake.
		tokio::time::advance(Duration::from_millis(2)).await;
		tokio::task::yield_now().await;
		assert!(waiting.is_finished(), "rate change must wake the old 1s sleep");
		assert_eq!(waiting.await.unwrap(), Ok(8192));

		tokio::time::advance(Duration::from_millis(125)).await;
		pacer.set_rate(4096);
		// 6144 bytes at the lower rate need another 1.5 seconds.
		tokio::time::advance(Duration::from_millis(1499)).await;
		assert!(pacer.write(1, write).now_or_never().is_none());
		tokio::time::advance(Duration::from_millis(1)).await;
		assert_eq!(pacer.write(1, write).now_or_never(), Some(Ok(1)));
	}

	#[tokio::test]
	async fn short_pending_failed_and_cancelled_writes_charge_only_accepted_bytes() {
		tokio::time::pause();
		let pacer = Pacer::default();
		pacer.set_rate(8192);
		assert_eq!(
			pacer
				.write(30_000, |_, limit| {
					assert_eq!(limit, 8192);
					Poll::Ready(Ok::<_, ()>(1024))
				})
				.await
				.unwrap(),
			1024
		);
		// Cancelling a future waiting for pacing must not reserve another burst.
		assert!(
			pacer
				.write(30_000, |_, _| Poll::Ready(Ok::<_, ()>(8192)))
				.now_or_never()
				.is_none()
		);
		tokio::time::advance(Duration::from_millis(124)).await;
		assert!(
			pacer
				.write(1, |_, _| Poll::Ready(Ok::<_, ()>(1)))
				.now_or_never()
				.is_none()
		);
		tokio::time::advance(Duration::from_millis(1)).await;
		// A flow-control-blocked stream cannot hostage session budget.
		assert!(
			pacer
				.write(30_000, |_, _| Poll::<Result<usize, ()>>::Pending)
				.now_or_never()
				.is_none()
		);
		assert_eq!(
			pacer
				.write(30_000, |_, _| Poll::Ready(Err::<usize, _>("stopped")))
				.await,
			Err("stopped")
		);
		assert_eq!(
			pacer
				.write(30_000, |_, limit| Poll::Ready(Ok::<_, ()>(limit)))
				.await
				.unwrap(),
			8192
		);
		// Empty writes remain immediate, even while a burst is outstanding.
		assert_eq!(
			pacer
				.write(0, |_, limit| {
					assert_eq!(limit, 0);
					Poll::Ready(Ok::<_, ()>(0))
				})
				.await,
			Ok(0)
		);
		assert_eq!(
			pacer.write(0, |_, _| Poll::Ready(Err::<usize, _>("closed"))).await,
			Err("closed")
		);
	}

	#[tokio::test]
	async fn unlimited_by_default_and_disabling_wakes_waiters() {
		tokio::time::pause();
		let pacer = Pacer::default();
		let write = |_: &mut Context<'_>, limit| Poll::Ready(Ok::<_, ()>(limit));
		assert_eq!(pacer.write(30_000, write).await.unwrap(), 30_000);
		pacer.set_rate(1);
		assert_eq!(pacer.write(8192, write).await.unwrap(), 8192);
		let waiting = pacer.write(30_000, write);
		tokio::pin!(waiting);
		assert!(waiting.as_mut().now_or_never().is_none());
		pacer.set_rate(0);
		assert_eq!(waiting.as_mut().now_or_never(), Some(Ok(30_000)));
	}

	#[tokio::test]
	async fn producer_abort_resets_paced_flow_blocked_and_fin_submitted_groups() -> anyhow::Result<()> {
		use web_transport_iroh::iroh::{
			Endpoint,
			endpoint::{QuicTransportConfig, presets},
		};
		use wt::{Error as _, RecvStream as _, SendStream as _, Session as _};
		let alpn = b"moq-pacer-test";
		let config = QuicTransportConfig::builder()
			.send_window(4096)
			.stream_receive_window(1024u32.into())
			.build();
		let server = Endpoint::builder(presets::Minimal)
			.alpns(vec![alpn.to_vec()])
			.transport_config(config.clone())
			.bind_addr("127.0.0.1:0")?
			.bind()
			.await?;
		let client = Endpoint::builder(presets::Minimal)
			.alpns(vec![alpn.to_vec()])
			.transport_config(config)
			.bind_addr("127.0.0.1:0")?
			.bind()
			.await?;
		let (connection, accepted) = tokio::try_join!(
			async { Ok::<_, anyhow::Error>(client.connect(server.addr(), alpn).await?) },
			async { Ok::<_, anyhow::Error>(server.accept().await.expect("incoming connection").await?) },
		)?;
		let near = Mux::new(connection);
		let far = Mux::new(accepted);
		let sending = near.session(7)?;
		let receiving = far.session(7)?;
		let uni_route = far.clone();
		let router = tokio::spawn(async move { uni_route.receive_uni().await });
		let bi_router = tokio::spawn(async move { far.receive_bi().await });
		let origin = crate::origin::spawn();
		let broadcast = origin.create_broadcast("desktop")?;
		let relay = origin.consume().request_broadcast("desktop").await?;
		let mut cx = Context::from_waker(futures::task::noop_waker_ref());
		// A checkpoint group is already live while obsolete states are reset.
		let checkpoints = broadcast.create_track("checkpoints", None)?;
		let mut publishing_checkpoint = Box::pin(moq_net::publish_fixed(
			crate::transport::Session::new(sending.clone()),
			relay.track("checkpoints")?.subscribe(None).await?,
			0,
		));
		let mut checkpoint = checkpoints.append_group()?;
		checkpoint.write_frame(moq_net::Timestamp::ZERO, b"checkpoint-head".as_slice())?;
		assert!(publishing_checkpoint.as_mut().poll(&mut cx).is_pending());
		let checkpoint_stream = tokio::time::timeout(Duration::from_secs(2), receiving.accept_uni()).await??;
		let states = broadcast.create_track("states", None)?;
		let mut publishing = Box::pin(moq_net::publish_fixed(
			crate::transport::Session::new(sending.clone()),
			relay.track("states")?.subscribe(None).await?,
			1,
		));
		for mode in 0..3 {
			let mut old = states.append_group()?;
			assert!(publishing.as_mut().poll(&mut cx).is_pending());
			// Ensure routing bytes arrived before resetting, while the producer
			// has not finished and the outgoing group remains writable.
			let mut obsolete = tokio::time::timeout(Duration::from_secs(2), receiving.accept_uni()).await??;
			if mode == 0 {
				sending.set_send_rate(1);
				// Establish debt before the publisher attempts the frame header/body.
				let mut debt = sending.open_uni().await?;
				debt.reset(0);
			}
			let size = match mode {
				0 => 4096,
				1 => 32 * 1024,
				_ => 31,
			};
			old.write_frame(moq_net::Timestamp::ZERO, vec![mode as u8; size])?;
			old.finish()?;
			// No yield between FIN submission and abort: the current-thread
			// endpoint driver cannot acknowledge the just-submitted final bytes.
			assert!(publishing.as_mut().poll(&mut cx).is_pending());
			old.abort(moq_net::Error::Old)?;
			assert!(publishing.as_mut().poll(&mut cx).is_pending());
			let err = tokio::time::timeout(Duration::from_secs(2), async {
				loop {
					match obsolete.read_chunk(4096).await {
						Ok(Some(_)) => {}
						Ok(None) => panic!("obsolete group reached FIN instead of reset"),
						Err(err) => break err,
					}
				}
			})
			.await?;
			assert_eq!(err.stream_error(), Some(moq_net::StreamError::Old.to_code()));
			sending.set_send_rate(0);
		}
		// Continue the same checkpoint after all three cancellation states.
		checkpoint.write_frame(moq_net::Timestamp::ZERO, b"checkpoint-tail".as_slice())?;
		checkpoint.finish()?;
		assert!(publishing_checkpoint.as_mut().poll(&mut cx).is_pending());
		let target = moq_net::broadcast::Info::default().produce();
		let targets = [
			target.create_track("checkpoints", None)?,
			target.create_track("states", None)?,
		];
		tokio::time::timeout(
			Duration::from_secs(2),
			moq_net::receive_fixed_group(
				crate::transport::RecvStream::new(checkpoint_stream),
				&targets,
				moq_net::Timescale::default(),
			),
		)
		.await??;
		let mut received = targets[0].subscribe(None);
		let mut group = received.recv_group().await?.expect("checkpoint");
		assert_eq!(
			group.read_frame().await?.expect("frame").payload.as_ref(),
			b"checkpoint-head"
		);
		assert_eq!(
			group.read_frame().await?.expect("tail").payload.as_ref(),
			b"checkpoint-tail"
		);
		assert!(group.read_frame().await?.is_none());
		assert!(targets[1].subscribe(None).recv_group().now_or_never().is_none());
		// Nonzero routing must update only the state target.
		let mut fresh = states.append_group()?;
		fresh.write_frame(moq_net::Timestamp::ZERO, b"fresh-state".as_slice())?;
		fresh.finish()?;
		assert!(publishing.as_mut().poll(&mut cx).is_pending());
		let stream = tokio::time::timeout(Duration::from_secs(2), receiving.accept_uni()).await??;
		tokio::time::timeout(
			Duration::from_secs(2),
			moq_net::receive_fixed_group(
				crate::transport::RecvStream::new(stream),
				&targets,
				moq_net::Timescale::default(),
			),
		)
		.await??;
		let mut state_target = targets[1].subscribe(None);
		let mut group = state_target.recv_group().await?.expect("state group");
		assert_eq!(
			group.read_frame().await?.expect("frame").payload.as_ref(),
			b"fresh-state"
		);
		let (mut control, mut response) = sending.open_bi().await?;
		control.write_all(b"control").await?;
		control.finish()?;
		response.stop(0);
		let (_, mut receive) = tokio::time::timeout(Duration::from_secs(2), receiving.accept_bi()).await??;
		let mut message = Vec::new();
		tokio::time::timeout(Duration::from_secs(2), async {
			while let Some(chunk) = receive.read_chunk(4096).await? {
				message.extend_from_slice(&chunk);
			}
			Ok::<_, Error>(())
		})
		.await??;
		assert_eq!(message, b"control");
		router.abort();
		bi_router.abort();
		client.close().await;
		server.close().await;
		Ok(())
	}

	#[tokio::test]
	async fn direct_iroh_uni_submission_is_paced_while_bi_control_bypasses() -> anyhow::Result<()> {
		use web_transport_iroh::iroh::{Endpoint, endpoint::presets};
		use wt::{RecvStream as _, SendStream as _, Session as _};
		let alpn = b"moq-pacer-test";
		let server = Endpoint::builder(presets::Minimal)
			.alpns(vec![alpn.to_vec()])
			.bind_addr("127.0.0.1:0")?
			.bind()
			.await?;
		let client = Endpoint::builder(presets::Minimal)
			.alpns(vec![alpn.to_vec()])
			.bind_addr("127.0.0.1:0")?
			.bind()
			.await?;
		let (connection, accepted) = tokio::try_join!(
			async { Ok::<_, anyhow::Error>(client.connect(server.addr(), alpn).await?) },
			async { Ok::<_, anyhow::Error>(server.accept().await.expect("incoming connection").await?) },
		)?;
		let near = Mux::new(connection);
		let far = Mux::new(accepted);
		let sending = near.session(3)?;
		let receiving = far.session(3)?;
		let route_uni = far.clone();
		let uni_router = tokio::spawn(async move { route_uni.receive_uni().await });
		let route_bi = far.clone();
		let bi_router = tokio::spawn(async move { route_bi.receive_bi().await });
		// Header pacing has consumed nine bytes, so at 1 B/s the media
		// body is guaranteed to remain blocked throughout the control test.
		sending.set_send_rate(1);
		let mut uni = sending.open_uni().await?;
		let mut media = receiving.accept_uni().await?;
		assert!(uni.write(&[37; 20_000]).now_or_never().is_none());
		tokio::time::timeout(Duration::from_secs(2), async {
			let (mut control, _) = sending.open_bi().await?;
			let (_, mut receive) = receiving.accept_bi().await?;
			control.write_all(b"control").await?;
			control.finish()?;
			let mut message = [0; 7];
			let mut read = 0;
			while read < message.len() {
				read += receive.read(&mut message[read..]).await?.expect("control bytes");
			}
			assert_eq!(&message, b"control");
			Ok::<_, anyhow::Error>(())
		})
		.await??;
		// The rate applies to streams opened before the update as well.
		sending.set_send_rate(8192);
		let started = Instant::now();
		let first = uni.write(&[37; 20_000]).await?;
		assert!(first > 0 && first <= 8192);
		assert!(started.elapsed() < Duration::from_millis(500));
		let mut received = vec![0; first];
		let mut read = 0;
		while read < first {
			read += media.read(&mut received[read..]).await?.expect("media bytes");
		}
		assert!(received.iter().all(|byte| *byte == 37));
		assert!(uni.write(&[91; 20_000]).now_or_never().is_none());
		// Finishing/resetting a stream does not release its accepted byte debt.
		uni.reset(0);
		let next_header = sending.open_uni();
		tokio::pin!(next_header);
		assert!(next_header.as_mut().now_or_never().is_none());
		sending.set_send_rate(0);
		let mut next = next_header.await?;
		assert_eq!(next.write(&[91; 20_000]).await?, 20_000);
		next.finish()?;
		uni_router.abort();
		bi_router.abort();
		client.close().await;
		server.close().await;
		Ok(())
	}
}
