//! Agent host connection: an IO task on the tokio runtime the caller hands in,
//! bridged to the reader through sinks. Each thing the host pushes has a
//! stream of its own, and what they carry becomes [`ConnEvent`]s on a
//! futures channel the workspace awaits (no polling); every request is a
//! stream of its own, answered once.

use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;

use crate::protocol;

/// Set when the client is going away, before its tokio runtime is dropped.
///
/// A supervisor waiting out a reconnect delay is a task inside
/// `tokio::time::sleep`, and a sleep still being polled when its runtime
/// shuts down panics in tokio's timer ("A Tokio 1.x context was found, but
/// it is being shutdown"). The supervisor has to end before the runtime
/// does, so quit says so here and every supervisor wakes and stops.
static CLOSING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CLOSED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Ends every host supervisor. Called from the app's quit hook, which runs
/// while the runtime is still there; nothing waits for the supervisors,
/// they are told and quit carries on.
pub fn close() {
    CLOSING.store(true, std::sync::atomic::Ordering::SeqCst);
    CLOSED.notify_waiters();
}

fn closing() -> bool {
    CLOSING.load(std::sync::atomic::Ordering::SeqCst)
}

/// Puts the flag back, for a test that has just closed the world. Nothing
/// reopens a real client: it is quitting.
#[cfg(test)]
fn reopen() {
    CLOSING.store(false, std::sync::atomic::Ordering::SeqCst);
}

const INITIAL_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
const MAX_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

fn next_reconnect_delay(delay: std::time::Duration) -> std::time::Duration {
    (delay * 2).min(MAX_RECONNECT_DELAY)
}

use crate::{AttachTarget, Dialer, HostId, HostStream, IrohAccess};

/// A connection event tagged with the agent host it came from. Every attached
/// host feeds the same channel, so the workspace handles one ordered stream
/// rather than polling several.
pub struct HostEvent {
    pub host: HostId,
    pub event: ConnEvent,
}

/// One host's end of the shared event channel. Every IO task holds a clone
/// and stamps its own [`HostId`] on whatever it sends.
#[derive(Clone)]
pub(crate) struct EventSink {
    host: HostId,
    events: std::sync::Arc<dyn crate::HostSink>,
}

impl EventSink {
    /// Hands one event to whoever is listening; the only way it fails is
    /// that nobody is, which means the same thing whatever the event was.
    pub(crate) fn unbounded_send(&self, event: ConnEvent) -> Result<(), crate::SinkClosed> {
        self.events.send(HostEvent {
            host: self.host,
            event,
        })
    }
}

pub enum ConnEvent {
    /// The host is up.
    Ready,
    /// Several events in order, delivered as one. An agent host sends them
    /// separately; a test that stands for one stands for the batch.
    Many(Vec<ConnEvent>),
    ServerError(String),
    Recovering(std::time::Duration),
    Recovered,
    Disconnected(String),
    /// The host does not trust this device yet: `command`, run on the host,
    /// enrolls it, and the window tries again within half a minute.
    Unenrolled {
        command: String,
    },
}

/// How to dial another stream to the agent host: another bi-stream on the
/// already authenticated iroh connection. Set by the IO task once
/// connected.
pub(crate) type ChannelDialer = rho_rpc::Dialer;

/// One call of the machine on a stream of its own. A refusal is an error.
async fn dial_call<C: rho_rpc::protocol::Call>(
    dialer: ChannelDialer,
    call: C,
) -> anyhow::Result<C::Reply> {
    let mut stream = dialer.open(C::PRIORITY).await?;
    rho_rpc::protocol::call(&mut stream, call).await
}

async fn dial_gui_telemetry(dialer: ChannelDialer, snapshot: Vec<u8>) -> anyhow::Result<String> {
    anyhow::ensure!(
        snapshot.len() <= crate::protocol::MAX_GUI_TELEMETRY_BYTES,
        "GUI telemetry snapshot is too large"
    );
    dial_call(dialer, protocol::GuiTelemetryUpload { snapshot }).await
}

pub struct Connection {
    link: Link,
    /// Cuts the wait before the next dial short.
    retry: Arc<tokio::sync::Notify>,
    /// Dropping this aborts the IO task, tearing the connection down with the
    /// workspace. A test connection has none.
    _io_task: Option<AbortOnDrop<()>>,
}

/// Where a host's work runs and how it reaches the host: the way a client
/// crate opens streams of its own on the host's connection. Clonable, and
/// valid across reconnects: each use dials whatever connection is up.
#[derive(Clone)]
pub struct Link {
    runtime: Option<tokio::runtime::Handle>,
    /// `None` until the IO task connects; streams cannot open earlier.
    dialer: Arc<Mutex<Option<ChannelDialer>>>,
}

