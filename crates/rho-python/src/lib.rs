//! In-process Python notebook with synchronous Rust host callbacks.
//!
//! RustPython objects stay inside the interpreter. Each execution carries a
//! shared Rust host handle; callbacks commit host state before Python
//! continues. Asynchronous completions wake the interpreter, which resolves
//! Python futures. This is ordinary, unsandboxed Python. A dedicated thread has
//! private cwd state and is initialized in the agent’s workspace view before
//! Python starts. Other process-global operations retain their normal
//! in-process semantics.

mod runtime;

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex, mpsc};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type CellId = u64;
pub type RequestId = u64;
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

fn wake(fd: &OwnedFd) {
    // eventfd is nonblocking; a saturated counter already wakes the selector.
    let value = 1_u64;
    unsafe { libc::write(fd.as_raw_fd(), (&value as *const u64).cast(), 8) };
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Input {
    Execute {
        cell: CellId,
        source: String,
    },
    Resolve {
        request: RequestId,
        value: Value,
        error: Option<String>,
    },
    Cancel {
        cell: CellId,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Started {
        cell: CellId,
    },
    Returned {
        cell: CellId,
        error: Option<String>,
    },
    Call {
        cell: CellId,
        request: RequestId,
        name: String,
        arguments: Value,
    },
    Text {
        cell: CellId,
        text: String,
        max_tokens: usize,
        important: bool,
    },
    Checkin {
        cell: CellId,
        seconds: u64,
        wake_on_tools: bool,
    },
    Finished {
        cell: CellId,
        error: Option<String>,
    },
    Stopped {
        error: Option<String>,
    },
}

/// An execution's Rust-owned state. Called synchronously on the interpreter
/// thread; implementations must not re-enter Python or wait for async work.
pub trait Execution: Send + Sync {
    fn event(&self, event: Event);
}

type Executions = Arc<Mutex<HashMap<CellId, Arc<dyn Execution>>>>;

/// A cloneable sender, not an owner of the interpreter's lifetime.
#[derive(Clone)]
pub struct Sender {
    tx: mpsc::SyncSender<Input>,
    executions: Executions,
    cancelled: Arc<Mutex<HashSet<CellId>>>,
    space: Arc<tokio::sync::Notify>,
    wake: Arc<OwnedFd>,
}

impl Sender {
    /// Publish the execution handle before making its code runnable.
    pub fn execute(
        &self,
        cell: CellId,
        source: String,
        exec: Arc<dyn Execution>,
    ) -> Result<(), String> {
        self.executions.lock().unwrap().insert(cell, exec);
        if let Err(error) = self.send(Input::Execute { cell, source }) {
            self.executions.lock().unwrap().remove(&cell);
            return Err(error);
        }
        Ok(())
    }

    pub fn send(&self, input: Input) -> Result<(), String> {
        if serde_json::to_vec(&input).map_err(|e| e.to_string())?.len() > MAX_MESSAGE_BYTES {
            return Err("Python input exceeds 1 MiB".into());
        }
        self.tx
            .try_send(input)
            .map_err(|e| format!("Python runtime unavailable or busy: {e}"))?;
        wake(&self.wake);
        Ok(())
    }

    /// Reliable, bounded host completion delivery with asynchronous
    /// backpressure.
    pub async fn send_async(&self, mut input: Input) -> Result<(), String> {
        if serde_json::to_vec(&input).map_err(|e| e.to_string())?.len() > MAX_MESSAGE_BYTES {
            return Err("Python input exceeds 1 MiB".into());
        }
        loop {
            let space = self.space.notified();
            tokio::pin!(space);
            space.as_mut().enable();
            match self.tx.try_send(input) {
                Ok(()) => {
                    wake(&self.wake);
                    return Ok(());
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err("Python runtime disconnected".into());
                }
                Err(mpsc::TrySendError::Full(value)) => input = value,
            }
            space.await;
        }
    }

    pub fn cancel(&self, cell: CellId) {
        self.cancelled.lock().unwrap().insert(cell);
        // The shared flag also interrupts a cell that is currently executing
        // Python bytecode and therefore cannot drain its inbox yet.
        let _ = self.tx.try_send(Input::Cancel { cell });
        wake(&self.wake);
    }
}

pub struct Session {
    sender: Sender,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Session {
    /// `setup` runs once on the dedicated thread after unsharing its cwd state.
    /// It may enter the agent's mount namespace and set the initial directory.
    pub fn new(
        setup: impl FnOnce() -> Result<(), String> + Send + 'static,
        tools: Value,
    ) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel(256);
        let executions: Executions = Default::default();
        let cancelled = Arc::new(Mutex::new(HashSet::new()));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let space = Arc::new(tokio::sync::Notify::new());
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
        runtime::spawn(
            rx,
            Arc::clone(&executions),
            Arc::clone(&cancelled),
            Arc::clone(&shutdown),
            Arc::clone(&space),
            Arc::clone(&wake),
            move || {
                setup()?;
                Ok(tools)
            },
        )?;
        Ok(Self {
            sender: Sender {
                tx,
                executions,
                cancelled,
                space,
                wake,
            },
            shutdown,
        })
    }

    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.sender.tx.try_send(Input::Shutdown);
        wake(&self.sender.wake);
        // Never join untrusted in-process code on an agent/Tokio thread. The
        // execution hook requests unwinding; native calls may still block.
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct RecordedExecution(tokio::sync::mpsc::Sender<Event>);
    impl Execution for RecordedExecution {
        fn event(&self, event: Event) {
            if !matches!(event, Event::Started { .. } | Event::Returned { .. }) {
                let _ = self.0.blocking_send(event);
            }
        }
    }
    struct TestSession {
        session: Session,
        events: tokio::sync::mpsc::Sender<Event>,
    }
    impl TestSession {
        fn sender(&self) -> TestSender {
            TestSender {
                sender: self.session.sender(),
                events: self.events.clone(),
            }
        }
    }
    #[derive(Clone)]
    struct TestSender {
        sender: Sender,
        events: tokio::sync::mpsc::Sender<Event>,
    }
    impl TestSender {
        fn send(&self, input: Input) -> Result<(), String> {
            match input {
                Input::Execute { cell, source } => self.sender.execute(
                    cell,
                    source,
                    Arc::new(RecordedExecution(self.events.clone())),
                ),
                other => self.sender.send(other),
            }
        }
        async fn send_async(&self, input: Input) -> Result<(), String> {
            self.sender.send_async(input).await
        }
        fn cancel(&self, cell: CellId) {
            self.sender.cancel(cell);
        }
    }
    fn test_session(
        setup: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) -> Result<(TestSession, tokio::sync::mpsc::Receiver<Event>), String> {
        let (events, receiver) = tokio::sync::mpsc::channel(256);
        Ok((
            TestSession {
                session: Session::new(setup, serde_json::json!([]))?,
                events,
            },
            receiver,
        ))
    }

    async fn next(rx: &mut tokio::sync::mpsc::Receiver<Event>) -> Event {
        tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("runtime timed out")
            .expect("runtime stopped")
    }

    #[tokio::test]
    async fn preinstalled_tls_provider_is_supported() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: "import ssl\nassert ssl.create_default_context().check_hostname".into(),
            })
            .unwrap();
        loop {
            match next(&mut rx).await {
                Event::Finished { error, .. } => {
                    assert!(error.is_none(), "{error:?}");
                    break;
                }
                Event::Started { .. } | Event::Returned { .. } => {}
                other => panic!("{other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn urllib_and_httpx_verify_https_certificates() {
        use std::io::{Read as _, Write as _};

        use rustls::pki_types::PrivatePkcs8KeyDer;

        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ca = directory.path().join("ca.pem");
        std::fs::write(&ca, certificate.cert.pem()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
        )
        .unwrap();
        let config = Arc::new(config);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut requests = 0;
            while requests < 4 && std::time::Instant::now() < deadline {
                let (socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                requests += 1;
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let connection = rustls::ServerConnection::new(config.clone()).unwrap();
                let mut stream = rustls::StreamOwned::new(connection, socket);
                let mut request = [0; 4096];
                // The untrusted-client case intentionally fails its handshake.
                if stream.read(&mut request).is_ok() {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                    stream.flush().unwrap();
                }
            }
            assert_eq!(requests, 4);
        });
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: format!(
                    r#"
import ssl, urllib.request, httpx
url = 'https://localhost:{port}/'
context = ssl.create_default_context(cafile={ca})
assert urllib.request.urlopen(url, context=context, timeout=5).read() == b'ok'
with httpx.Client(verify=context, trust_env=False) as client:
    assert client.get(url).text == 'ok'
async with httpx.AsyncClient(verify=context, trust_env=False) as client:
    assert (await client.get(url)).text == 'ok'
try:
    urllib.request.urlopen(url, timeout=5)
except urllib.error.URLError as error:
    assert isinstance(error.reason, ssl.SSLCertVerificationError), repr(error.reason)
else:
    raise AssertionError('untrusted certificate accepted')
"#,
                    ca = serde_json::to_string(&ca.to_string_lossy()).unwrap()
                ),
            })
            .unwrap();
        loop {
            match next(&mut rx).await {
                Event::Finished { error, .. } => {
                    assert!(error.is_none(), "{error:?}");
                    break;
                }
                Event::Started { .. } | Event::Returned { .. } => {}
                other => panic!("{other:?}"),
            }
        }
        server.join().unwrap();
    }

    #[tokio::test]
    async fn worker_cannot_consume_cell_cancellation_when_inbox_is_full() {
        let directory = tempfile::tempdir().unwrap();
        let release = directory.path().join("release");
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: format!(
                    r#"
import threading, sys, time
sys.settrace(None)
checkpoint = asyncio.get_running_loop().run_in_executor.__globals__['_cancel_requested']
# Library code can inspect the checkpoint without user-bytecode tracing
# raising before we can observe whether the read consumed the flag.
exec(compile("def worker():\n    notify('worker ready')\n    while not checkpoint(1, False):\n        time.sleep(0.01)\n    assert checkpoint(1, False)\n    notify('worker stopped')\n", '<cancel-probe>', 'exec'))
threading.Thread(target=worker).start()
while not Path({release}).exists():
    time.sleep(0.01)
await asyncio.Event().wait()
"#,
                    release = serde_json::to_string(&release.to_string_lossy()).unwrap()
                ),
            })
            .unwrap();
        assert!(matches!(next(&mut rx).await,
            Event::Text { text, .. } if text.contains("worker ready")));
        let sender = session.session.sender();
        while sender
            .tx
            .try_send(Input::Resolve {
                request: u64::MAX,
                value: Value::Null,
                error: None,
            })
            .is_ok()
        {}
        sender.cancel(1);
        loop {
            if let Event::Text { text, .. } = next(&mut rx).await
                && text.contains("worker stopped")
            {
                break;
            }
        }
        let cancellation_retained = sender.cancelled.lock().unwrap().contains(&1);
        // Always release the notebook, including when testing a broken checkpoint.
        std::fs::write(release, "").unwrap();
        assert!(cancellation_retained);
        loop {
            if let Event::Finished { error, .. } = next(&mut rx).await {
                assert!(error.unwrap().contains("CancelledError"));
                break;
            }
        }
    }

    #[tokio::test]
    async fn cancellation_preserves_already_completed_executor_bookkeeping() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: r#"
