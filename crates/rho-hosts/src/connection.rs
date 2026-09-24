//! Daemon connection: an IO task on the tokio runtime the caller hands in,
//! bridged to the reader through sinks. What the control stream pushes
//! becomes [`ConnEvent`]s on a futures channel the workspace awaits (no
//! polling); every request is a stream of its own, answered once.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use rho_agent_host_proto::client::Client;
use rho_agent_host_proto::control::{
    ClientFrame as ControlClientFrame, ServerFrame as ControlFrame,
};
use rho_agent_host_proto::host::{Open as HostOpen, Reply, Request};
use rho_agent_host_proto::{
    GitProvided, GitService, GitTransportRequest, Open, Opened, read_frame, write_frame,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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

use crate::{AttachTarget, Dialer, HostId, HostStream};

/// Owns the transport pumps for a dedicated stream. Dropping it cancels both
/// directions on every target.
pub type ChannelTask = rho_rpc::ChannelTask;

/// A connection event tagged with the daemon it came from. Every attached
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
    DesktopSessions(Vec<rho_agent_host_proto::DesktopSession>),
    Ready {
        auth: rho_agent_host_proto::AuthState,
        machine_seed: u64,
        agent_counter: u64,
    },
    AuthState(rho_agent_host_proto::AuthState),
    /// Several events in order, delivered as one. A daemon sends them
    /// separately; a test that stands for one stands for the batch.
    Many(Vec<ConnEvent>),
    ServerError(String),
    Recovering(std::time::Duration),
    Recovered,
    Disconnected(String),
    GitTransportApproval {
        request_id: u64,
        prompt: String,
        response: tokio::sync::oneshot::Sender<GitApprovalDecision>,
    },
    GitTransportDone {
        request_id: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitApprovalDecision {
    Allow,
    Deny,
    Done,
}

/// How to dial an extra workspace-file stream to the daemon: locally a
/// second Unix connection, remotely another bi-stream on the already
/// authenticated iroh connection. Set by the IO task once connected.
pub(crate) type ChannelDialer = rho_rpc::Dialer;

pub(crate) async fn dial_realtime(
    dialer: ChannelDialer,
    offer_sdp: String,
) -> anyhow::Result<crate::realtime_client::RealtimeChannel> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(&mut stream, &Open::Host(HostOpen::Realtime { offer_sdp })).await?;
    let answer_sdp = match read_frame(&mut stream).await? {
        rho_agent_host_proto::realtime::Opened::Answer { answer_sdp } => answer_sdp,
        rho_agent_host_proto::realtime::Opened::Refused { reason } => anyhow::bail!("{reason}"),
    };
    let channel = stream.into_channel(rho_rpc::ChannelConfig {
        tx_limit: rho_agent_host_proto::MAX_FRAME_LEN,
        rx_limit: rho_agent_host_proto::MAX_FRAME_LEN,
        tx_capacity: 32,
        rx_capacity: 32,
    });
    let (requests, replies, transport) = channel.into_parts();
    Ok(crate::realtime_client::RealtimeChannel {
        answer_sdp,
        requests,
        replies,
        _transport: transport,
    })
}

/// One request of the machine on a stream of its own. A refusal is an
/// error.
async fn dial_request(dialer: ChannelDialer, request: Request) -> anyhow::Result<Reply> {
    // Bulk answers wait behind interactive traffic; everything else is
    // someone waiting on a keypress, as urgent as the control stream.
    let priority = match &request {
        Request::GuiTelemetryUpload { .. } => None,
        _ => Some(1),
    };
    let mut stream = dialer.open(priority).await?;
    write_frame(&mut stream, &Open::Host(HostOpen::Request(request))).await?;
    match read_frame(&mut stream).await? {
        Reply::Failed { reason } => anyhow::bail!(reason),
        reply => Ok(reply),
    }
}

async fn dial_stream(dialer: ChannelDialer) -> anyhow::Result<rho_rpc::Stream> {
    // Interactive streams outrank the control session (priority 1).
    dialer.open(Some(50)).await
}

async fn dial_gui_telemetry(dialer: ChannelDialer, snapshot: Vec<u8>) -> anyhow::Result<String> {
    anyhow::ensure!(
        snapshot.len() <= rho_agent_host_proto::MAX_GUI_TELEMETRY_BYTES,
        "GUI telemetry snapshot is too large"
    );
    match dial_request(dialer, Request::GuiTelemetryUpload { snapshot }).await? {
        Reply::GuiTelemetryStored { path } => Ok(path),
        _ => anyhow::bail!("unexpected reply to a GUI telemetry upload"),
    }
}

pub struct Connection {
    link: Link,
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

    /// Runs `work` against the host on the connection's runtime. The answer
    /// needs no particular executor; dropping it cancels the work.
    pub fn run<T, F>(
        &self,
        work: impl FnOnce(Dialer) -> F,
    ) -> impl Future<Output = anyhow::Result<T>> + Send + 'static
    where
        F: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let dialer = self.dialer.lock().unwrap().clone();
        let task = match (&self.runtime, dialer) {
            (Some(runtime), Some(dialer)) => Some(AbortOnDrop(runtime.spawn(work(dialer)))),
            _ => None,
        };
        async move {
            let Some(mut task) = task else {
                anyhow::bail!("not connected to rho-daemon");
            };
            (&mut task.0)
                .await
                .map_err(|error| anyhow::anyhow!("connection task failed: {error}"))?
        }
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
    pub fn open_wayland(
        &self,
        agent: String,
        session: String,
    ) -> impl Future<Output = anyhow::Result<crate::wayland::Viewer>> + Send + 'static {
        let started = std::time::Instant::now();
        self.link.run(move |dialer| async move {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                "desktop IO task started"
            );
            let ChannelDialer::Iroh { connection, media } = dialer else {
                anyhow::bail!("the live Wayland viewer requires an Iroh host");
            };
            open_wayland_stream(connection, media, agent, session, started).await
        })
    }

    pub fn upload_gui_telemetry(
        &self,
        snapshot: Vec<u8>,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static {
        self.link.run(|dialer| dial_gui_telemetry(dialer, snapshot))
    }

    pub fn start_native_realtime(
        &self,
        stop: tokio::sync::oneshot::Receiver<()>,
        input_muted: tokio::sync::watch::Receiver<bool>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send + 'static {
        self.link
            .run(|dialer| crate::realtime_client::run_native(dialer, stop, input_muted))
    }

    /// The way a client crate reaches this host.
    pub fn link(&self) -> Link {
        self.link.clone()
    }

    /// Makes one request of the machine on a stream of its own. The answer
    /// needs no particular executor; a refusal is an error.
    pub fn request(
        &self,
        request: Request,
    ) -> impl Future<Output = anyhow::Result<Reply>> + Send + 'static {
        self.link.run(|dialer| dial_request(dialer, request))
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

/// Attaches one daemon. Its events join `events`, tagged with `host`, so
/// several daemons feed the workspace through a single ordered stream.
/// `streams` is handed the host's [`Link`] and returns the streams that
/// open beside the control stream on every connection. The connection's
/// work runs on `runtime`.
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
    let io_task = supervises()
        .then(|| AbortOnDrop(runtime.spawn(supervise(target, events, streams, dialer))));
    Connection {
        link,
        _io_task: io_task,
    }
}