impl Link {
    /// A link to no host, for work whose host has gone: every use reports
    /// the same "not connected" as a dropped connection.
    pub fn detached() -> Self {
        Self {
            runtime: None,
            dialer: Arc::default(),
        }
    }

    /// Puts the host in this process: every stream opened from now on is
    /// handed over here, far end first, for a test to answer as the host
    /// would. A supervisor's next connection takes the host back.
    pub fn connect_in_process(&self) -> tokio::sync::mpsc::UnboundedReceiver<rho_rpc::Stream> {
        let (streams, opened) = tokio::sync::mpsc::unbounded_channel();
        *self.dialer.lock().unwrap() = Some(ChannelDialer::InProcess(streams));
        opened
    }

    /// Stores a GUI performance snapshot on the host, which answers where
    /// it put it.
    pub fn upload_gui_telemetry(
        &self,
        snapshot: Vec<u8>,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static {
        self.run(|dialer| dial_gui_telemetry(dialer, snapshot))
    }

    /// Makes one call of the machine on a stream of its own. The answer
    /// needs no particular executor; a refusal is an error.
    pub fn call<C: rho_rpc::protocol::Call>(
        &self,
        call: C,
    ) -> impl Future<Output = anyhow::Result<C::Reply>> + Send + 'static + use<C> {
        self.run(|dialer| dial_call(dialer, call))
    }

    /// Runs `work` against the host on the connection's runtime. The answer
    /// needs no particular executor; dropping it cancels the work. An
    /// in-process host's work runs in the answer itself, on whatever
    /// executor polls it: nothing in it needs a runtime, and a test steps
    /// it with everything else.
    pub fn run<T, F, W>(
        &self,
        work: W,
    ) -> impl Future<Output = anyhow::Result<T>> + Send + 'static + use<T, F, W>
    where
        W: FnOnce(Dialer) -> F,
        F: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let dialer = self.dialer.lock().unwrap().clone();
        if let Some(dialer @ ChannelDialer::InProcess(_)) = dialer {
            return futures::future::Either::Left(work(dialer));
        }
        let task = match (&self.runtime, dialer) {
            (Some(runtime), Some(dialer)) => Some(AbortOnDrop(runtime.spawn(work(dialer)))),
            _ => None,
        };
        futures::future::Either::Right(async move {
            let Some(mut task) = task else {
                anyhow::bail!("not connected to an agent host");
            };
            (&mut task.0)
                .await
                .map_err(|error| anyhow::anyhow!("connection task failed: {error}"))?
        })
    }
}

/// A task that ends when its owner lets go of it.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Connection {
    /// The way a client crate reaches this host.
    pub fn link(&self) -> Link {
        self.link.clone()
    }

    /// Dials again now, rather than after the wait since the last try.
    pub fn retry_now(&self) {
        self.retry.notify_one();
    }
}

/// Whether `spawn` starts the supervisor that dials the target and keeps
/// reconnecting. A test binary gets none: nothing in a test should reach a
/// socket, and a supervisor that outlives the test's tokio runtime is
/// polled while that runtime shuts down, which panics inside tokio's timer
/// ("A Tokio 1.x context was found, but it is being shutdown").
///
/// `cfg!(test)` alone said this wrong: it is true only inside this crate's
/// own tests, so every downstream test binary, rho-gui's suite among them,
/// got a live supervisor dialing a socket that is not there.
/// `test-support` is the feature those binaries turn on, so it is the one
/// that answers here.
pub fn supervises() -> bool {
    !cfg!(test) && !cfg!(feature = "test-support")
}

/// Attaches one agent host. Its events join `events`, tagged with `host`, so
/// several agent hosts feed the workspace through a single ordered stream.
/// `streams` is handed the host's [`Link`] and returns the streams that
/// open on every connection. The connection's work runs on `runtime`.
pub fn spawn(
    host: HostId,
    target: AttachTarget,
    events: Arc<dyn crate::HostSink>,
    streams: impl FnOnce(Link) -> Vec<Arc<dyn HostStream>>,
    runtime: &tokio::runtime::Handle,
) -> Connection {
    let events = EventSink { host, events };
    let link = Link {
        runtime: Some(runtime.clone()),
        dialer: Arc::default(),
    };
    let streams = streams(link.clone());
    let dialer = link.dialer.clone();
    let retry = Arc::new(tokio::sync::Notify::new());
    let io_task = supervises().then(|| {
        AbortOnDrop(runtime.spawn(supervise(target, events, streams, dialer, retry.clone())))
    });
    Connection {
        link,
        retry,
        _io_task: io_task,
    }
}

