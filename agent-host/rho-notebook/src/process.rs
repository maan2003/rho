//! Typed control channel for a notebook in its own process. Python and its
//! Rust tool runtime stay together; the owner receives only observations.
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use bytes::Bytes;
use senax_encoder::{Decode, Encode};
use tokio::sync::{Notify, mpsc as tokio_mpsc};

use crate::{CellHandle, Notebook, Report, SourceFacts, StreamProgress};

const MAX_FRAME: usize = 32 * 1024 * 1024;

#[derive(Debug, Encode, Decode)]
struct Request {
    id: u64,
    operation: Operation,
}

#[derive(Debug, Encode, Decode)]
enum Operation {
    Run(String),
    Stream,
    Feed { cell: u64, code: String, eof: bool },
    Stop(u64),
    Interrupt(u64),
    Cancel(u64),
    CancelAll,
    Fresh,
    Checkin,
    ResetCheckin,
    LatestCell,
    Facts,
    CellFacts(u64),
    Progress(u64),
    Report,
    Prepare,
    Resume,
    Ack(u64),
    Shutdown,
    ServiceReply { id: u64, answer: ServiceAnswer },
}

#[derive(Debug, Encode, Decode)]
enum Answer {
    Empty,
    Cell { id: u64, session: crate::SessionId },
    LatestCell(Option<(u64, crate::SessionId)>),
    Checkin { seconds: u64, nanos: u32 },
    Facts(Vec<SourceFacts>),
    CellFacts(SourceFacts),
    Progress(StreamProgress),
    Interrupted(Option<usize>),
    Report(Option<Report>),
}

#[derive(Debug, Encode, Decode)]
enum Message {
    Reply { id: u64, answer: RpcAnswer },
    Changed,
    ServiceRequest(ServiceRequest),
    Event { id: u64, event: Event },
}

#[derive(Debug, Encode, Decode)]
enum RpcAnswer {
    Ok(Answer),
    Err(String),
}

impl From<Result<Answer, String>> for RpcAnswer {
    fn from(value: Result<Answer, String>) -> Self {
        match value {
            Ok(value) => Self::Ok(value),
            Err(error) => Self::Err(error),
        }
    }
}

impl RpcAnswer {
    fn into_result(self) -> Result<Answer, String> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Err(error) => Err(error),
        }
    }
}

#[derive(Debug, Encode, Decode)]
enum ServiceAnswer {
    Ok(Vec<u8>),
    Err(String),
}

impl ServiceAnswer {
    fn into_result(self) -> Result<Vec<u8>, String> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Err(error) => Err(error),
        }
    }
}

/// The two owner services available to code running inside the child.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ServiceKind {
    HostCall,
    WebCredentials,
}

/// One request to the owner; respond through [`Client::service_reply`].
#[derive(Debug, Encode, Decode)]
pub struct ServiceRequest {
    pub id: u64,
    pub kind: ServiceKind,
    pub payload: Vec<u8>,
}

struct OutboundService {
    kind: ServiceKind,
    payload: Vec<u8>,
    reply: tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>,
}

/// Cloneable child-side handle for asynchronous owner services.
#[derive(Clone)]
pub struct ServiceClient(tokio_mpsc::UnboundedSender<OutboundService>);

impl ServiceClient {
    /// Give the child a handle and its process server the receiving half.
    pub fn channel() -> (Self, ServiceReceiver) {
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        (Self(tx), ServiceReceiver(rx))
    }

    pub async fn request(&self, kind: ServiceKind, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.0
            .send(OutboundService {
                kind,
                payload,
                reply: tx,
            })
            .map_err(|_| "notebook service disconnected".to_owned())?;
        rx.await
            .map_err(|_| "notebook service disconnected".to_owned())?
    }
}

/// The receiving half consumed by the child process server.
pub struct ServiceReceiver(tokio_mpsc::UnboundedReceiver<OutboundService>);

/// Notebook-originated agent messages; never Python objects on the wire.
#[derive(Clone, Debug, Encode, Decode)]
pub enum Event {
    Send { cell: u64, text: String },
    Status(String),
    EndTurn,
    Archive,
}

/// The worker side. Calls are synchronous so existing cell admission and
/// source-reading code need no async wrapper; notifications are independent.
pub struct Client {
    writer: Mutex<UnixStream>,
    next: AtomicU64,
    waiting: Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Answer, String>>>>>,
}