async fn supervise(
    target: AttachTarget,
    events: EventSink,
    streams: Vec<Arc<dyn HostStream>>,
    dialer: Arc<Mutex<Option<ChannelDialer>>>,
) {
    let mut delay = INITIAL_RECONNECT_DELAY;
    let mut reconnecting = false;
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
            reconnecting = false;
        }
        let reason = result
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_else(|| "daemon connection closed".to_owned());
        if (!reconnecting
            && events
                .unbounded_send(ConnEvent::Disconnected(reason))
                .is_err())
            || events.unbounded_send(ConnEvent::Recovering(delay)).is_err()
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
            _ = closed => break,
        }
        delay = next_reconnect_delay(delay);
        reconnecting = true;
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
    let (mut stream, agent_connection, _endpoint) = match target {
        AttachTarget::Unix(socket_path) => {
            let client = Client::connect(&socket_path)
                .await
                .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
            *dialer.lock().unwrap() = Some(ChannelDialer::Unix(socket_path));
            (client.into_stream(), None, None)
        }
        AttachTarget::Iroh {
            endpoint_id,
            ssh_destination,
            remote_rho,
        } => {
            let (stream, connection, endpoint) =
                connect_iroh(endpoint_id, &ssh_destination, &remote_rho).await?;
            let media = rho_rpc::media::Mux::new(connection.clone());
            let uni = media.clone();
            tokio::spawn(async move { uni.receive_uni().await });
            let bi = media.clone();
            tokio::spawn(async move { bi.receive_bi().await });
            *dialer.lock().unwrap() = Some(ChannelDialer::Iroh {
                connection: connection.clone(),
                media,
            });
            (stream, Some(connection), Some(endpoint))
        }
    };
    write_frame(&mut stream, &Open::Host(HostOpen::Control)).await?;
    let frame: ControlFrame = read_frame(&mut stream).await?;
    let ControlFrame::Ready {
        auth,
        machine_seed,
        agent_counter,
    } = frame
    else {
        anyhow::bail!("rho daemon did not send ready message");
    };
    if events
        .unbounded_send(ConnEvent::Ready {
            auth,
            machine_seed,
            agent_counter,
        })
        .is_err()
    {
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
    write_frame(&mut stream, &ControlClientFrame::ProvideGitTransport).await?;

    let health_connection = agent_connection;
    let git_transport_limit = Arc::new(tokio::sync::Semaphore::new(1));
    let git_requests = Arc::new(Mutex::new(
        HashMap::<u64, tokio::sync::watch::Sender<bool>>::new(),
    ));

    // The write half is held, unused, so the stream stays open.
    let (mut reader, _writer) = tokio::io::split(stream);
    let health_task = health_connection.map(|connection| {
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
    });
    let mut git_provider_tasks = tokio::task::JoinSet::new();

    let read_error = loop {
        let frame: ControlFrame = tokio::select! {
            Some(result) = stream_tasks.join_next() => {
                break Some(match result {
                    Ok((name, Ok(()))) => anyhow::anyhow!("daemon {name} stream closed"),
                    Ok((name, Err(error))) => error.context(format!("daemon {name} stream")),
                    Err(error) => anyhow::anyhow!("daemon stream task failed: {error}"),
                });
            }
            result = read_frame(&mut reader) => match result {
                Ok(frame) => frame,
                Err(error) => break Some(error),
            },
        };
        let event = match frame {
            ControlFrame::DesktopSessions { sessions } => {
                Some(ConnEvent::DesktopSessions(sessions))
            }
            ControlFrame::Ready {
                auth,
                machine_seed,
                agent_counter,
            } => Some(ConnEvent::Ready {
                auth,
                machine_seed,
                agent_counter,
            }),
            ControlFrame::AuthState { auth } => Some(ConnEvent::AuthState(auth)),
            ControlFrame::GitTransportRequested {
                request_id,
                provider_id,
                request,
            } => {
                let events = events.clone();
                let provider_dialer = dialer.lock().unwrap().clone();
                let git_transport_limit = git_transport_limit.clone();
                let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
                git_requests.lock().unwrap().insert(request_id, done_tx);
                let git_requests = git_requests.clone();
                git_provider_tasks.spawn(async move {
                    let result = async {
                        let _permit = tokio::select! {
                            permit = git_transport_limit.acquire_owned() => {
                                permit.context("Git transport provider closed")?
                            }
                            _ = done_rx.changed() => return Ok(()),
                        };
                        let provider_dialer =
                            provider_dialer.context("not connected to rho daemon")?;
                        run_git_transport_provider(
                            provider_dialer,
                            request_id,
                            provider_id,
                            request,
                            events.clone(),
                        )
                        .await
                    }
                    .await;
                    if let Err(error) = result {
                        let _ = events.unbounded_send(ConnEvent::ServerError(format!(
                            "SSH Git transport failed: {error:#}"
                        )));
                    }
                    let _ = events.unbounded_send(ConnEvent::GitTransportDone { request_id });
                    git_requests.lock().unwrap().remove(&request_id);
                });
                None
            }
            ControlFrame::GitTransportDone { request_id } => {
                if let Some(done) = git_requests.lock().unwrap().remove(&request_id) {
                    done.send_replace(true);
                }
                Some(ConnEvent::GitTransportDone { request_id })
            }
        };
        if let Some(event) = event
            && events.unbounded_send(event).is_err()
        {
            break None;
        }
    };
    abort_tasks(&mut stream_tasks).await;
    if let Some(task) = health_task {
        task.abort();
        let _ = task.await;
    }
    abort_tasks(&mut git_provider_tasks).await;
    git_requests.lock().unwrap().clear();
    match read_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn request_git_approval(
    events: &EventSink,
    request_id: u64,
    prompt: String,
) -> anyhow::Result<GitApprovalDecision> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    events
        .unbounded_send(ConnEvent::GitTransportApproval {
            request_id,
            prompt,
            response: tx,
        })
        .map_err(|_| anyhow::anyhow!("GUI closed before Git transport approval"))?;
    tokio::time::timeout(std::time::Duration::from_secs(60), rx)
        .await
        .context("Git transport approval timed out after 60 seconds")?
        .context("GUI closed the Git transport approval prompt")
}