async fn supervise(
    target: AttachTarget,
    events: EventSink,
    streams: Vec<Arc<dyn HostStream>>,
    dialer: Arc<Mutex<Option<ChannelDialer>>>,
    retry: Arc<tokio::sync::Notify>,
) {
    let mut delay = INITIAL_RECONNECT_DELAY;
    // Since when the host has been away, while it is.
    let mut dropped: Option<tokio::time::Instant> = None;
    loop {
        if closing() {
            break;
        }
        let mut connected = false;
        let result = run(target.clone(), &events, &streams, &dialer, &mut connected).await;
        *dialer.lock().unwrap() = None;
        if events.events.is_closed() {
            break;
        }
        if connected {
            delay = INITIAL_RECONNECT_DELAY;
            dropped = None;
        }
        // Enrollment already reported its code and waited for approval.
        // Keep that reason visible and do not add reconnect backoff.
        if result
            .as_ref()
            .is_err_and(|error| error.is::<EnrollmentPending>())
        {
            dropped = None;
            delay = INITIAL_RECONNECT_DELAY;
            continue;
        }
        // The user hears what happened in their words; how the transport
        // failed is the log's.
        let detail = match &result {
            Err(error) => format!("{error:#}"),
            Ok(()) => "the connection closed".to_owned(),
        };
        tracing::warn!(host = %target.describe(), %detail, "agent host connection ended");
        let reason = match connected {
            true => "connection lost",
            false => "cannot reach the host",
        };
        let first = dropped.is_none();
        let since = *dropped.get_or_insert_with(tokio::time::Instant::now);
        if (first
            && events
                .unbounded_send(ConnEvent::Disconnected(reason.to_owned()))
                .is_err())
            || events
                .unbounded_send(ConnEvent::Recovering(since.elapsed()))
                .is_err()
        {
            break;
        }
        // The waiter is made before the flag is read: a `close` between the
        // two would otherwise notify nobody and leave this sleeping into
        // the runtime's shutdown.
        let closed = CLOSED.notified();
        if closing() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = retry.notified() => {
                delay = INITIAL_RECONNECT_DELAY;
                continue;
            }
            _ = closed => break,
        }
        delay = next_reconnect_delay(delay);
    }
}

async fn abort_tasks<T: 'static>(tasks: &mut tokio::task::JoinSet<T>) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn run(
    target: AttachTarget,
    events: &EventSink,
    streams: &[Arc<dyn HostStream>],
    dialer: &Mutex<Option<ChannelDialer>>,
    connected: &mut bool,
) -> anyhow::Result<()> {
    // Held for the whole connection: the endpoint is what the iroh
    // connection runs on.
    let (connection, _endpoint) = match target {
        AttachTarget::Local { runtime_dir } => connect_local(&runtime_dir).await?,
        AttachTarget::Iroh {
            endpoint_id,
            access,
        } => connect_iroh(endpoint_id, &access, events).await?,
    };
    let media = rho_rpc::media::Mux::new(connection.clone());
    let uni = media.clone();
    tokio::spawn(async move { uni.receive_uni().await });
    let bi = media.clone();
    tokio::spawn(async move { bi.receive_bi().await });
    *dialer.lock().unwrap() = Some(ChannelDialer::Iroh {
        connection: connection.clone(),
        media,
    });
    if events.unbounded_send(ConnEvent::Ready).is_err() {
        return Ok(());
    }
    *connected = true;
    if events.unbounded_send(ConnEvent::Recovered).is_err() {
        return Ok(());
    }

    let streams_dialer = dialer
        .lock()
        .unwrap()
        .clone()
        .context("connected without a dialer")?;
    // The first stream to end, however it ends, takes the connection down
    // and brings every stream up again together.
    let mut stream_tasks = tokio::task::JoinSet::new();
    for host_stream in streams {
        let name = host_stream.name();
        let run = host_stream.run(streams_dialer.clone());
        stream_tasks.spawn(async move { (name, run.await) });
    }

    let health_task = {
        let events = events.clone();
        tokio::spawn(async move {
            const RECOVERY_NOTICE_AFTER: std::time::Duration = std::time::Duration::from_secs(10);
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            let mut received = connection.stats().authenticated_packets;
            let mut last_received = tokio::time::Instant::now();
            let mut recovering = false;
            loop {
                interval.tick().await;
                let current = connection.stats().authenticated_packets;
                if current != received {
                    received = current;
                    last_received = tokio::time::Instant::now();
                }
                let elapsed = last_received.elapsed();
                if elapsed >= RECOVERY_NOTICE_AFTER {
                    recovering = true;
                    if events
                        .unbounded_send(ConnEvent::Recovering(elapsed))
                        .is_err()
                    {
                        break;
                    }
                } else if recovering {
                    recovering = false;
                    if events.unbounded_send(ConnEvent::Recovered).is_err() {
                        break;
                    }
                }
            }
        })
    };

    let ended = stream_tasks.join_next().await;
    abort_tasks(&mut stream_tasks).await;
    health_task.abort();
    let _ = health_task.await;
    Err(match ended {
        Some(Ok((name, Ok(())))) => anyhow::anyhow!("agent host {name} stream closed"),
        Some(Ok((name, Err(error)))) => error.context(format!("agent host {name} stream")),
        Some(Err(error)) => anyhow::anyhow!("agent host stream task failed: {error}"),
        None => anyhow::anyhow!("agent host connection has no streams"),
    })
}

