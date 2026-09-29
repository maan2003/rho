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
    inference: crate::inference::Accounts,
    agent: rho_agent_types::AgentId,
    pool: std::sync::Weak<crate::host::pool::AgentPool>,
) -> std::sync::Arc<HostClient> {
    let (client, server) = pair();
    let services = std::sync::Arc::new(crate::host::services::Services::new(
        db,
        inference,
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
