//! A reconnecting host session: snapshot first, then only appended chat.
use futures::channel::mpsc::UnboundedSender;
use futures::future::BoxFuture;
use rho_agent_hosts::{Dialer, HostId, HostStream};
use rho_rpc::protocol::{read_frame, write_open};

use crate::protocol::{Open, ServerFrame};

pub struct Agents2Event {
    pub host: HostId,
    pub frame: ServerFrame,
}
pub struct Agents2Stream {
    host: HostId,
    sender: UnboundedSender<Agents2Event>,
}
impl Agents2Stream {
    pub fn new(host: HostId, sender: UnboundedSender<Agents2Event>) -> Self {
        Self { host, sender }
    }
}
impl HostStream for Agents2Stream {
    fn name(&self) -> &'static str {
        "agents2"
    }
    fn run(&self, dialer: Dialer) -> BoxFuture<'static, anyhow::Result<()>> {
        let host = self.host;
        let sender = self.sender.clone();
        Box::pin(async move {
            let mut stream = dialer.open(None).await?;
            write_open(&mut stream, &Open::Session).await?;
            // This stream is optional. A host from before Agents2 closes it
            // immediately; keeping it parked lets the legacy streams live.
            // After the first snapshot, a disconnect must reconnect normally.
            let first = match read_frame::<_, ServerFrame>(&mut stream).await {
                Ok(frame @ ServerFrame::Snapshot { .. }) => frame,
                Ok(_) => anyhow::bail!("agent2 session began without a snapshot"),
                Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|io|
                    io.kind() == std::io::ErrorKind::UnexpectedEof) => {
                    return std::future::pending().await;
                }
                Err(error) => return Err(error),
            };
            sender.unbounded_send(Agents2Event { host, frame: first })?;
            loop {
                let frame = read_frame::<_, ServerFrame>(&mut stream).await?;
                sender.unbounded_send(Agents2Event { host, frame })?;
            }
        })
    }
}