/// The client's iroh identity, bound once and shared by every attached
/// agent host. One identity means one key for the user to recognize across
/// hosts, and each host still enrolls it separately over its own SSH login.
static CLIENT_ENDPOINT: tokio::sync::OnceCell<iroh::Endpoint> = tokio::sync::OnceCell::const_new();

async fn client_endpoint() -> anyhow::Result<iroh::Endpoint> {
    CLIENT_ENDPOINT
        .get_or_try_init(rho_rpc::bind_ephemeral_iroh_client)
        .await
        .cloned()
}

#[derive(Debug)]
struct EnrollmentPending;

impl std::fmt::Display for EnrollmentPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("iroh enrollment awaiting approval")
    }
}
impl std::error::Error for EnrollmentPending {}

/// The host in `runtime_dir`, over its IPC socket: the way it knows this
/// process is the user's own.
async fn connect_local(
    runtime_dir: &std::path::Path,
) -> anyhow::Result<(iroh::endpoint::Connection, iroh::Endpoint)> {
    let paths = rho_rpc::protocol::RuntimePaths::new(Some(runtime_dir))?;
    let host = rho_rpc::local_host(&paths)?;
    let endpoint = client_endpoint().await?;
    let connection =
        rho_rpc::connect_host(&endpoint, host, rho_rpc::protocol::IROH_ALPN, Some(&paths)).await?;
    match rho_rpc::authenticate_iroh_client(&connection, endpoint.id(), None).await? {
        rho_iroh_auth::ClientAuthResult::Approved => Ok((connection, endpoint)),
        other => anyhow::bail!("the local agent host did not trust this process: {other:?}"),
    }
}

async fn connect_iroh(
    host_id: iroh::EndpointId,
    access: &IrohAccess,
    events: &EventSink,
) -> anyhow::Result<(iroh::endpoint::Connection, iroh::Endpoint)> {
    let endpoint = match access {
        IrohAccess::Ssh {
            ssh_destination,
            remote_rho,
        } => {
            let endpoint = client_endpoint().await?;
            tracing::info!(
                destination = ssh_destination,
                "trusting ephemeral iroh client over SSH"
            );
            trust_in_memory_over_ssh(ssh_destination, remote_rho, endpoint.id()).await?;
            tracing::info!(
                destination = ssh_destination,
                "ephemeral iroh client trusted over SSH"
            );
            endpoint
        }
        IrohAccess::Fido | IrohAccess::Link { .. } => crate::fido::endpoint()
            .await
            .context("security key identity")?,
    };
    let pairing = match access {
        IrohAccess::Link { key } => Some(key),
        _ => None,
    };
    let connection =
        rho_rpc::connect_host(&endpoint, host_id, rho_rpc::protocol::IROH_ALPN, None).await?;
    match rho_rpc::authenticate_iroh_client(&connection, endpoint.id(), pairing).await? {
        rho_iroh_auth::ClientAuthResult::Approved => {}
        rho_iroh_auth::ClientAuthResult::EnrollmentRequired(code)
            if matches!(access, IrohAccess::Fido | IrohAccess::Link { .. }) =>
        {
            events.unbounded_send(ConnEvent::Unenrolled {
                command: format!("rho iroh approve --role client {code}"),
            })?;
            // The server closes after our auth ack. Keep the per-connection
            // code visible long enough for approval, without racing
            // a new code.
            let closed = CLOSED.notified();
            let retry_at = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            if !closing() {
                tokio::select! {
                    _ = async {
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), connection.closed()).await;
                        tokio::time::sleep_until(retry_at).await;
                    } => {}
                    _ = closed => {}
                }
            }
            return Err(EnrollmentPending.into());
        }
        _ => anyhow::bail!("agent host did not approve iroh client"),
    }
    Ok((connection, endpoint))
}

