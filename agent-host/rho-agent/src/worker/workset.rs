//! Legacy worker policy envelope and GUI relay. Sessions live in rho-workset.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub use rho_workset::workset::{Action, Attach, Reply};
pub(super) use rho_workset::workset::{Clients, Execution, decode, encode};
use tokio::sync::{mpsc, oneshot};

use super::transport::{Port, Sender};

#[derive(senax_encoder::Encode, senax_encoder::Decode)]
pub(super) enum Message {
    Policy(super::policy::Message),
    Action { id: u64, action: Action },
    Attach { id: u64, port: Port, attach: Attach },
    Reply { id: u64, body: Reply },
    Detach(Port),
}

/// One GUI attachment. It owns no PTY, shell, or transport connection.
pub struct Client {
    pub(super) port: Port,
    pub(super) sender: Sender,
    pub(super) incoming: mpsc::Receiver<bytes::Bytes>,
    pub(super) commands: mpsc::UnboundedSender<Message>,
}
impl Client {
    /// Relay one already-authenticated GUI stream without owning execution.
    pub async fn relay<R, W, I, O>(mut self, mut reader: R, mut writer: W) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
        I: senax_encoder::Unpacker + senax_encoder::Encoder,
        O: senax_encoder::Decoder + senax_encoder::Packer,
    {
        let input = async {
            while let Some((frame, _)) =
                rho_rpc::read_frame_optional::<_, I>(&mut reader, rho_rpc::protocol::MAX_FRAME_LEN)
                    .await?
            {
                self.sender.send(self.port, encode(&frame)?).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let output = async {
            while let Some(bytes) = self.incoming.recv().await {
                let frame: O = decode(&bytes)?;
                rho_rpc::write_frame(&mut writer, &frame, rho_rpc::protocol::MAX_FRAME_LEN).await?;
            }
            tokio::io::AsyncWriteExt::shutdown(&mut writer).await?;
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! { result = input => result, result = output => result }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.commands.send(Message::Detach(self.port));
    }
}

pub(super) type Pending = Arc<
    Mutex<
        HashMap<
            u64,
            (
                oneshot::Sender<Reply>,
                Option<tokio::sync::OwnedRwLockReadGuard<()>>,
            ),
        >,
    >,
>;