impl Client {
    pub fn connect(stream: UnixStream, wake: Arc<Notify>) -> Result<Self, String> {
        Self::connect_with_services(stream, wake, None, None)
    }

    pub fn connect_with_events(
        stream: UnixStream,
        wake: Arc<Notify>,
        events: Option<tokio_mpsc::UnboundedSender<(u64, Event)>>,
    ) -> Result<Self, String> {
        Self::connect_with_services(stream, wake, events, None)
    }

    pub fn connect_with_services(
        stream: UnixStream,
        wake: Arc<Notify>,
        events: Option<tokio_mpsc::UnboundedSender<(u64, Event)>>,
        services: Option<tokio_mpsc::UnboundedSender<ServiceRequest>>,
    ) -> Result<Self, String> {
        let reader = stream.try_clone().map_err(|e| e.to_string())?;
        let waiting: Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Answer, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending = Arc::clone(&waiting);
        thread::spawn(move || {
            let mut reader = reader;
            while let Ok(message) = read_frame::<Message>(&mut reader) {
                match message {
                    Message::Reply { id, answer } => {
                        if let Some(reply) = pending.lock().unwrap().remove(&id) {
                            let _ = reply.send(answer.into_result());
                        }
                    }
                    Message::Changed => wake.notify_one(),
                    Message::ServiceRequest(request) => {
                        if let Some(services) = &services {
                            let _ = services.send(request);
                        }
                        wake.notify_one();
                    }
                    Message::Event { id, event } => {
                        if let Some(events) = &events {
                            let _ = events.send((id, event));
                        }
                        wake.notify_one();
                    }
                }
            }
            pending.lock().unwrap().clear();
            wake.notify_one();
        });
        Ok(Self {
            writer: Mutex::new(stream),
            next: AtomicU64::new(1),
            waiting,
        })
    }