async fn trust_in_memory_over_ssh(
    destination: &str,
    remote_rho: &str,
    endpoint_id: iroh::EndpointId,
) -> anyhow::Result<()> {
    anyhow::ensure!(!destination.starts_with('-'), "invalid SSH destination");
    anyhow::ensure!(
        is_safe_remote_executable(remote_rho),
        "invalid remote rho executable path"
    );
    // An endpoint id's text form has a fixed safe alphabet even though
    // OpenSSH sends the remote argv through the login shell.
    let endpoint_id = rho_agent_types::EndpointId(*endpoint_id.as_bytes()).to_string();
    let status = tokio::process::Command::new("ssh")
        .arg("--")
        .arg(destination)
        .args([remote_rho, "iroh", "trust-in-memory", &endpoint_id])
        .status()
        .await
        .context("run SSH enrollment approval")?;
    anyhow::ensure!(
        status.success(),
        "SSH enrollment approval failed with {status}"
    );
    Ok(())
}

fn is_safe_remote_executable(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'+' | b'-')
        })
}

#[cfg(test)]
mod shutdown_tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A sink that takes everything and never says it is closed, so the
    /// supervisor's own "nobody is listening" exit cannot be what ends the
    /// loop here.
    /// It counts the dials that failed, by the `Recovering` that follows
    /// each.
    #[derive(Default)]
    struct Listening(std::sync::atomic::AtomicUsize);

    impl crate::HostSink for Listening {
        fn send(&self, event: crate::HostEvent) -> Result<(), crate::SinkClosed> {
            if matches!(event.event, ConnEvent::Recovering(_)) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        }

        fn is_closed(&self) -> bool {
            false
        }
    }

    /// Quit has to end the supervisor while the runtime is still there.
    /// A supervisor waiting out its reconnect delay is inside
    /// `tokio::time::sleep`, and a sleep polled after its runtime starts
    /// shutting down panics in tokio's timer, which is what a real quit was
    /// doing on a worker thread.
    ///
    /// The dial fails at once (no host ever ran there), so the loop is
    /// in the delay within a few milliseconds; `retry_now` dials again
    /// inside it, and `close` has to bring it out well before the 500ms
    /// that delay would otherwise take. One test, because `close` is
    /// process-wide.
    #[test]
    fn retry_and_close_cut_a_supervisors_reconnect_delay_short() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let runtime_dir = directory.path().join("nothing");

        let listening = Arc::new(Listening::default());
        let events = EventSink {
            host: crate::HostId(0),
            events: listening.clone(),
        };
        let retry = Arc::new(tokio::sync::Notify::new());
        let supervisor = supervise(
            crate::AttachTarget::Local { runtime_dir },
            events,
            Vec::new(),
            Arc::new(Mutex::new(None)),
            retry.clone(),
        );
        let failed = || listening.0.load(std::sync::atomic::Ordering::SeqCst);

        let ended = runtime.block_on(async {
            let task = tokio::spawn(supervisor);
            // Long enough for the failed dial and the events that follow it,
            // short enough to be inside the 500ms delay.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(failed(), 1, "one dial, then the delay");
            retry.notify_one();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(failed(), 2, "the retry dialed again inside the delay");
            close();
            tokio::time::timeout(std::time::Duration::from_millis(200), task).await
        });
        reopen();

        assert!(
            ended.is_ok(),
            "the supervisor ends when quit closes it, rather than sleeping on"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{INITIAL_RECONNECT_DELAY, MAX_RECONNECT_DELAY, abort_tasks, next_reconnect_delay};

    #[test]
    fn reconnect_backoff_caps_at_ten_seconds() {
        let mut delay = INITIAL_RECONNECT_DELAY;
        for _ in 0..10 {
            delay = next_reconnect_delay(delay);
        }
        assert_eq!(delay, MAX_RECONNECT_DELAY);
        assert_eq!(next_reconnect_delay(delay), MAX_RECONNECT_DELAY);
    }

    #[test]
    fn session_teardown_aborts_and_awaits_child_tasks() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
                struct Dropped(Arc<std::sync::atomic::AtomicBool>);
                impl Drop for Dropped {
                    fn drop(&mut self) {
                        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                let guard = Dropped(Arc::clone(&dropped));
                let mut tasks = tokio::task::JoinSet::new();
                tasks.spawn(async move {
                    let _guard = guard;
                    futures::future::pending::<()>().await;
                });
                tokio::task::yield_now().await;

                abort_tasks(&mut tasks).await;

                assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
                assert!(tasks.is_empty());
            });
    }
}