import concurrent.futures, contextvars
loop = asyncio.get_running_loop()
class ImmediateExecutor(concurrent.futures.Executor):
    def submit(self, function, *args):
        result = concurrent.futures.Future()
        result.set_result(function(*args))
        return result
# Arrange inbox cancellation immediately before the completed worker's
# cleanup callback, without depending on an OS-thread scheduling race.
runtime = loop.run_in_executor.__globals__
receive = runtime['_receive']
def cancellation_message():
    runtime['_receive'] = receive
    return '[{"kind":"cancel","cell":1}]'
runtime['_receive'] = cancellation_message
loop.call_soon(runtime['_receive_ready'], context=contextvars.Context())
loop.run_in_executor(ImmediateExecutor(), lambda: None)
await asyncio.sleep(60)
"#
                .into(),
            })
            .unwrap();
        loop {
            match next(&mut rx).await {
                Event::Finished { error, .. } => {
                    assert!(error.unwrap().contains("CancelledError"));
                    break;
                }
                Event::Started { .. } | Event::Returned { .. } => {}
                other => panic!("{other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn threads_and_cancelled_executor_awaits_keep_their_cell_alive() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: r#"
import threading, time, contextvars
def background():
    time.sleep(0.1)
    print('raw thread output')
threading.Thread(target=background).start()
user_context = contextvars.ContextVar('user_context', default='default')
user_context.set('caller')
def direct_executor_work():
    assert user_context.get() == 'default'
    print('direct executor output')
await asyncio.get_running_loop().run_in_executor(None, direct_executor_work)
def executor_work():
    time.sleep(0.2)
    print('executor output after cancelled await')
task = asyncio.create_task(asyncio.to_thread(executor_work))
await asyncio.sleep(0.05)
task.cancel()
try:
    await task
except asyncio.CancelledError:
    pass
"#
                .into(),
            })
            .unwrap();
        let mut output = String::new();
        loop {
            match next(&mut rx).await {
                Event::Text { cell, text, .. } => {
                    assert_eq!(cell, 1);
                    output.push_str(&text);
                }
                Event::Finished { error, .. } => {
                    assert!(error.is_none(), "{error:?}");
                    break;
                }
                Event::Started { .. } | Event::Returned { .. } => {}
                other => panic!("{other:?}"),
            }
        }
        assert!(output.contains("raw thread output"), "{output}");
        assert!(output.contains("direct executor output"), "{output}");
        assert!(
            output.contains("executor output after cancelled await"),
            "{output}"
        );
    }

    #[tokio::test]
    async fn standard_libraries_and_bundled_packages_work() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: r#"
