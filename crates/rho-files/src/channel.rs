//! Dialing a workspace file channel on a host.

use std::future::Future;

use futures::channel::mpsc as futures_mpsc;
use rho_agent_types::WorkspaceInfo;
use rho_rpc::protocol::{Opened, read_frame, write_open};

use crate::protocol::{MAX_WORKSPACE_FRAME_LEN, Open, WorkspaceClientFrame, WorkspaceServerFrame};

/// Dials a dedicated workspace file stream on the host `link` reaches and
/// runs the handshake.
pub fn open(
    link: &rho_hosts::Link,
    workspace: WorkspaceInfo,
) -> impl Future<Output = anyhow::Result<WorkspaceChannel>> + Send + 'static {
    link.run(|dialer| dial(dialer, workspace))
}

/// One workspace file channel. Dropping the owner cancels the transport and
/// tears down its daemon-side watcher.
pub struct WorkspaceChannel {
    pub outgoing: futures_mpsc::Sender<WorkspaceClientFrame>,
    pub incoming: futures_mpsc::Receiver<anyhow::Result<WorkspaceServerFrame>>,
    pub transport: rho_rpc::ChannelTask,
}

async fn dial(
    dialer: rho_hosts::Dialer,
    workspace: WorkspaceInfo,
) -> anyhow::Result<WorkspaceChannel> {
    let mut stream = dialer.open(None).await?;
    write_open(&mut stream, &Open { workspace }).await?;
    if let Opened::Refused { reason } = read_frame(&mut stream).await? {
        anyhow::bail!("daemon refused workspace file channel: {reason}")
    }

    let channel = stream.into_channel(rho_rpc::ChannelConfig {
        tx_limit: MAX_WORKSPACE_FRAME_LEN,
        rx_limit: MAX_WORKSPACE_FRAME_LEN,
        tx_capacity: 16,
        rx_capacity: 32,
    });
    let (outgoing, incoming, transport) = channel.into_parts();
    Ok(WorkspaceChannel {
        outgoing,
        incoming,
        transport,
    })
}