async fn run_git_transport_provider(
    dialer: ChannelDialer,
    request_id: u64,
    provider_id: u64,
    request: GitTransportRequest,
    events: EventSink,
) -> anyhow::Result<()> {
    if let Err(error) = validate_git_transport_request(&request) {
        report_git_transport_decision(dialer, request_id, provider_id, false).await?;
        return Err(error);
    }
    let prompt = match request.service {
        GitService::UploadPack => format!(
            "Fetch via SSH from {}:{}/{}? [shift-Y/N]",
            display_field(&request.host),
            request.port,
            display_field(&request.repository),
        ),
        GitService::ReceivePack => git_push_prompt(
            &request,
            request
                .planned_refs
                .as_deref()
                .context("SSH Git push is missing its destination ref plan")?,
        ),
    };
    match request_git_approval(&events, request_id, prompt).await? {
        GitApprovalDecision::Allow => {}
        GitApprovalDecision::Deny => {
            report_git_transport_decision(dialer, request_id, provider_id, false).await?;
            return Ok(());
        }
        GitApprovalDecision::Done => return Ok(()),
    }

    let Some(mut stream) = open_git_transport_provider(dialer, request_id, provider_id).await?
    else {
        return Ok(());
    };
    let remote_command = format!(
        "{} '{}'",
        match request.service {
            GitService::UploadPack => "git-upload-pack",
            GitService::ReceivePack => "git-receive-pack",
        },
        request.repository
    );
    let mut child = tokio::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes"])
        .args(["-o", "ClearAllForwardings=yes"])
        .args(["-o", "PermitLocalCommand=no"])
        .args(["-o", "ControlMaster=no"])
        .arg("-p")
        .arg(request.port.to_string())
        .arg("-l")
        .arg(&request.user)
        .arg("--")
        .arg(&request.host)
        .arg(remote_command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("launch local OpenSSH")?;
    let mut ssh_stdin = child.stdin.take().context("OpenSSH stdin unavailable")?;
    let mut ssh_stdout = child.stdout.take().context("OpenSSH stdout unavailable")?;
    let ssh_stderr = child.stderr.take().context("OpenSSH stderr unavailable")?;
    let (mut transport_read, mut transport_write) = tokio::io::split(&mut stream);

    let input = async {
        if request.service == GitService::ReceivePack {
            copy_planned_receive_pack(
                &mut transport_read,
                &mut ssh_stdin,
                request
                    .planned_refs
                    .as_deref()
                    .context("SSH Git push is missing its destination ref plan")?,
            )
            .await?;
        } else {
            rho_rpc::copy_flush(&mut transport_read, &mut ssh_stdin).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let output = async {
        rho_rpc::copy_flush(&mut ssh_stdout, &mut transport_write).await?;
        Ok::<(), anyhow::Error>(())
    };
    let stderr = async {
        const MAX_STDERR: usize = 64 * 1024;
        let mut bytes = Vec::new();
        ssh_stderr
            .take(MAX_STDERR as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > MAX_STDERR {
            bytes.truncate(MAX_STDERR);
            bytes.extend_from_slice(b"\n[SSH stderr truncated]");
        }
        Ok::<Vec<u8>, anyhow::Error>(bytes)
    };
    let ((), (), stderr) = tokio::try_join!(input, output, stderr)?;
    let status = child.wait().await.context("wait for local OpenSSH")?;
    anyhow::ensure!(
        status.success(),
        "OpenSSH exited with {status}: {}",
        String::from_utf8_lossy(&stderr)
    );
    Ok(())
}

async fn copy_planned_receive_pack<R, W>(
    reader: &mut R,
    writer: &mut W,
    planned_refs: &[String],
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (prefix, commands) = read_receive_pack_prefix(reader).await?;
    anyhow::ensure!(
        receive_pack_refs_match(planned_refs, &commands),
        "Git receive-pack destination refs differ from the approved plan"
    );
    writer.write_all(&prefix).await?;
    rho_rpc::copy_flush(reader, writer).await?;
    Ok(())
}

fn receive_pack_refs_match(
    planned_refs: &[String],
    commands: &octo_types::ReceivePackCommands,
) -> bool {
    if planned_refs.len() != commands.updates.len() {
        return false;
    }
    let planned = planned_refs
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let actual = commands
        .updates
        .iter()
        .map(|update| update.reference.as_str())
        .collect::<BTreeSet<_>>();
    planned.len() == planned_refs.len() && planned == actual
}

fn git_push_prompt(request: &GitTransportRequest, planned_refs: &[String]) -> String {
    let destination = format!(
        "ssh://{}:{}/{}",
        display_field(&request.host),
        request.port,
        display_field(&request.repository)
    );
    let mut prompt = format!("Push via SSH to {destination}:");
    for reference in planned_refs {
        use std::fmt::Write as _;
        let reference = reference
            .strip_prefix("refs/heads/")
            .map(|name| format!("branch {name}"))
            .or_else(|| {
                reference
                    .strip_prefix("refs/tags/")
                    .map(|name| format!("tag {name}"))
            })
            .unwrap_or_else(|| reference.clone());
        let _ = write!(prompt, "\n  {reference}");
    }
    prompt.push_str("\nApprove? [shift-Y/N]");
    prompt
}

async fn report_git_transport_decision(
    dialer: ChannelDialer,
    request_id: u64,
    provider_id: u64,
    claim: bool,
) -> anyhow::Result<()> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(
        &mut stream,
        &Open::Host(HostOpen::GitProvide {
            request_id,
            provider_id,
            claim,
        }),
    )
    .await?;
    let _: GitProvided = read_frame(&mut stream).await?;
    Ok(())
}

async fn open_git_transport_provider(
    dialer: ChannelDialer,
    request_id: u64,
    provider_id: u64,
) -> anyhow::Result<Option<rho_rpc::Stream>> {
    let mut stream = dial_stream(dialer).await?;
    write_frame(
        &mut stream,
        &Open::Host(HostOpen::GitProvide {
            request_id,
            provider_id,
            claim: true,
        }),
    )
    .await?;
    match read_frame(&mut stream).await? {
        GitProvided::Ready => Ok(Some(stream)),
        GitProvided::Done => Ok(None),
    }
}

async fn read_receive_pack_prefix<R>(
    reader: &mut R,
) -> anyhow::Result<(Vec<u8>, octo_types::ReceivePackCommands)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut prefix = Vec::new();
    loop {
        let mut chunk = [0_u8; 8192];
        let read = reader.read(&mut chunk).await?;
        anyhow::ensure!(read != 0, "truncated Git receive-pack command list");
        prefix.extend_from_slice(&chunk[..read]);
        match octo_types::parse_receive_pack_commands(&prefix) {
            Ok(Some(commands)) => return Ok((prefix, commands)),
            Ok(None) => {}
            Err(error) => anyhow::bail!(error),
        }
    }
}

fn validate_git_transport_request(request: &GitTransportRequest) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(request.host.as_str(), "github.com" | "git.sr.ht"),
        "invalid SSH Git host"
    );
    anyhow::ensure!(request.port != 0, "invalid SSH Git port");
    anyhow::ensure!(request.user == "git", "invalid SSH Git user");
    anyhow::ensure!(
        octo_types::valid_ssh_repository(&request.host, &request.repository),
        "invalid SSH Git repository path"
    );
    match (&request.service, &request.planned_refs) {
        (GitService::UploadPack, None) => {}
        (GitService::ReceivePack, Some(planned_refs)) => {
            anyhow::ensure!(
                !planned_refs.is_empty(),
                "SSH Git push has an empty ref plan"
            );
            anyhow::ensure!(
                planned_refs.iter().map(String::len).sum::<usize>()
                    <= octo_types::MAX_RECEIVE_PACK_COMMAND_BYTES,
                "SSH Git push ref plan is too large"
            );
            let mut unique = HashSet::new();
            anyhow::ensure!(
                planned_refs.iter().all(|reference| {
                    octo_types::valid_git_ref(reference) && unique.insert(reference)
                }),
                "SSH Git push ref plan is invalid"
            );
        }
        _ => anyhow::bail!("SSH Git transport has an invalid ref plan"),
    }
    Ok(())
}

