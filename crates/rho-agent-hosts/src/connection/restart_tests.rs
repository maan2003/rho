//! A host that dies the way a crash kills it, with no close on the wire,
//! and comes back on the same store: the window has to notice in seconds
//! and connect to the new process by itself.
//!
//! The host is this test binary again, run as [`host`] in a process of its
//! own, so that `SIGKILL` takes it down as it takes a real host down.

use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::{FutureExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::ConnEvent;

/// Where the child host keeps its store and runtime directory. Set only on
/// the child.
const HOST_DIRECTORY: &str = "RHO_AGENT_HOSTS_TEST_HOST";
const READY: &str = "rho-agent-hosts test host: ready";

/// The host the tests below start and kill: an authenticated listener in
/// the runtime directory, whose every stream stays open until the client
/// ends it. It runs until it is killed.
#[test]
#[ignore = "the host of the restart tests; runs only as their child process"]
fn host() {
    let Some(directory) = std::env::var_os(HOST_DIRECTORY).map(PathBuf::from) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let paths = rho_rpc::protocol::RuntimePaths::new(Some(directory.join("runtime"))).unwrap();
        let db = rho_db::RhoDb::open(directory.join("host.redb"));
        let (mut listener, _) = rho_rpc::AuthenticatedIrohListener::bind(
            db,
            rho_rpc::protocol::IROH_ALPN,
            b"rho-agent-hosts-test-gccp".to_vec(),
            &paths,
        )
        .await
        .unwrap();
        println!("{READY}");
        while let Some(accepted) = listener.accept().await {
            let Ok(rho_rpc::Accepted::Client(connection)) = accepted else {
                continue;
            };
            tokio::spawn(async move {
                while let Ok((send, recv)) = connection.accept_bi().await {
                    tokio::spawn(async move {
                        let mut stream = rho_rpc::Stream::new(recv, send);
                        let _ = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
                    });
                }
            });
        }
    });
}

/// A host process, killed when dropped.
struct Host(std::process::Child);

impl Host {
    /// Starts the host on `directory`'s store and returns once it listens.
    fn start(directory: &Path) -> Self {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "connection::restart_tests::host",
                "--ignored",
                "--nocapture",
            ])
            .env(HOST_DIRECTORY, directory)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let host = Self(child);
        for line in stdout.lines() {
            if line.unwrap().contains(READY) {
                return host;
            }
        }
        panic!("the test host ended before it listened");
    }

    /// `SIGKILL`: no close reaches the client.
    fn kill(mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One stream that lasts the connection, as every real client stream does:
/// it says a byte so the host sees it, then waits for the end.
struct Held;

impl crate::HostStream for Held {
    fn name(&self) -> &'static str {
        "held"
    }

    fn run(
        &self,
        dialer: crate::Dialer,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
        async move {
            let mut stream = dialer.open(None).await?;
            stream.write_all(b"x").await?;
            stream.flush().await?;
            // The host never writes: this ends when the connection does.
            let mut end = Vec::new();
            stream.read_to_end(&mut end).await?;
            Ok(())
        }
        .boxed()
    }
}

/// What the connection said, and the hosts that keep it.
struct Heard {
    events: futures::channel::mpsc::UnboundedReceiver<crate::HostEvent>,
    _hosts: crate::Hosts,
}

impl Heard {
    /// Waits for `wanted`, up to `within`, and returns what came before it.
    async fn until(
        &mut self,
        within: Duration,
        wanted: impl Fn(&ConnEvent) -> bool,
    ) -> Result<Vec<String>, Vec<String>> {
        let mut before = Vec::new();
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.events.next()).await {
                Ok(Some(event)) if wanted(&event.event) => return Ok(before),
                Ok(Some(event)) => before.push(describe(&event.event)),
                Ok(None) | Err(_) => return Err(before),
            }
        }
    }
}

fn describe(event: &ConnEvent) -> String {
    match event {
        ConnEvent::Ready => "ready".into(),
        ConnEvent::Many(_) => "many".into(),
        ConnEvent::ServerError(error) => format!("server error {error}"),
        ConnEvent::Recovering(elapsed) => format!("recovering {elapsed:?}"),
        ConnEvent::Recovered => "recovered".into(),
        ConnEvent::Disconnected(reason) => format!("disconnected: {reason}"),
        ConnEvent::Unenrolled { .. } => "unenrolled".into(),
    }
}

/// Attaches the host in `directory` the way the window does.
fn connect(directory: &Path, runtime: &tokio::runtime::Runtime) -> Heard {
    let (sink, events) = futures::channel::mpsc::unbounded();
    let mut hosts = crate::Hosts::new(Arc::new(sink));
    hosts.attach(
        "lab".to_owned(),
        crate::AttachTarget::Local {
            runtime_dir: directory.join("runtime"),
        },
        |_, _| vec![Arc::new(Held) as Arc<dyn crate::HostStream>],
        runtime.handle(),
    );
    Heard {
        events,
        _hosts: hosts,
    }
}

