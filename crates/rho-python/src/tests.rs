use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::*;

struct RecordedExecution(tokio::sync::mpsc::UnboundedSender<Event>);

impl Execution for RecordedExecution {
    fn event(&self, event: Event) {
        if !matches!(event, Event::Started { .. } | Event::Returned { .. }) {
            let _ = self.0.send(event);
        }
    }
}

struct TestSession {
    session: Session,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
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
    events: tokio::sync::mpsc::UnboundedSender<Event>,
}

impl TestSender {
    fn send(&self, input: Input) -> Result<(), String> {
        match input {
            Input::Execute { cell, source } => self.sender.execute(
                cell,
                source,
                Arc::new(RecordedExecution(self.events.clone())),
            ),
            Input::BeginStream { cell } => self
                .sender
                .stream(cell, Arc::new(RecordedExecution(self.events.clone()))),
            other => self.sender.send(other),
        }
    }

    fn cancel(&self, cell: CellId) {
        self.sender.cancel(cell);
    }
}

fn test_session(
    setup: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(TestSession, tokio::sync::mpsc::UnboundedReceiver<Event>), String> {
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    Ok((
        TestSession {
            session: Session::new(setup, Host::default(), tokio::runtime::Handle::current())?,
            events,
        },
        receiver,
    ))
}

fn session_with_host(host: Host) -> Result<(TestSession, tokio::sync::mpsc::UnboundedReceiver<Event>), String> {
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    Ok((
        TestSession {
            session: Session::new(|| Ok(()), host, tokio::runtime::Handle::current())?,
            events,
        },
        receiver,
    ))
}

async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .expect("runtime timed out")
        .expect("runtime stopped")
}

async fn finished(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>, cell: CellId) -> Option<String> {
    loop {
        match next(rx).await {
            Event::Finished { cell: got, error } if got == cell => return error,
            Event::Stopped { error } => panic!("interpreter stopped: {error:?}"),
            _ => {}
        }
    }
}

#[derive(Default)]
struct CountingHistory {
    gets: AtomicUsize,
}

impl History for CountingHistory {
    fn len(&self, _cell: CellId) -> Result<usize, String> {
        Ok(3)
    }

