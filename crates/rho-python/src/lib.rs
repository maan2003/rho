//! In-process Python notebook with synchronous Rust host callbacks.
//!
//! RustPython objects stay on the interpreter thread. Each execution carries a
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
    Patience {
        cell: CellId,
        seconds: u64,
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
