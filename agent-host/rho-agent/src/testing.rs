//! In-process fixtures using the production Unix transport and service
//! handlers.
use crate::ipc::{protocol, transport};
use crate::worker::host_client::HostClient;
pub struct Endpoint {
    pub(crate) sender: transport::Sender,
    pub(crate) port: transport::Port,
    pub(crate) incoming: tokio::sync::mpsc::UnboundedReceiver<transport::Packet>,
}
impl Endpoint {
    pub fn host(self) -> std::sync::Arc<HostClient> {
        HostClient::connect(
            self.sender.clone(),
            self.sender,
            self.port,
            self.incoming,
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        )
    }
    pub(crate) async fn read(&mut self) -> std::io::Result<protocol::Message<'static>> {
        protocol::decode(
            &self
                .incoming
                .recv()
                .await
                .ok_or(std::io::ErrorKind::UnexpectedEof)?
                .bytes,
        )
    }
    pub(crate) async fn write(&self, message: &protocol::Message<'_>) -> std::io::Result<()> {
        self.sender
            .send(self.port, protocol::encode(message)?)
            .await
    }
}
/// A host-side route for `incoming`, each packet with its own in-flight
/// token.
pub(crate) fn route(
    mut incoming: tokio::sync::mpsc::UnboundedReceiver<transport::Packet>,
) -> tokio::sync::mpsc::UnboundedReceiver<(transport::Packet, std::sync::Arc<()>)> {
    let (route, routed) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(packet) = incoming.recv().await {
            if route.send((packet, std::sync::Arc::new(()))).is_err() {
                break;
            }
        }
    });
    routed
}

pub fn pair() -> (Endpoint, Endpoint) {
    let (left, right) = tokio::net::UnixStream::pair().unwrap();
    fn endpoint(socket: tokio::net::UnixStream) -> Endpoint {
        let (sender, mut receiver, writer) = transport::connect(socket);
        let (incoming, messages) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::select! {
                _ = incoming.closed() => {}
                _ = async {
                    while let Ok(packet) = receiver.next().await {
                        if incoming.send(packet).is_err() { break; }
                    }
                } => {}
            }
            writer.abort();
        });
        Endpoint {
            sender,
            port: transport::Port::Workset,
            incoming: messages,
        }
    }
    (endpoint(left), endpoint(right))
}

pub(crate) fn services_pair(
    db: rho_db::RhoDb,
    agent: rho_agent_types::AgentId,
    pool: std::sync::Weak<crate::host::pool::AgentPool>,
) -> std::sync::Arc<HostClient> {
    let (client, server) = pair();
    let services = std::sync::Arc::new(crate::host::services::Services::new(
        db,
        agent,
        pool,
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
    ));
    tokio::spawn(async move {
        let _ = services
            .serve(
                server.sender.clone(),
                server.sender,
                server.port,
                route(server.incoming),
            )
            .await;
    });
    client.host()
}

/// The policy every test notebook gets: fixed instructions, a `human`
/// whose sends are messages in one topic of the agent's, mentioning the
/// user unless they are a status, and a fixed text after a compaction.
const TEST_POLICY: &str = r#"
import _rho
from rho import hooks

hooks.instructions = lambda agent: "You are a test agent."

def on_notebook(agent, ns):
    topic = []

    class Human:
        def send(self, text, kind):
            mentions = [] if kind == "status" else [("user", kind)]
            if topic:
                _rho.post(agent.id, topic[0], text, mentions=mentions)
            else:
                topic.append(_rho.start_topic(agent.id, "rho", "test", text, mentions=mentions)[0])
    ns["human"] = Human()

hooks.on_notebook = on_notebook
hooks.on_compaction = lambda agent: [f"Kept after compaction: {agent.id}"]
hooks.on_turn_end = lambda agent, outcome: None
hooks.deliver = lambda agent: None
"#;

/// The stores of the agents tests registered, by agent.
static AGENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<rho_agent_types::AgentId, rho_db::RhoDb>>,
> = std::sync::LazyLock::new(Default::default);

/// Start the process's workset Python once, with [`TEST_POLICY`], and
/// answer its `_rho` calls from the store `agent` was registered with.
pub(crate) fn python(agent: rho_agent_types::AgentId, db: &rho_db::RhoDb) {
    use crate::ipc::workset::{PyReply, PyRequest};
    use crate::store::{ChatWrite as _, NewMessage, StoreRead as _};
    AGENTS.lock().expect("poison").insert(agent, db.clone());
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        // Its own runtime: a test's ends with the test.
        static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> =
            std::sync::LazyLock::new(|| tokio::runtime::Runtime::new().unwrap());
        let init = tempfile::Builder::new()
            .suffix(".py")
            .tempfile()
            .unwrap()
            .into_temp_path()
            .keep()
            .unwrap();
        std::fs::write(&init, TEST_POLICY).unwrap();
        let call: crate::worker::python::HostCall = std::sync::Arc::new(|request| {
            Box::pin(async move {
                let db = |agent| {
                    AGENTS
                        .lock()
                        .expect("poison")
                        .get(&agent)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("no test agent {}", agent.encoded()))
                };
                Ok(match request {
                    PyRequest::Agents => {
                        let agents = AGENTS.lock().expect("poison").clone();
                        PyReply::Agents(
                            agents
                                .into_iter()
                                .filter_map(|(agent, db)| {
                                    db.read().agent(agent).map(|head| (agent, head.spec))
                                })
                                .collect(),
                        )
                    }
                    PyRequest::Post {
                        sender,
                        topic,
                        mentions,
                        content,
                    } => {
                        let db = db(sender)?;
                        let mut write = db.write().await;
                        let message = NewMessage {
                            from: rho_agent_types::Address::Agent(sender),
                            mentions,
                            content,
                        };
                        match write.post(rho_agent_types::UnixMs::now(), topic, message, None) {
                            Ok(id) => {
                                write.commit();
                                PyReply::Seq(id.0)
                            }
                            Err(error) => PyReply::Error(format!("{error:#}")),
                        }
                    }
                    PyRequest::StartTopic {
                        sender,
                        stream,
                        name,
                        mentions,
                        content,
                    } => {
                        let db = db(sender)?;
                        let mut write = db.write().await;
                        let now = rho_agent_types::UnixMs::now();
                        let first = NewMessage {
                            from: rho_agent_types::Address::Agent(sender),
                            mentions,
                            content,
                        };
                        let started = write
                            .stream(now, stream)
                            .and_then(|stream| write.start_topic(now, stream, name, first, None));
                        match started {
                            Ok((topic, first)) => {
                                write.commit();
                                let message = crate::store::ChatRead::message(&db.read(), first)
                                    .expect("just posted");
                                debug_assert_eq!(message.topic, topic);
                                PyReply::Messages(vec![message])
                            }
                            Err(error) => PyReply::Error(format!("{error:#}")),
                        }
                    }
                    _ => PyReply::Error("tests answer only agents and chat".into()),
                })
            })
        });
        crate::worker::python::start(
            crate::worker::python::PyHost {
                call,
                runtime: RUNTIME.handle().clone(),
                workset: rho_agent_types::WorksetId([0; 6]),
                isolated: false,
            },
            Some(camino::Utf8PathBuf::from_path_buf(init).unwrap()),
        )
        .unwrap();
    });
}