    fn get(&self, _cell: CellId, index: usize) -> Result<HistoryItem, String> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        Ok(HistoryItem {
            kind: match index {
                0 => "item_0",
                1 => "item_1",
                _ => "item_2",
            },
            ..Default::default()
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EchoArgs {
    text: String,
    count: Option<u64>,
}

#[derive(Serialize)]
struct EchoResult {
    text: String,
    count: u64,
}

#[derive(Deserialize)]
struct EmptyArgs {}

#[derive(Default)]
struct FakeCommands {
    calls: Mutex<Vec<String>>,
}

impl FakeCommands {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn ready<T: Send + 'static>(value: T) -> HostFuture<T> {
        Box::pin(async move { Ok(value) })
    }
}

impl Commands for FakeCommands {
    fn start(
        &self,
        cell: CellId,
        cmd: String,
        workdir: Option<String>,
        max_tokens: usize,
    ) -> Result<(u64, HostFuture<CommandExit>), String> {
        self.calls.lock().unwrap().push(format!(
            "start:{cell}:{cmd}:{}:{max_tokens}",
            workdir.unwrap_or_default()
        ));
        Ok((41, Self::ready(CommandExit { id: 41, exit_code: Some(0) })))
    }

    fn find(&self, session_id: u64) -> Result<u64, String> {
        self.calls.lock().unwrap().push(format!("find:{session_id}"));
        Ok(42)
    }

    fn wait(&self, cell: CellId, id: u64) -> Result<HostFuture<CommandExit>, String> {
        self.calls.lock().unwrap().push(format!("wait:{cell}:{id}"));
        Ok(Self::ready(CommandExit { id, exit_code: Some(0) }))
    }

    fn write_stdin(&self, cell: CellId, id: u64, chars: String) -> Result<HostFuture<()>, String> {
        self.calls.lock().unwrap().push(format!("stdin:{cell}:{id}:{chars}"));
        Ok(Self::ready(()))
    }

    fn more_output(&self, cell: CellId, id: u64, max_tokens: usize) -> Result<HostFuture<()>, String> {
        self.calls.lock().unwrap().push(format!("more:{cell}:{id}:{max_tokens}"));
        Ok(Self::ready(()))
    }

    fn cancel(&self, cell: CellId, id: u64) -> Result<HostFuture<()>, String> {
        self.calls.lock().unwrap().push(format!("cancel:{cell}:{id}"));
        Ok(Self::ready(()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_loads_constant_sets_and_preserves_notebook_state() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    for (cell, source) in [
        (1, "names = {'fedimint-core', 'fedimint-api-client', 'fedimint-wallet-client'}"),
        (2, "assert sorted(names) == ['fedimint-api-client', 'fedimint-core', 'fedimint-wallet-client']\nnames.add('extra')\nassert len(names) == 4\ndef nested(value):\n return value in {'fedimint-core', 'fedimint-api-client', 'fedimint-wallet-client'}\nassert nested('fedimint-core') and not nested('extra')\nassert eval(\"{'a', 'b', 'c'}\") == set(['a', 'b', 'c'])"),
    ] {
        sender.send(Input::BeginStream { cell }).unwrap();
        sender.send(Input::StreamFeed { cell, source: source.into(), eof: true }).unwrap();
        loop {
            match next(&mut events).await {
                Event::UnitReady { cell, end } => sender.send(Input::StreamPermit { cell, end }).unwrap(),
                Event::UnitSettled { error, .. } => assert!(error.is_none(), "{error:?}"),
                Event::Finished { cell: got, error } if got == cell => {
                    assert!(error.is_none(), "{error:?}");
                    break;
                }
                _ => {}
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn history_only_materializes_indexed_items() {
    let history = Arc::new(CountingHistory::default());
    let host = Host { history: history.clone(), ..Host::default() };
    let (session, mut events) = session_with_host(host).unwrap();
    session.sender().send(Input::Execute {
        cell: 7,
        source: "assert len(transcript) == 3\nassert transcript[-1].kind == 'item_2'".into(),
    }).unwrap();
    assert_eq!(finished(&mut events, 7).await, None);
    assert_eq!(history.gets.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_functions_serialize_results_and_reject_bad_arguments() {
    let detached = Arc::new(AtomicUsize::new(0));
    let called = detached.clone();
    let host = Host {
        functions: vec![
            Function::new("echo", &["text"], |_, args: EchoArgs| {
                Ok(Box::pin(async move {
                    Ok(EchoResult { text: args.text, count: args.count.unwrap_or(1) })
                }))
            }),
            Function::new("broken", &[], |_, _: EmptyArgs| {
                Ok(Box::pin(async { Err::<(), String>("host failed".into()) }))
            }),
            Function::new("detached", &[], move |_, _: EmptyArgs| {
                called.fetch_add(1, Ordering::Relaxed);
                Ok(Box::pin(async { Ok(()) }))
            }).detached(),
        ],
        ..Host::default()
    };
    let (session, mut events) = session_with_host(host).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: r#"
assert await echo('hi', count=3) == {'text': 'hi', 'count': 3}
async def rejects():
    try:
        await echo('a', 'b')
    except TypeError as error:
        assert "takes 1 positional arguments but 2 were given" in str(error)
    else: raise AssertionError('extra position')
    try:
        await echo(text='a', extra=1)
    except RuntimeError as error:
        assert "unknown field `extra`" in str(error)
    else: raise AssertionError('unknown keyword')
    try:
        await broken()
    except RuntimeError as error:
        assert "host failed" in str(error)
    else: raise AssertionError('host failure')
await rejects()
assert detached() is None
"#.into() }).unwrap();
    assert_eq!(finished(&mut events, 1).await, None);
    assert_eq!(detached.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_start_await_and_control_typed_host() {
    let commands = Arc::new(FakeCommands::default());
    let host = Host { commands: Some(commands.clone()), ..Host::default() };
    let (session, mut events) = session_with_host(host).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: r#"
job = command('echo hello', workdir='/tmp', max_tokens=12000)
assert job.id == 41
assert (await job)['exit_code'] == 0
await write_stdin(job, 'input')
await job.more_output(max_tokens=12000)
await job.cancel()
other = Command.from_session_id(9)
assert other.id == 42
assert (await other)['id'] == 42
"#.into() }).unwrap();
    assert_eq!(finished(&mut events, 1).await, None);
    assert_eq!(commands.calls(), vec![
        "start:1:echo hello:/tmp:10000",
        "stdin:1:41:input",
        "more:1:41:10000",
        "cancel:1:41",
        "find:9",
        "wait:1:42",
    ]);
}

#[tokio::test(flavor = "multi_thread")]
async fn input_backlog_crosses_multiple_drain_batches_without_refusing_a_cell() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    for _ in 0..513 {
        sender.send(Input::StreamStop { cell: u64::MAX }).unwrap();
    }
    sender.send(Input::Execute { cell: 1, source: "notify('after the backlog')".into() }).unwrap();
    loop {
        if let Event::Text { text, .. } = next(&mut events).await
            && text.contains("after the backlog") { break; }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_bypasses_backlog_with_cloned_senders_alive() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    sender.send(Input::Execute { cell: 1, source: "notify('waiting'); await asyncio.Event().wait()".into() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Text { .. }));
    for _ in 0..513 { sender.send(Input::StreamStop { cell: u64::MAX }).unwrap(); }
    drop(session);
    loop {
        if matches!(next(&mut events).await, Event::Stopped { .. } | Event::Finished { .. }) { break; }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_requires_each_permit_and_stop_does_not_finish_the_suffix() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    let first = "seen = ['first']\n";
    sender.send(Input::BeginStream { cell: 1 }).unwrap();
    sender.send(Input::StreamFeed { cell: 1, source: format!("{first}seen.append('second')\nif True:\n    seen.append('suffix')\n"), eof: false }).unwrap();
    assert!(matches!(next(&mut events).await, Event::UnitReady { end, .. } if end == first.len()));
    sender.send(Input::Execute { cell: 2, source: "assert 'seen' not in globals()".into() }).unwrap();
    assert_eq!(finished(&mut events, 2).await, None);
    sender.send(Input::StreamPermit { cell: 1, end: first.len() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::UnitSettled { end, error: None, .. } if end == first.len()));
    assert!(matches!(next(&mut events).await, Event::UnitReady { cell: 1, .. }));
    sender.send(Input::StreamStop { cell: 1 }).unwrap();
    assert_eq!(finished(&mut events, 1).await, None);
    sender.send(Input::Execute { cell: 3, source: "assert seen == ['first']".into() }).unwrap();
    assert_eq!(finished(&mut events, 3).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_eof_closes_compounds_and_preserves_future_flags() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    sender.send(Input::BeginStream { cell: 1 }).unwrap();
    sender.send(Input::StreamFeed { cell: 1, source: "from __future__ import annotations\nif True:\n    def f(x: Missing):\n        return x\n".into(), eof: true }).unwrap();
    loop {
        match next(&mut events).await {
            Event::UnitReady { cell, end } => sender.send(Input::StreamPermit { cell, end }).unwrap(),
            Event::Finished { cell: 1, error } => { assert!(error.is_none(), "{error:?}"); break; }
            _ => {}
        }
    }
    sender.send(Input::Execute { cell: 2, source: "assert f.__annotations__['x'] == 'Missing'".into() }).unwrap();
    assert_eq!(finished(&mut events, 2).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_loss_allows_admitted_await_to_settle_without_admitting_more() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    sender.send(Input::Execute { cell: 1, source: "import asyncio\ngate = asyncio.Event()\nseen = []".into() }).unwrap();
    assert_eq!(finished(&mut events, 1).await, None);
    let first = "await gate.wait(); seen.append('settled')\n";
    sender.send(Input::BeginStream { cell: 2 }).unwrap();
    sender.send(Input::StreamFeed { cell: 2, source: format!("{first}seen.append('wrong')\n"), eof: false }).unwrap();
    loop { if let Event::UnitReady { end, .. } = next(&mut events).await { sender.send(Input::StreamPermit { cell: 2, end }).unwrap(); break; } }
    sender.send(Input::StreamStop { cell: 2 }).unwrap();
    sender.send(Input::Execute { cell: 3, source: "gate.set()".into() }).unwrap();
    let mut settled = false;
    loop {
        match next(&mut events).await {
            Event::UnitSettled { cell: 2, error: None, .. } => settled = true,
            Event::UnitReady { cell: 2, .. } => panic!("source after loss became runnable"),
            Event::Finished { cell: 2, error } => { assert!(error.is_none(), "{error:?}"); assert!(settled); break; }
            _ => {}
        }
    }
    sender.send(Input::Execute { cell: 4, source: "assert seen == ['settled']".into() }).unwrap();
    assert_eq!(finished(&mut events, 4).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_libraries_and_bundled_packages_work() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: r#"
import ssl, sqlite3, yaml, httpx, pickle, threading, time
assert ssl.create_default_context().check_hostname
with sqlite3.connect(':memory:') as database:
 database.execute('create table values_ (value text)')
 database.execute('insert into values_ values (?)', ('雪',))
 assert database.execute('select value from values_').fetchone() == ('雪',)
assert yaml.safe_load('items: [one, two]') == {'items': ['one', 'two']}
with httpx.Client(transport=httpx.MockTransport(lambda request: httpx.Response(200, json={'path': request.url.path}))) as client:
 assert client.get('https://test.invalid/example').json() == {'path': '/example'}
assert isinstance(pickle.loads(pickle.dumps({'snow': '雪'})), dict)
assert threading.get_ident() != await asyncio.to_thread(threading.get_ident)
events = []
async def tick(): await asyncio.sleep(.02); events.append('tick')
async def work(): await asyncio.to_thread(time.sleep, .05); events.append('work')
await asyncio.gather(tick(), work())
assert events == ['tick', 'work']
"#.into() }).unwrap();
    assert_eq!(finished(&mut events, 1).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn default_text_io_uses_utf8() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: r#"
import io, sys, tempfile
assert sys.flags.utf8_mode == 1 and io.text_encoding(None) == 'utf-8'
sample = '— 雪🙂 café'
with io.TextIOWrapper(io.BytesIO(sample.encode('utf-8'))) as wrapper: assert wrapper.read() == sample
with tempfile.TemporaryDirectory() as directory:
 path = Path(directory) / 'unicode.txt'
 path.write_text(sample)
 assert path.read_bytes() == sample.encode('utf-8')
"#.into() }).unwrap();
    assert_eq!(finished(&mut events, 1).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_cells_share_globals_and_keep_attribution() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    sender.send(Input::Execute { cell: 1, source: "values = []\nvalues.append(1)\nawait asyncio.sleep(.05)\nvalues.append(3)\nnotify(values)".into() }).unwrap();
    sender.send(Input::Execute { cell: 2, source: "values.append(2)\nprint(values)".into() }).unwrap();
    let mut finished_cells = 0;
    let mut messages = Vec::new();
    while finished_cells < 2 {
        match next(&mut events).await {
            Event::Text { cell, text, .. } => messages.push((cell, text.trim_end().to_owned())),
            Event::Finished { error, .. } => { assert!(error.is_none(), "{error:?}"); finished_cells += 1; }
            _ => {}
        }
    }
    assert_eq!(messages, vec![(2, "[1, 2]".into()), (1, "[1, 2, 3]".into())]);
}

#[tokio::test(flavor = "multi_thread")]
async fn native_events_keep_payload_limits_and_python_argument_semantics() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: r#"
try: print('x' * (1024 * 1024 + 1), max_tokens=1)
except Exception as error: raise AssertionError(repr(error))
print('Ω "quoted"')
set_max_wait(seconds=3)
suppress_tool_wakeups()
"#.into() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Text { text, max_tokens: 1, .. } if text.ends_with("[truncated]")));
    assert!(matches!(next(&mut events).await, Event::Text { text, .. } if text == "Ω \"quoted\"\n"));
    assert!(matches!(next(&mut events).await, Event::MaxWait { cell: 1, seconds: 3 }));
    assert!(matches!(next(&mut events).await, Event::SuppressToolWakeups { cell: 1 }));
    assert_eq!(finished(&mut events, 1).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_stops_awaiting_work_without_losing_notebook() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    sender.send(Input::Execute { cell: 1, source: "survives = 42\nasyncio.create_task(asyncio.sleep(3600))\nnotify('started')\nawait asyncio.Event().wait()".into() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Text { .. }));
    sender.cancel(1);
    assert!(finished(&mut events, 1).await.unwrap().contains("CancelledError"));
    sender.send(Input::Execute { cell: 2, source: "print(survives)".into() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Text { cell: 2, text, .. } if text.trim() == "42"));
    assert_eq!(finished(&mut events, 2).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn other_asyncio_loops_run_on_their_own_tokio_runtime() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: r#"
import threading
assert type(asyncio.get_running_loop()).__name__ == 'NotebookEventLoop'
async def work(n):
 await asyncio.sleep(.01)
 return n * 2
results = []
thread = threading.Thread(target=lambda: results.append(asyncio.run(work(21))))
thread.start()
await asyncio.to_thread(thread.join)
assert results == [42], results
def reuse():
 loop = asyncio.new_event_loop()
 try:
  ticks = []
  async def background():
   while True:
    ticks.append(1)
    await asyncio.sleep(0)
  task = loop.create_task(background())
  assert loop.run_until_complete(work(1)) == 2
  seen = len(ticks)
  assert loop.run_until_complete(work(2)) == 4
  assert len(ticks) > seen, 'background task stalled between runs'
  task.cancel()
  loop.run_until_complete(asyncio.sleep(0))
  assert task.cancelled()
  loop.call_later(.01, loop.stop)
  loop.run_forever()
 finally:
  loop.close()
await asyncio.to_thread(reuse)
"#.into() }).unwrap();
    assert_eq!(finished(&mut events, 1).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn gathered_errors_can_be_handled_and_child_tasks_stay_attributed() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: "async def child():\n await asyncio.sleep(.01)\n notify('child')\n raise ValueError('expected')\nresults = await asyncio.gather(child(), return_exceptions=True)\nassert isinstance(results[0], ValueError)".into() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Text { cell: 1, .. }));
    assert_eq!(finished(&mut events, 1).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_asyncio_queues_task_groups_timeouts_and_streams_work() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: r#"
queue = asyncio.Queue(maxsize=1)
lock, values = asyncio.Lock(), []
async def produce():
 for n in range(3): await queue.put(n)
async def consume():
 for _ in range(3):
  n = await queue.get()
  async with lock: values.append(n)
  queue.task_done()
async with asyncio.TaskGroup() as group:
 group.create_task(produce()); group.create_task(consume())
await queue.join()
assert values == [0, 1, 2]
for use_context in [True, False]:
 try:
  if use_context:
   async with asyncio.timeout(.01): await asyncio.Event().wait()
  else: await asyncio.wait_for(asyncio.Event().wait(), .01)
 except TimeoutError: pass
 else: raise AssertionError('timeout did not fire')
async def echo(reader, writer):
 writer.write((await reader.readline()).upper()); await writer.drain(); writer.close()
server = await asyncio.start_server(echo, '127.0.0.1', 0)
async with server:
 reader, writer = await asyncio.open_connection('localhost', server.sockets[0].getsockname()[1])
 writer.write(b'hello\n'); await writer.drain()
 assert await reader.readline() == b'HELLO\n'
 writer.close()
"#.into() }).unwrap();
    assert_eq!(finished(&mut events, 1).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_asyncio_thread_work_preserves_cell_context() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: "await asyncio.to_thread(notify, 'thread output')".into() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Text { cell: 1, text, .. } if text.trim() == "thread output"));
    assert_eq!(finished(&mut events, 1).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_asyncio_callbacks_keep_cells_alive_and_can_be_cancelled() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    sender.send(Input::Execute { cell: 1, source: "asyncio.get_running_loop().call_later(.01, notify, 'callback')".into() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Text { cell: 1, .. }));
    assert_eq!(finished(&mut events, 1).await, None);
    sender.send(Input::Execute { cell: 2, source: "asyncio.get_running_loop().call_later(3600, notify, 'too late')\nprint('scheduled')".into() }).unwrap();
    let scheduled = next(&mut events).await;
    assert!(matches!(scheduled, Event::Text { cell: 2, .. }), "{scheduled:?}");
    sender.cancel(2);
    assert!(finished(&mut events, 2).await.unwrap().contains("CancelledError"));
}

#[tokio::test(flavor = "multi_thread")]
async fn selector_callbacks_keep_returned_cells_alive_and_print_errors() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    let sender = session.sender();
    sender.send(Input::Execute { cell: 1, source: r#"
import socket
reader, writer = socket.socketpair()
loop = asyncio.get_running_loop()
def ready():
 reader.recv(1); loop.remove_reader(reader); reader.close(); raise ValueError('reader failure')
loop.add_reader(reader, ready)
print('watching')
"#.into() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Text { cell: 1, .. }));
    assert!(tokio::time::timeout(Duration::from_millis(20), events.recv()).await.is_err());
    sender.send(Input::Execute { cell: 2, source: "writer.send(b'x'); writer.close()".into() }).unwrap();
    let mut done = Vec::new();
    let mut printed = String::new();
    while done.len() < 2 {
        match next(&mut events).await {
            Event::Text { cell: 1, text, .. } => printed.push_str(&text),
            Event::Finished { cell, error: None } => done.push(cell),
            event => panic!("unexpected event: {event:?}"),
        }
    }
    assert!(printed.contains("ValueError: reader failure"), "{printed}");
    done.sort();
    assert_eq!(done, [1, 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn output_limits_and_standard_streams_preserve_attribution() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: "import sys\nprint('hello', 'world', end='!', max_tokens=12000)\nsys.stderr.write('diagnostic')".into() }).unwrap();
    let mut printed = String::new();
    loop {
        match next(&mut events).await {
            Event::Text { cell: 1, text, max_tokens, .. } => { assert_eq!(max_tokens, 10000); printed.push_str(&text); }
            Event::Finished { error: None, .. } => break,
            event => panic!("{event:?}"),
        }
    }
    assert_eq!(printed, "hello world!diagnostic");
}

#[tokio::test(flavor = "multi_thread")]
async fn pathlib_is_prebound_and_each_notebook_has_its_own_cwd() {
    let original = std::env::current_dir().unwrap();
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    let path_one = one.path().to_owned();
    let path_two = two.path().to_owned();
    let (first, mut first_events) = test_session(move || std::env::set_current_dir(path_one).map_err(|e| e.to_string())).unwrap();
    let (second, mut second_events) = test_session(move || std::env::set_current_dir(path_two).map_err(|e| e.to_string())).unwrap();
    first.sender().send(Input::Execute { cell: 1, source: "import os\nPath('sub').mkdir()\nos.chdir('sub')\nPath('note.txt').write_text('one')\nassert Path('note.txt').read_text() == 'one'".into() }).unwrap();
    second.sender().send(Input::Execute { cell: 1, source: "assert pathlib.Path is Path\nPath('note.txt').write_text('two')\nassert Path('note.txt').read_text() == 'two'".into() }).unwrap();
    assert_eq!(finished(&mut first_events, 1).await, None);
    assert_eq!(finished(&mut second_events, 1).await, None);
    assert_eq!(std::env::current_dir().unwrap(), original);
    assert_eq!(std::fs::read_to_string(one.path().join("sub/note.txt")).unwrap(), "one");
    assert_eq!(std::fs::read_to_string(two.path().join("note.txt")).unwrap(), "two");
}

#[tokio::test(flavor = "multi_thread")]
async fn notebooks_have_separate_globals_and_output() {
    let (first, mut first_events) = test_session(|| Ok(())).unwrap();
    let (second, mut second_events) = test_session(|| Ok(())).unwrap();
    first.sender().send(Input::Execute { cell: 1, source: "secret = 'first'\nprint('from first')".into() }).unwrap();
    assert!(matches!(next(&mut first_events).await, Event::Text { cell: 1, text, .. } if text == "from first\n"));
    assert_eq!(finished(&mut first_events, 1).await, None);
    second.sender().send(Input::Execute { cell: 1, source: "assert 'secret' not in globals()\nprint('from second')".into() }).unwrap();
    assert!(matches!(next(&mut second_events).await, Event::Text { cell: 1, text, .. } if text == "from second\n"));
    assert_eq!(finished(&mut second_events, 1).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn threads_keep_their_cell_alive_and_write_to_it() {
    let (session, mut events) = test_session(|| Ok(())).unwrap();
    session.sender().send(Input::Execute { cell: 1, source: "import threading, time\ndef work():\n time.sleep(.05)\n print('from thread')\nthreading.Thread(target=work).start()".into() }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Text { cell: 1, text, .. } if text == "from thread\n"));
    assert_eq!(finished(&mut events, 1).await, None);
}