import ssl, sqlite3, yaml, httpx, pickle, threading, time
context = ssl.create_default_context()
assert context.verify_mode == ssl.CERT_REQUIRED
assert context.check_hostname
assert context.cert_store_stats()['x509'] > 0
with sqlite3.connect(':memory:') as database:
    database.execute('create table values_ (value text)')
    database.execute('insert into values_ values (?)', ('雪',))
    assert database.execute('select value from values_').fetchone() == ('雪',)
assert yaml.safe_load('items: [one, two]') == {'items': ['one', 'two']}
assert yaml.safe_load(yaml.safe_dump({'snow': '雪'})) == {'snow': '雪'}
with httpx.Client(transport=httpx.MockTransport(
        lambda request: httpx.Response(200, json={'path': request.url.path}))) as client:
    assert client.get('https://test.invalid/example').json() == {'path': '/example'}
async with httpx.AsyncClient(transport=httpx.MockTransport(
        lambda request: httpx.Response(200, json={'ok': True}))) as client:
    assert (await client.get('https://test.invalid')).json() == {'ok': True}
class Example:
    pass
assert isinstance(pickle.loads(pickle.dumps(Example())), Example)
main_id = threading.get_ident()
worker_id = await asyncio.to_thread(threading.get_ident)
assert main_id != worker_id
events = []
async def tick():
    await asyncio.sleep(0.02)
    events.append('tick')
