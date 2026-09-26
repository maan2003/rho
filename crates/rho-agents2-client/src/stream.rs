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
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof) =>
                {
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

#[cfg(test)]
mod tests {
    use futures::channel::mpsc;
    use rho_agent_types::{AgentIdDomain, UnixMs};
    use rho_rpc::protocol::{read_frame, write_frame};

    use super::*;
    use crate::protocol::{AgentId, ChatEvent, ChatKind};

    #[tokio::test]
    async fn unsupported_optional_stream_does_not_reconnect_legacy_host() {
        let (host_tx, mut host_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, _) = mpsc::unbounded();
        let stream = Agents2Stream::new(HostId(3), events);
        let running = tokio::spawn(stream.run(Dialer::InProcess(host_tx)));
        let mut far = host_rx.recv().await.unwrap();
        let open: rho_rpc::protocol::Open = read_frame(&mut far).await.unwrap();
        assert_eq!(open.protocol, rho_rpc::protocol::Protocol::Agents2);
        drop(far); // An older host rejects the unknown protocol at Open.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(80), running)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn snapshots_and_chat_stream_in_order_and_later_eof_reconnects() {
        let (host_tx, mut host_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut received) = mpsc::unbounded();
        let stream = Agents2Stream::new(HostId(9), events);
        let running = tokio::spawn(stream.run(Dialer::InProcess(host_tx)));
        let mut far = host_rx.recv().await.unwrap();
        let _: rho_rpc::protocol::Open = read_frame(&mut far).await.unwrap();
        write_frame(&mut far, &ServerFrame::Snapshot { agents: Vec::new() })
            .await
            .unwrap();
        let id = AgentId::from_counter(23, &AgentIdDomain(42)).unwrap();
        write_frame(
            &mut far,
            &ServerFrame::Chat {
                agent_id: id.clone(),
                event: ChatEvent {
                    seq: 2,
                    at: UnixMs(34),
                    kind: ChatKind::Status("ready".into()),
                },
            },
        )
        .await
        .unwrap();
        use futures::StreamExt as _;
        assert!(
            matches!(received.next().await.unwrap().frame, ServerFrame::Snapshot { agents } if agents.is_empty())
        );
        assert!(
            matches!(received.next().await.unwrap().frame, ServerFrame::Chat { agent_id, .. } if agent_id == id)
        );
        drop(far);
        assert!(running.await.unwrap().is_err());
    }
}