fn display_field(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
            {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

/// The client's iroh identity, bound once and shared by every attached
/// daemon. One identity means one key for the user to recognize across
/// hosts, and each host still enrolls it separately over its own SSH login.
static CLIENT_ENDPOINT: tokio::sync::OnceCell<iroh::Endpoint> = tokio::sync::OnceCell::const_new();

async fn client_endpoint() -> anyhow::Result<iroh::Endpoint> {
    CLIENT_ENDPOINT
        .get_or_try_init(rho_rpc::bind_ephemeral_iroh_client)
        .await
        .cloned()
}

async fn connect_iroh(
    daemon_id: iroh::EndpointId,
    ssh_destination: &str,
    remote_rho: &str,
) -> anyhow::Result<(rho_rpc::Stream, iroh::endpoint::Connection, iroh::Endpoint)> {
    // The native client's identity intentionally lives only as long as this
    // process. Each daemon can trust it in memory via an existing SSH login.
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
    let connection = endpoint
        .connect(daemon_id, rho_agent_host_proto::IROH_ALPN)
        .await
        .context("connect to daemon over iroh")?;
    anyhow::ensure!(
        rho_rpc::authenticate_iroh_client(&connection, endpoint.id()).await?
            == rho_iroh_auth::ClientAuthResult::Approved,
        "daemon did not approve SSH-trusted iroh client"
    );
    let (send, recv) = connection
        .open_bi()
        .await
        .context("open iroh control stream")?;
    send.set_priority(1)
        .context("set iroh control stream priority")?;
    let stream = rho_rpc::Stream::new(recv, send);
    Ok((stream, connection, endpoint))
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
    // EndpointId's text form has a fixed safe alphabet even though OpenSSH
    // sends the remote argv through the login shell.
    let endpoint_id = endpoint_id.to_string();
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
    struct Listening;

    impl crate::HostSink for Listening {
        fn send(&self, _event: crate::HostEvent) -> Result<(), crate::SinkClosed> {
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
    /// The dial fails at once (nothing listens on the path), so the loop is
    /// in the delay within a few milliseconds; `close` has to bring it out
    /// well before the 500ms that delay would otherwise take.
    #[test]
    fn close_ends_a_supervisor_waiting_out_its_reconnect_delay() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("nothing.sock");

        let events = EventSink {
            host: crate::HostId(0),
            events: Arc::new(Listening),
        };
        let supervisor = supervise(
            crate::AttachTarget::Unix(socket),
            events,
            Vec::new(),
            Arc::new(Mutex::new(None)),
        );

        let ended = runtime.block_on(async {
            let task = tokio::spawn(supervisor);
            // Long enough for the failed dial and the events that follow it,
            // short enough to be inside the 500ms delay.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

    use octo_types::{ReceivePackCommands, RefUpdate};
    use rho_agent_host_proto::{GitService, GitTransportRequest};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::{
        INITIAL_RECONNECT_DELAY, MAX_RECONNECT_DELAY, abort_tasks, copy_planned_receive_pack,
        display_field, git_push_prompt, next_reconnect_delay, receive_pack_refs_match,
        validate_git_transport_request,
    };

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

    fn receive_pack_input(reference: &str, old: &str, new: &str, tail: &[u8]) -> Vec<u8> {
        let command = format!("{old} {new} {reference}\0report-status\n");
        let mut input = format!("{:04x}{command}", command.len() + 4).into_bytes();
        input.extend_from_slice(b"0000");
        input.extend_from_slice(tail);
        input
    }

    #[test]
    fn client_rejects_unsafe_git_transport_fields() {
        let valid = GitTransportRequest {
            host: "github.com".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "team/repo".to_owned(),
            service: GitService::ReceivePack,
            planned_refs: Some(vec!["refs/heads/main".to_owned()]),
        };
        assert!(validate_git_transport_request(&valid).is_ok());
        let mut sourcehut = valid.clone();
        sourcehut.host = "git.sr.ht".to_owned();
        sourcehut.repository = "~alice/project".to_owned();
        assert!(validate_git_transport_request(&sourcehut).is_ok());
        for host in ["github.com", "git.sr.ht"] {
            let mut request = valid.clone();
            request.host = host.to_owned();
            request.user = "root".to_owned();
            assert!(validate_git_transport_request(&request).is_err());
        }
        let mut unknown_host = valid.clone();
        unknown_host.host = "git.example".to_owned();
        assert!(validate_git_transport_request(&unknown_host).is_err());
        for repository in ["team/repo-name", "team/repo.git"] {
            let mut request = valid.clone();
            request.repository = repository.to_owned();
            assert!(validate_git_transport_request(&request).is_ok());
        }
        for repository in ["../repo", "team//repo"] {
            let mut request = valid.clone();
            request.repository = repository.to_owned();
            assert!(validate_git_transport_request(&request).is_err());
        }
        for planned_refs in [
            None,
            Some(Vec::new()),
            Some(vec![
                "refs/heads/main".to_owned(),
                "refs/heads/main".to_owned(),
            ]),
            Some(vec!["refs/heads/../main".to_owned()]),
        ] {
            let mut request = valid.clone();
            request.planned_refs = planned_refs;
            assert!(validate_git_transport_request(&request).is_err());
        }
        let mut fetch = valid;
        fetch.service = GitService::UploadPack;
        assert!(validate_git_transport_request(&fetch).is_err());
        fetch.planned_refs = None;
        assert!(validate_git_transport_request(&fetch).is_ok());
    }

    #[test]
    fn git_prompt_fields_replace_bidi_controls() {
        assert_eq!(display_field("main\u{202e}txt"), "main\u{fffd}txt");
    }

    #[test]
    fn push_prompt_names_destination_refs() {
        let request = GitTransportRequest {
            host: "github.com".to_owned(),
            port: 2222,
            user: "git".to_owned(),
            repository: "acme/repo".to_owned(),
            service: GitService::ReceivePack,
            planned_refs: Some(vec![
                "refs/heads/main".to_owned(),
                "refs/tags/v1".to_owned(),
                "refs/heads/rho/test".to_owned(),
                "refs/notes/review".to_owned(),
            ]),
        };
        let prompt = git_push_prompt(&request, request.planned_refs.as_deref().unwrap());
        assert!(prompt.contains("ssh://github.com:2222/acme/repo"));
        assert!(!prompt.contains("git@"));
        assert!(prompt.contains("branch main"));
        assert!(prompt.contains("tag v1"));
        assert!(prompt.contains("branch rho/test"));
        assert!(prompt.contains("refs/notes/review"));
        assert!(prompt.ends_with("Approve? [shift-Y/N]"));
    }

    #[test]
    fn receive_pack_plan_comparison_ignores_order_and_object_ids() {
        let commands = ReceivePackCommands {
            end: 0,
            updates: vec![
                RefUpdate {
                    old: "1".repeat(40),
                    new: "2".repeat(40),
                    reference: "refs/tags/v1".to_owned(),
                },
                RefUpdate {
                    old: "3".repeat(40),
                    new: "4".repeat(40),
                    reference: "refs/heads/main".to_owned(),
                },
            ],
        };
        assert!(receive_pack_refs_match(
            &["refs/heads/main".to_owned(), "refs/tags/v1".to_owned()],
            &commands
        ));
        assert!(!receive_pack_refs_match(
            &["refs/heads/main".to_owned()],
            &commands
        ));
    }

    #[test]
    fn matching_receive_pack_plan_forwards_exact_bytes() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let old = "1".repeat(40);
                let new = "2".repeat(40);
                let input = receive_pack_input("refs/heads/main", &old, &new, b"PACK tail");
                let (mut client, mut transport) = tokio::io::duplex(4096);
                let (mut ssh, mut remote) = tokio::io::duplex(4096);
                client.write_all(&input).await.unwrap();
                client.shutdown().await.unwrap();
                let copy = tokio::spawn(async move {
                    copy_planned_receive_pack(
                        &mut transport,
                        &mut ssh,
                        &["refs/heads/main".to_owned()],
                    )
                    .await
                });
                let mut received = Vec::new();
                remote.read_to_end(&mut received).await.unwrap();
                copy.await.unwrap().unwrap();
                assert_eq!(received, input);
            });
    }

    #[test]
    fn mismatched_receive_pack_plan_writes_nothing() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let input = receive_pack_input(
                    "refs/heads/rho/test",
                    &"1".repeat(40),
                    &"2".repeat(40),
                    b"PACK tail",
                );
                let (mut client, mut transport) = tokio::io::duplex(4096);
                let (mut ssh, mut remote) = tokio::io::duplex(4096);
                client.write_all(&input).await.unwrap();
                client.shutdown().await.unwrap();
                let result = copy_planned_receive_pack(
                    &mut transport,
                    &mut ssh,
                    &["refs/heads/main".to_owned()],
                )
                .await;
                assert!(result.is_err());
                drop(ssh);
                let mut received = Vec::new();
                remote.read_to_end(&mut received).await.unwrap();
                assert!(received.is_empty());
            });
    }

    #[test]
    fn malformed_receive_pack_writes_nothing() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (mut client, mut transport) = tokio::io::duplex(64);
                let (mut ssh, mut remote) = tokio::io::duplex(64);
                client.write_all(b"0003").await.unwrap();
                client.shutdown().await.unwrap();
                let result = copy_planned_receive_pack(
                    &mut transport,
                    &mut ssh,
                    &["refs/heads/main".to_owned()],
                )
                .await;
                assert!(result.is_err());
                drop(ssh);
                let mut received = Vec::new();
                remote.read_to_end(&mut received).await.unwrap();
                assert!(received.is_empty());
            });
    }
}