async def work():
    await asyncio.to_thread(time.sleep, 0.2)
    events.append('work')
await asyncio.gather(tick(), work())
assert events == ['tick', 'work'], events
"#
                .into(),
            })
            .unwrap();
        loop {
            match next(&mut rx).await {
                Event::Finished { error, .. } => {
                    assert!(error.is_none(), "{error:?}");
                    break;
                }
                Event::Started { .. } | Event::Returned { .. } => {}
                other => panic!("{other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn default_text_io_uses_utf8() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: r#"
import io, sys, tempfile
from pathlib import Path
assert sys.flags.utf8_mode == 1
assert io.text_encoding(None) == 'utf-8'
sample = '— 雪🙂 café'
with io.TextIOWrapper(io.BytesIO(sample.encode('utf-8'))) as wrapper:
    assert wrapper.read() == sample
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / 'unicode.txt'
    path.write_bytes(sample.encode('utf-8'))
    assert path.read_text() == sample
    path.write_text(sample)
    assert path.read_bytes() == sample.encode('utf-8')
"#
                .into(),
            })
            .unwrap();
        match next(&mut rx).await {
            Event::Finished { error, .. } => assert!(error.is_none(), "{error:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn live_cells_share_globals_and_keep_attribution() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session.sender().send(Input::Execute { cell: 1, source: "values = []\nvalues.append(1)\nawait asyncio.sleep(0.1)\nvalues.append(3)\nnotify(values)".into() }).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 2,
                source: "values.append(2)\ntext(values)".into(),
            })
            .unwrap();
        let mut finished = 0;
        let mut messages = Vec::new();
        while finished < 2 {
            match next(&mut rx).await {
                Event::Text { cell, text, .. } => messages.push((cell, text.trim_end().to_owned())),
                Event::Finished { error, .. } => {
                    assert!(error.is_none(), "{error:?}");
                    finished += 1;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            messages,
            vec![(2, "[1, 2]".into()), (1, "[1, 2, 3]".into())]
        );
    }

    #[tokio::test]
    async fn command_starts_without_await_and_can_be_awaited_later() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: "job = command('echo hello')".into(),
            })
            .unwrap();
        let request = match next(&mut rx).await {
            Event::Call { request, name, .. } => {
                assert_eq!(name, "command");
                request
            }
            e => panic!("{e:?}"),
        };
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished { error: None, .. }
        ));
        session
            .sender()
            .send(Input::Execute {
                cell: 2,
                source: "text(await job)".into(),
            })
            .unwrap();
        session
            .sender()
            .send(Input::Resolve {
                request,
                value: serde_json::json!({"exit_code":0}),
                error: None,
            })
            .unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { cell: 2, .. }));
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished {
                cell: 2,
                error: None
            }
        ));
    }

    #[tokio::test]
    async fn blocked_loop_timeout_interrupts_library_code_and_preserves_other_cells() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("busy_library.py"),
            "def spin():\n    while True:\n        pass\n",
        )
        .unwrap();
        let path = serde_json::to_string(&dir.path().to_string_lossy()).unwrap();
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: format!(
                    r#"
import sys, time
sys.path.insert(0, {path})
import busy_library
await asyncio.to_thread(lambda: None)
runtime = asyncio.get_running_loop().run_in_executor.__globals__
assert runtime['_SYNC_TIMEOUT'] == 120
assert runtime['_HEARTBEAT_INTERVAL'] == 10
runtime['_HEARTBEAT_INTERVAL'] = 0.02
runtime['_heartbeat_handle'].cancel()
runtime['_heartbeat']()
released = asyncio.Event()
worker_released = __import__('threading').Event()
def worker_body():
    while not worker_released.wait(0.01):
        pass
async def worker():
    await asyncio.to_thread(worker_body)
    print('worker survived')
asyncio.create_task(worker())
runtime['_SYNC_TIMEOUT'] = 1
notify('waiting')
await released.wait()
print('waiter survived')
"#
                ),
            })
            .unwrap();
        let event = next(&mut rx).await;
        assert!(
            matches!(&event, Event::Text { text, .. } if text.trim() == "waiting"),
            "{event:?}"
        );

        for (cell, source) in [
            (2, "busy_library.spin()"),
            (
                3,
                "exec(compile('while True:\\n    pass', '<dynamic>', 'exec'))",
            ),
            (4, "asyncio.get_running_loop().call_soon(busy_library.spin)"),
            (
                5,
                "async def spin():\n    busy_library.spin()\nasyncio.create_task(spin())",
            ),
        ] {
            session
                .sender()
                .send(Input::Execute {
                    cell,
                    source: source.into(),
                })
                .unwrap();
            assert!(
                matches!(next(&mut rx).await, Event::Finished { cell: finished, error: Some(error) }
                    if finished == cell && error.contains("blocking the event loop for 2 minutes")),
                "cell {cell} did not time out"
            );
        }

        session
            .sender()
            .send(Input::Execute {
                cell: 6,
                source: "assert sys.gettrace() is not None\nreleased.set()\nworker_released.set()"
                    .into(),
            })
            .unwrap();
        let mut finished = HashSet::new();
        let mut output = String::new();
        while finished.len() < 2 {
            match next(&mut rx).await {
                Event::Finished { cell, error: None } => {
                    finished.insert(cell);
                }
                Event::Text { text, .. } => output.push_str(&text),
                event => panic!("unexpected event: {event:?}"),
            }
        }
        assert_eq!(finished, HashSet::from([1, 6]));
        assert!(output.contains("worker survived"), "{output}");
        assert!(output.contains("waiter survived"), "{output}");
    }

    #[tokio::test]
    async fn blocked_loop_timeout_protects_bookkeeping_and_resets_between_callbacks() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: r#"