    fn ask(&self, operation: Operation) -> Result<Answer, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        if let Err(error) = write_frame(
            &mut *self.writer.lock().unwrap(),
            &Request { id, operation },
        ) {
            self.waiting.lock().unwrap().remove(&id);
            return Err(error);
        }
        // A worker may have only a few Tokio threads, while the child asks
        // the owner for services on that same runtime. Yield this thread's
        // executor slot while waiting for the reader thread's reply.
        let reply = || rx.recv();
        let received = match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(reply)
            }
            _ => reply(),
        };
        received.map_err(|_| "notebook process disconnected".to_owned())?
    }

    pub fn run(&self, code: String) -> Result<u64, String> {
        match self.ask(Operation::Run(code))? {
            Answer::Cell { id, .. } => Ok(id),
            _ => Err("unexpected notebook reply".into()),
        }
    }

    pub fn latest_cell(&self) -> Result<Option<(u64, crate::SessionId)>, String> {
        match self.ask(Operation::LatestCell)? {
            Answer::LatestCell(cell) => Ok(cell),
            _ => Err("unexpected notebook reply".into()),
        }
    }

    pub fn checkin(&self) -> Result<std::time::Duration, String> {
        match self.ask(Operation::Checkin)? {
            Answer::Checkin { seconds, nanos } => Ok(std::time::Duration::new(seconds, nanos)),
            _ => Err("unexpected notebook reply".into()),
        }
    }

    pub fn reset_checkin(&self) -> Result<(), String> {
        self.ask(Operation::ResetCheckin)?;
        Ok(())
    }

    pub fn fresh(&self) -> Result<(), String> {
        self.ask(Operation::Fresh)?;
        Ok(())
    }

    pub fn stream(&self) -> Result<u64, String> {
        match self.ask(Operation::Stream)? {
            Answer::Cell { id, .. } => Ok(id),
            _ => Err("unexpected notebook reply".into()),
        }
    }

    pub fn feed(&self, cell: u64, code: String, eof: bool) -> Result<(), String> {
        self.ask(Operation::Feed { cell, code, eof })?;
        Ok(())
    }

    pub fn stop(&self, cell: u64) -> Result<(), String> {
        self.ask(Operation::Stop(cell))?;
        Ok(())
    }

    pub fn interrupt(&self, cell: u64) -> Result<Option<usize>, String> {
        match self.ask(Operation::Interrupt(cell))? {
            Answer::Interrupted(admitted) => Ok(admitted),
            _ => Err("unexpected notebook reply".into()),
        }
    }

    pub fn cancel(&self, cell: u64) -> Result<(), String> {
        self.ask(Operation::Cancel(cell))?;
        Ok(())
    }

    pub fn facts(&self) -> Result<Vec<SourceFacts>, String> {
        match self.ask(Operation::Facts)? {
            Answer::Facts(facts) => Ok(facts),
            _ => Err("unexpected notebook reply".into()),
        }
    }

    pub fn cell_facts(&self, cell: u64) -> Result<SourceFacts, String> {
        match self.ask(Operation::CellFacts(cell))? {
            Answer::CellFacts(facts) => Ok(facts),
            _ => Err("unexpected notebook reply".into()),
        }
    }

    pub fn progress(&self, cell: u64) -> Result<StreamProgress, String> {
        match self.ask(Operation::Progress(cell))? {
            Answer::Progress(progress) => Ok(progress),
            _ => Err("unexpected notebook reply".into()),
        }
    }

    pub fn report(&self) -> Result<Option<Report>, String> {
        match self.ask(Operation::Report)? {
            Answer::Report(report) => Ok(report),
            _ => Err("unexpected notebook reply".into()),
        }
    }

    pub fn cancel_all(&self) -> Result<(), String> {
        self.ask(Operation::CancelAll)?;
        Ok(())
    }

    pub fn prepare(&self) -> Result<(), String> {
        self.ask(Operation::Prepare)?;
        Ok(())
    }

    pub fn ack(&self, id: u64) -> Result<(), String> {
        self.ask(Operation::Ack(id))?;
        Ok(())
    }

    pub fn resume(&self) -> Result<(), String> {
        self.ask(Operation::Resume)?;
        Ok(())
    }

    pub fn service_reply(&self, id: u64, answer: Result<Vec<u8>, String>) -> Result<(), String> {
        let answer = match answer {
            Ok(value) => ServiceAnswer::Ok(value),
            Err(error) => ServiceAnswer::Err(error),
        };
        self.ask(Operation::ServiceReply { id, answer })?;
        Ok(())
    }

    pub fn shutdown(&self) -> Result<(), String> {
        self.ask(Operation::Shutdown)?;
        Ok(())
    }
}

/// Serve one owner connection. The worker can replace the connection on
/// restore via CRIU's inherited-fd mechanism, but the notebook stays intact.
pub async fn serve(
    notebook: Notebook,
    socket: UnixStream,
    wake: Arc<Notify>,
) -> Result<(), String> {
    serve_with_services(notebook, socket, wake, None, None, None).await
}