fn is_ready(event: &ConnEvent) -> bool {
    matches!(event, ConnEvent::Ready)
}

fn is_dropped(event: &ConnEvent) -> bool {
    matches!(event, ConnEvent::Disconnected(_))
}

/// The client's iroh endpoint is bound once a process, on the runtime
/// of its first dial, and its tasks end with that runtime: every test
/// here dials on this one, as the window dials on its one runtime.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    });
    &RUNTIME
}

fn rig() -> (tempfile::TempDir, &'static tokio::runtime::Runtime) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("runtime")).unwrap();
    (directory, runtime())
}

/// Killed and started again at once, faster than any poll of the old
/// process could see it gone: the new process is what tells the client.
#[test]
fn a_host_killed_and_started_again_at_once_is_reconnected_within_seconds() {
    let (directory, runtime) = rig();
    let first = Host::start(directory.path());
    let mut heard = connect(directory.path(), runtime);
    runtime
        .block_on(heard.until(Duration::from_secs(20), is_ready))
        .expect("the first connection");

    first.kill();
    let killed = std::time::Instant::now();
    let _second = Host::start(directory.path());
    let before = runtime
        .block_on(heard.until(Duration::from_secs(30), is_ready))
        .unwrap_or_else(|heard| panic!("no reconnect within 30s; heard {heard:?}"));
    let took = killed.elapsed();
    eprintln!("reconnected {took:?} after the kill; heard {before:?}");
    assert!(
        before.iter().any(|event| event.starts_with("disconnected")),
        "the drop was reported before the reconnect: {before:?}"
    );
    assert!(took < Duration::from_secs(10), "took {took:?}");
}

/// Killed and left down: the client says so within seconds, rather than
/// waiting out the recovery window a roaming host gets, and connects
/// when the host is back.
#[test]
fn a_host_killed_and_left_down_is_reported_within_seconds() {
    let (directory, runtime) = rig();
    let first = Host::start(directory.path());
    let mut heard = connect(directory.path(), runtime);
    runtime
        .block_on(heard.until(Duration::from_secs(20), is_ready))
        .expect("the first connection");

    first.kill();
    let killed = std::time::Instant::now();
    let before = runtime
        .block_on(heard.until(Duration::from_secs(30), is_dropped))
        .unwrap_or_else(|heard| panic!("no drop within 30s; heard {heard:?}"));
    let took = killed.elapsed();
    eprintln!("dropped {took:?} after the kill; heard {before:?}");
    assert!(took < Duration::from_secs(5), "took {took:?}");

    std::thread::sleep(Duration::from_secs(2));
    let restarted = std::time::Instant::now();
    let _second = Host::start(directory.path());
    runtime
        .block_on(heard.until(Duration::from_secs(30), is_ready))
        .unwrap_or_else(|heard| panic!("no reconnect within 30s; heard {heard:?}"));
    let took = restarted.elapsed();
    eprintln!("reconnected {took:?} after the restart");
    assert!(took < Duration::from_secs(10), "took {took:?}");
}

/// `test-support` is what every other crate's tests turn on, and a build
/// that has them beside the window links it into the window too: it must
/// not stop a host attached the ordinary way from being dialed. Nothing
/// listens in the runtime directory, so the dial fails, and says so.
#[test]
fn an_attached_host_is_dialed_with_test_support_compiled_in() {
    const { assert!(cfg!(feature = "test-support"), "these tests build with it") };
    let (directory, runtime) = rig();
    let mut heard = connect(directory.path(), runtime);
    let before = runtime
        .block_on(heard.until(Duration::from_secs(5), is_dropped))
        .unwrap_or_else(|heard| panic!("nothing dialed; heard {heard:?}"));
    assert!(before.is_empty(), "{before:?}");
}

/// A host in this process is never dialed: what is opened to it comes to
/// the test, and its status is the test's to set.
#[test]
fn a_host_in_this_process_is_not_dialed() {
    let (sink, mut events) = futures::channel::mpsc::unbounded();
    let mut hosts = crate::Hosts::new(Arc::new(sink));
    let (host, mut opened) = hosts.attach_in_process(
        "lab".to_owned(),
        crate::AttachTarget::Local {
            runtime_dir: "/nonexistent/lab".into(),
        },
        |_, _| vec![Arc::new(Held) as Arc<dyn crate::HostStream>],
    );
    let dialer = hosts.link(host);
    futures::executor::block_on(async {
        let open = dialer.run(|dialer| async move { dialer.open(None).await.map(drop) });
        let (opened_here, reached) = futures::join!(open, opened.recv());
        opened_here.unwrap();
        assert!(reached.is_some(), "the stream reached the test");
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        matches!(
            events.try_recv(),
            Err(futures::channel::mpsc::TryRecvError::Empty)
        ),
        "no supervisor said anything"
    );
    assert_eq!(
        hosts.get(host).unwrap().status,
        crate::HostStatus::Connecting
    );
}