import time
runtime = asyncio.get_running_loop().run_in_executor.__globals__
runtime['_SYNC_TIMEOUT'] = 0.1
runtime['_HEARTBEAT_INTERVAL'] = 0.01
runtime['_heartbeat_handle'].cancel()
runtime['_heartbeat']()
class SlowString:
    def __str__(self):
        until = time.monotonic() + 0.2
        while time.monotonic() < until:
            pass
        return 'formatting survived'
text(SlowString())
"#
                .into(),
            })
            .unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { text, .. }
            if text.trim() == "formatting survived"));
        // Unwinding/returning to user code after protected synchronous work
        // may time out, but the runtime's output bookkeeping must stay intact.
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished { cell: 1, .. }
        ));
        session
            .sender()
            .send(Input::Execute {
                cell: 2,
                source: r#"
loop = asyncio.get_running_loop()
loop.call_soon(time.sleep, 0.2)
loop.call_soon(notify, 'next callback survived')
await asyncio.sleep(0.4)
"#
                .into(),
            })
            .unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { text, .. }
            if text.trim() == "next callback survived"));
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished {
                cell: 2,
                error: None
            }
        ));
        session
            .sender()
            .send(Input::Execute {
                cell: 4,
                source: r#"
class InvalidYield:
    def __repr__(self):
        until = time.monotonic() + 0.2
        while time.monotonic() < until:
            pass
        return 'invalid-yield'
class InvalidAwaitable:
    def __await__(self):
        yield InvalidYield()
async def invalid(after_wakeup):
    if after_wakeup:
        await asyncio.sleep(0.01)
    await InvalidAwaitable()
# Both native Task step and wakeup must protect result bookkeeping.
results = await asyncio.gather(invalid(False), invalid(True), return_exceptions=True)
assert all(isinstance(error, RuntimeError) and 'invalid-yield' in str(error)
           for error in results), results
"#
                .into(),
            })
            .unwrap();
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished {
                cell: 4,
                error: None
            }
        ));
        // Idle time and long awaits are not synchronous blocking.
        tokio::time::sleep(Duration::from_millis(300)).await;
        session
            .sender()
            .send(Input::Execute {
                cell: 3,
                source: "import sys\nawait asyncio.sleep(0.3)\nassert sys.gettrace() is not None"
                    .into(),
            })
            .unwrap();
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished {
                cell: 3,
                error: None
            }
        ));
    }

    #[tokio::test]
    async fn cancels_python_loop_without_losing_notebook() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: "survives = 42\nnotify('started')\nwhile True:\n    pass".into(),
            })
            .unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { .. }));
        session.sender().cancel(1);
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished { error: Some(_), .. }
        ));
        session
            .sender()
            .send(Input::Execute {
                cell: 2,
                source: "import sys\nassert sys.gettrace() is not None\ntext(survives)".into(),
            })
            .unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text{text,..} if text.trim() == "42"));
    }
    #[tokio::test]
    async fn completion_bursts_apply_backpressure_without_losing_awaits() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session.sender().send(Input::Execute { cell:1, source:"import asyncio\njobs = [command('test') for _ in range(600)]\nawait asyncio.gather(*jobs)".into() }).unwrap();
        let mut deliveries = tokio::task::JoinSet::new();
        let mut calls = 0;
        loop {
            match next(&mut rx).await {
                Event::Call { request, .. } => {
                    calls += 1;
                    let sender = session.sender();
                    deliveries.spawn(async move {
                        sender
                            .send_async(Input::Resolve {
                                request,
                                value: Value::Null,
                                error: None,
                            })
                            .await
                            .unwrap();
                    });
                }
                Event::Finished { error, .. } => {
                    assert!(error.is_none(), "{error:?}");
                    break;
                }
                event => panic!("{event:?}"),
            }
        }
        assert_eq!(calls, 600);
        while let Some(result) = deliveries.join_next().await {
            result.unwrap();
        }
    }

    #[tokio::test]
    async fn gathered_errors_can_be_handled_and_child_tasks_stay_attributed() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session.sender().send(Input::Execute {cell:1,source:"import asyncio\nasync def child():\n    await asyncio.sleep(0.01)\n    notify('child')\n    raise ValueError('expected')\nresults = await asyncio.gather(child(), return_exceptions=True)\nassert isinstance(results[0], ValueError)".into()}).unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { cell: 1, .. }));
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished {
                cell: 1,
                error: None
            }
        ));
    }
    #[tokio::test]
    async fn cancellation_survives_execute_and_cancel_in_the_same_inbox_batch() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: "notify('blocked')\nwhile True:\n    pass".into(),
            })
            .unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { cell: 1, .. }));
        session
            .sender()
            .send(Input::Execute {
                cell: 2,
                source: "while True:\n    pass".into(),
            })
            .unwrap();
        session.sender().cancel(2);
        session.sender().cancel(1);
        for expected in [1, 2] {
            assert!(
                matches!(next(&mut rx).await, Event::Finished{cell,error:Some(_)} if cell == expected)
            );
        }
    }
    #[tokio::test]
    async fn standard_asyncio_queues_task_groups_timeouts_and_streams_work() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: r#"