async fn open_wayland_stream(
    connection: iroh::endpoint::Connection,
    media: rho_rpc::media::Mux,
    agent: String,
    session: String,
    started: std::time::Instant,
) -> anyhow::Result<crate::wayland::Viewer> {
    static NEXT_MEDIA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_MEDIA.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(desktop_id = id, %agent, %session, "desktop open requested");
    for path in connection.paths().iter().filter(|path| path.is_selected()) {
        tracing::info!(
            desktop_id = id,
            direct = path.is_ip(),
            rtt_ms = path.rtt().as_millis(),
            lost_packets = connection.stats().lost_packets,
            "desktop selected network path"
        );
    }
    let transport = media.session(id)?;
    let (send, recv) = connection.open_bi().await?;
    send.set_priority(100)?;
    let mut stream = rho_rpc::Stream::new(recv, send);
    write_frame(
        &mut stream,
        &Open::Host(HostOpen::Wayland {
            media_id: id,
            agent,
            session,
        }),
    )
    .await?;
    if let Opened::Refused { reason } = read_frame(&mut stream).await? {
        anyhow::bail!("Wayland open refused: {reason}");
    }
    tracing::info!(
        desktop_id = id,
        elapsed_ms = started.elapsed().as_millis(),
        "desktop open acknowledged"
    );
    crate::wayland::open(transport, stream, started, id).await
}