/// Serve notebook state and child-originated service calls on one socket.
pub async fn serve_with_services(
    mut notebook: Notebook,
    socket: UnixStream,
    wake: Arc<Notify>,
    fresh: Option<Box<dyn Fn() -> Result<Notebook, String> + Send + Sync>>,
    mut events: Option<tokio_mpsc::UnboundedReceiver<Event>>,
    mut services: Option<ServiceReceiver>,
) -> Result<(), String> {
    let mut reader = socket.try_clone().map_err(|e| e.to_string())?;
    let writer = Arc::new(Mutex::new(socket));
    let (tx, mut requests) = tokio_mpsc::unbounded_channel();
    thread::spawn(move || {
        while let Ok(request) = read_frame::<Request>(&mut reader) {
            if tx.send(request).is_err() {
                break;
            }
        }
    });
    let mut cells: HashMap<u64, CellHandle> = HashMap::new();
    let mut prepared = false;
    let mut outstanding = HashSet::new();
    let mut queued = VecDeque::new();
    let mut next_event = 1u64;
    let mut next_service = 1u64;
    let mut service_waiting = HashMap::new();
    let mut queued_services: VecDeque<OutboundService> = VecDeque::new();
    loop {
        tokio::select! {
            biased;
            request = requests.recv() => {
                let Some(request) = request else { break };
                let id = request.id;
                let shutdown = matches!(request.operation, Operation::Shutdown);
                let answer = if prepared && matches!(
                    request.operation, Operation::Run(_) | Operation::Stream | Operation::Feed { .. }
                ) {
                    Err("notebook is prepared for checkpoint".into())
                } else { match request.operation {
                    Operation::Prepare => {
                        // Drain notifications already submitted before deciding whether
                        // the child is quiescent. A newly arriving one is queued below.
                        if let Some(events) = events.as_mut() {
                            while let Ok(event) = events.try_recv() {
                                if prepared { queued.push_back(event); } else {
                                    let event_id = next_event;
                                    next_event += 1;
                                    outstanding.insert(event_id);
                                    write_frame(&mut *writer.lock().unwrap(), &Message::Event { id: event_id, event })?;
                                }
                            }
                        }
                        if let Some(services) = services.as_mut() {
                            while let Ok(service) = services.0.try_recv() {
                                if prepared {
                                    queued_services.push_back(service);
                                } else {
                                    let request = ServiceRequest { id: next_service, kind: service.kind, payload: service.payload };
                                    service_waiting.insert(next_service, service.reply);
                                    next_service += 1;
                                    write_frame(&mut *writer.lock().unwrap(), &Message::ServiceRequest(request))?;
                                }
                            }
                        }
                        if !outstanding.is_empty() || !queued.is_empty() {
                            Err("notebook events awaiting agent log".into())
                        } else if !service_waiting.is_empty() || !queued_services.is_empty() {
                            Err("notebook services awaiting owner".into())
                        } else if notebook.facts().iter().any(|facts|
                            matches!(facts.kind, crate::Kind::Command | crate::Kind::Call)
                            && facts.finished.is_none()
                        ) {
                            Err("host work is still running".into())
                        } else {
                            prepared = true;
                            Ok(Answer::Empty)
                        }
                    }
                    Operation::Resume => {
                        prepared = false;
                        while let Some(service) = queued_services.pop_front() {
                            let request = ServiceRequest { id: next_service, kind: service.kind, payload: service.payload };
                            service_waiting.insert(next_service, service.reply);
                            next_service += 1;
                            write_frame(&mut *writer.lock().unwrap(), &Message::ServiceRequest(request))?;
                        }
                        while let Some(event) = queued.pop_front() {
                            let event_id = next_event;
                            next_event += 1;
                            outstanding.insert(event_id);
                            write_frame(&mut *writer.lock().unwrap(), &Message::Event { id: event_id, event })?;
                        }
                        Ok(Answer::Empty)
                    }
                    Operation::ServiceReply { id, answer } => {
                        match service_waiting.remove(&id) {
                            Some(reply) => { let _ = reply.send(answer.into_result()); Ok(Answer::Empty) }
                            None => Err("unknown notebook service request".into()),
                        }
                    }
                    Operation::Ack(event_id) => {
                        if !outstanding.remove(&event_id) {
                            Err("unknown notebook event".into())
                        } else {
                            Ok(Answer::Empty)
                        }
                    }
                    Operation::Checkin => {
                        let wait = notebook.checkin();
                        Ok(Answer::Checkin { seconds: wait.as_secs(), nanos: wait.subsec_nanos() })
                    }
                    Operation::ResetCheckin => { notebook.reset_checkin(); Ok(Answer::Empty) }
                    Operation::LatestCell => {
                        Ok(Answer::LatestCell(cells.iter().max_by_key(|(id, _)| *id)
                            .map(|(id, cell)| (*id, cell.session_id()))))
                    }
                    Operation::Fresh => {
                        match &fresh {
                            Some(fresh) => {
                                notebook.cancel();
                                match notebook.shutdown().await {
                                    Ok(()) => match fresh() {
                                        Ok(replacement) => {
                                            notebook = replacement;
                                            cells.clear();
                                            Ok(Answer::Empty)
                                        }
                                        Err(error) => Err(error),
                                    },
                                    Err(error) => Err(error),
                                }
                            },
                            None => Err("notebook cannot be replaced".into()),
                        }
                    }
                    Operation::Run(code) => {
                        let cell = notebook.run(code);
                        let id = cell.id();
                        cells.insert(id, cell);
                        Ok(Answer::Cell { id, session: cells[&id].session_id() })
                    }
                    Operation::Stream => {
                        let cell = notebook.stream();
                        let id = cell.id();
                        cells.insert(id, cell);
                        Ok(Answer::Cell { id, session: cells[&id].session_id() })
                    }
                    Operation::Feed { cell, code, eof } => cells.get(&cell)
                        .ok_or_else(|| "unknown cell".to_owned())
                        .and_then(|cell| cell.feed(code, eof)).map(|()| Answer::Empty),
                    Operation::Stop(id) => cells.get(&id).map(|cell| cell.stop())
                        .ok_or_else(|| "unknown cell".to_owned()).map(|()| Answer::Empty),
                    Operation::Interrupt(id) => cells.get(&id).map(|cell| Answer::Interrupted(cell.interrupt()))
                        .ok_or_else(|| "unknown cell".to_owned()),
                    Operation::Cancel(id) => cells.get(&id).map(|cell| cell.cancel())
                        .ok_or_else(|| "unknown cell".to_owned()).map(|()| Answer::Empty),
                    Operation::CancelAll => { notebook.cancel(); Ok(Answer::Empty) }
                    Operation::Facts => Ok(Answer::Facts(notebook.facts())),
                    Operation::CellFacts(id) => cells.get(&id).map(|cell| Answer::CellFacts(cell.facts()))
                        .ok_or_else(|| "unknown cell".to_owned()),
                    Operation::Progress(id) => cells.get(&id).map(|cell| Answer::Progress(cell.progress()))
                        .ok_or_else(|| "unknown cell".to_owned()),
                    Operation::Report => {
                        let report = notebook.report();
                        let latest = cells.keys().copied().max();
                        let live: HashSet<_> = notebook.facts().into_iter()
                            .filter(|facts| facts.kind == crate::Kind::Cell)
                            .map(|facts| facts.session_id).collect();
                        cells.retain(|id, cell| Some(*id) == latest || live.contains(&cell.session_id()));
                        Ok(Answer::Report(report))
                    },
                    Operation::Shutdown => notebook.shutdown().await.map(|()| Answer::Empty),
                }};
                write_frame(&mut *writer.lock().unwrap(), &Message::Reply { id, answer: answer.into() })?;
                if shutdown { break; }
            }
            service = async {
                match services.as_mut() {
                    Some(services) => services.0.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(service) = service {
                    if prepared {
                        queued_services.push_back(service);
                    } else {
                        let request = ServiceRequest { id: next_service, kind: service.kind, payload: service.payload };
                        service_waiting.insert(next_service, service.reply);
                        next_service += 1;
                        write_frame(&mut *writer.lock().unwrap(), &Message::ServiceRequest(request))?;
                    }
                } else {
                    services = None;
                }
            }
            event = async {
                match events.as_mut() {
                    Some(events) => events.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(event) = event {
                    if prepared {
                        queued.push_back(event);
                    } else {
                        let event_id = next_event;
                        next_event += 1;
                        outstanding.insert(event_id);
                        write_frame(&mut *writer.lock().unwrap(), &Message::Event { id: event_id, event })?;
                    }
                } else {
                    events = None;
                }
            }
            () = wake.notified() => {
                write_frame(&mut *writer.lock().unwrap(), &Message::Changed)?;
            }
            else => break,
        }
    }
    Ok(())
}

fn write_frame<T: senax_encoder::Encoder>(
    stream: &mut UnixStream,
    value: &T,
) -> Result<(), String> {
    let data = senax_encoder::encode(value).map_err(|e| e.to_string())?;
    let len: u32 = data
        .len()
        .try_into()
        .map_err(|_| "notebook frame too large")?;
    if data.len() > MAX_FRAME {
        return Err("notebook frame too large".into());
    }
    stream
        .write_all(&len.to_le_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(&data).map_err(|e| e.to_string())
}

fn read_frame<T: senax_encoder::Decoder>(stream: &mut UnixStream) -> Result<T, String> {
    let mut len = [0; 4];
    stream.read_exact(&mut len).map_err(|e| e.to_string())?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err("notebook frame too large".into());
    }
    let mut data = vec![0; len];
    stream.read_exact(&mut data).map_err(|e| e.to_string())?;
    let mut bytes = Bytes::from(data);
    let value = senax_encoder::decode(&mut bytes).map_err(|e| e.to_string())?;
    if !bytes.is_empty() {
        return Err("trailing notebook frame data".into());
    }
    Ok(value)
}