import asyncio
queue = asyncio.Queue(maxsize=1)
lock = asyncio.Lock()
values = []
async def produce():
    for n in range(3):
        await queue.put(n)
async def consume():
    for _ in range(3):
        n = await queue.get()
        async with lock:
            values.append(n)
        queue.task_done()
async with asyncio.TaskGroup() as group:
    group.create_task(produce())
    group.create_task(consume())
await queue.join()
assert values == [0, 1, 2]
for timeout in [True, False]:
    try:
        if timeout:
            async with asyncio.timeout(0.01):
                await asyncio.Event().wait()
        else:
            await asyncio.wait_for(asyncio.Event().wait(), 0.01)
    except TimeoutError:
        pass
    else:
        raise AssertionError('timeout did not fire')
async def echo(reader, writer):
    writer.write((await reader.readline()).upper())
    await writer.drain()
    writer.close()
    await writer.wait_closed()
server = await asyncio.start_server(echo, '127.0.0.1', 0)
async with server:
    reader, writer = await asyncio.open_connection('localhost', server.sockets[0].getsockname()[1])
    writer.write(b'hello\n')
    await writer.drain()
    assert await reader.readline() == b'HELLO\n'
    writer.close()
    await writer.wait_closed()
process = await asyncio.create_subprocess_exec('sh', '-c', 'printf native', stdout=asyncio.subprocess.PIPE)
assert await process.communicate() == (b'native', None)
assert process.returncode == 0
"#
                .into(),
            })
            .unwrap();
        let event = next(&mut rx).await;
        assert!(
            matches!(&event, Event::Finished { error: None, .. }),
            "{event:?}"
        );
    }

    #[tokio::test]
    async fn asyncio_task_limit_rejects_a_new_cell_without_losing_the_loop() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session.sender().send(Input::Execute {cell: 1, source: "for _ in range(1023):\n    asyncio.create_task(asyncio.sleep(3600))\nnotify('full')\nawait asyncio.Event().wait()".into()}).unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { cell: 1, .. }));
        session
            .sender()
            .send(Input::Execute {
                cell: 2,
                source: "text('must not run')".into(),
            })
            .unwrap();
        let event = next(&mut rx).await;
        assert!(
            matches!(&event, Event::Finished { cell: 2, error: Some(error) } if error.contains("task limit")),
            "{event:?}"
        );
        session.sender().cancel(1);
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished {
                cell: 1,
                error: Some(_)
            }
        ));
        session
            .sender()
            .send(Input::Execute {
                cell: 3,
                source: "text('alive')".into(),
            })
            .unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { cell: 3, .. }));
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished {
                cell: 3,
                error: None
            }
        ));
    }

    #[tokio::test]
    async fn standard_asyncio_thread_work_preserves_cell_context() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: "await asyncio.to_thread(notify, 'thread output')".into(),
            })
            .unwrap();
        let event = next(&mut rx).await;
        assert!(
            matches!(&event, Event::Text { cell: 1, text, .. } if text.trim() == "thread output"),
            "{event:?}"
        );
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished { error: None, .. }
        ));
    }

    #[tokio::test]
    async fn standard_asyncio_callbacks_keep_their_cell_alive_and_can_be_cancelled() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: "asyncio.get_running_loop().call_later(0.01, notify, 'callback')".into(),
            })
            .unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { cell: 1, .. }));
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished {
                cell: 1,
                error: None
            }
        ));
        session.sender().send(Input::Execute {cell: 2, source: "asyncio.get_running_loop().call_later(3600, notify, 'too late')\ntext('scheduled')".into()}).unwrap();
        assert!(matches!(next(&mut rx).await, Event::Text { cell: 2, .. }));
        session.sender().cancel(2);
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished {
                cell: 2,
                error: Some(_)
            }
        ));
    }

    #[tokio::test]
    async fn large_output_requests_are_capped_without_rejecting_the_cell() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source: "text('hello', max_tokens=12000)\ncommand('true', max_tokens=12000)".into(),
            })
            .unwrap();
        assert!(matches!(
            next(&mut rx).await,
            Event::Text {
                max_tokens: 10000,
                ..
            }
        ));
        assert!(
            matches!(next(&mut rx).await, Event::Call { arguments, .. } if arguments["max_tokens"] == 10000)
        );
        assert!(matches!(
            next(&mut rx).await,
            Event::Finished { error: None, .. }
        ));
    }

    #[tokio::test]
    async fn standard_streams_preserve_print_format_and_cell_attribution() {
        let (session, mut rx) = test_session(|| Ok(())).unwrap();
        session
            .sender()
            .send(Input::Execute {
                cell: 1,
                source:
                    "import sys\nprint('hello', 'world', end='!')\nsys.stderr.write('diagnostic')"
                        .into(),
            })
            .unwrap();
        let mut printed = String::new();
        loop {
            match next(&mut rx).await {
                Event::Text { cell: 1, text, .. } => printed.push_str(&text),
                Event::Finished { error: None, .. } => break,
                event => panic!("{event:?}"),
            }
        }
        assert_eq!(printed, "hello world!diagnostic");
    }
    #[tokio::test]
    async fn pathlib_is_prebound_and_each_notebook_has_its_own_cwd() {
        let original = std::env::current_dir().unwrap();
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        let path_one = one.path().to_owned();
        let path_two = two.path().to_owned();
        let (first, mut first_rx) =
            test_session(move || std::env::set_current_dir(path_one).map_err(|e| e.to_string()))
                .unwrap();
        let (second, mut second_rx) =
            test_session(move || std::env::set_current_dir(path_two).map_err(|e| e.to_string()))
                .unwrap();
        first.sender().send(Input::Execute {cell:1,source:"import pathlib\nfrom pathlib import Path\nimport os\nPath('sub').mkdir()\nos.chdir('sub')\nPath('note.txt').write_text('one')\nassert Path('note.txt').read_text() == 'one'\nimport shutil, json\nshutil.copyfile('note.txt', 'copy.txt')\nassert [p.name for p in Path('.').glob('copy.*')] == ['copy.txt']\nwith open('data.json', 'w') as f:\n    json.dump({'ok': True}, f)\nwith open('data.json') as f:\n    assert json.load(f)['ok']".into()}).unwrap();
        second.sender().send(Input::Execute {cell:1,source:"assert pathlib.Path is Path\nPath('note.txt').write_text('two')\nassert Path('note.txt').read_text() == 'two'".into()}).unwrap();
        assert!(matches!(
            next(&mut first_rx).await,
            Event::Finished { error: None, .. }
        ));
        assert!(matches!(
            next(&mut second_rx).await,
            Event::Finished { error: None, .. }
        ));
        assert_eq!(std::env::current_dir().unwrap(), original);
        assert_eq!(
            std::fs::read_to_string(one.path().join("sub/note.txt")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(two.path().join("note.txt")).unwrap(),
            "two"
        );
    }
}
