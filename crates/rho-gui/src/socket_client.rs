#![allow(dead_code)]

use std::io::BufWriter;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context as _, Result, anyhow};
use tau_proto::{
    ClientKind, EventSelector, HarnessInputMessage, Hello, PROTOCOL_VERSION, PeerInputMessage,
    PeerInputReader, PeerOutputWriter, Subscribe,
};

pub(crate) type Writer = Arc<Mutex<PeerOutputWriter<BufWriter<UnixStream>>>>;

pub(crate) enum SocketEvent {
    Message(PeerInputMessage),
    Disconnected(String),
}

pub(crate) fn spawn(socket_path: PathBuf, tx: mpsc::Sender<SocketEvent>) -> Result<Writer> {
    let stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
    let read_stream = stream.try_clone().context("failed to clone socket")?;
    let writer = Arc::new(Mutex::new(PeerOutputWriter::new(BufWriter::new(stream))));

    send_message(
        &writer,
        &HarnessInputMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "rho-gui".into(),
            client_kind: ClientKind::Ui,
        }),
    )?;
    send_message(
        &writer,
        &HarnessInputMessage::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Prefix("ui.".to_owned()),
                EventSelector::Prefix("action.".to_owned()),
                EventSelector::Prefix("provider.".to_owned()),
                EventSelector::Prefix("tool.".to_owned()),
                EventSelector::Prefix("extension.".to_owned()),
                EventSelector::Prefix("agent.".to_owned()),
                EventSelector::Prefix("harness.".to_owned()),
                EventSelector::Prefix("shell.".to_owned()),
                EventSelector::Prefix("term.".to_owned()),
                EventSelector::Prefix("factory.".to_owned()),
            ],
        }),
    )?;

    std::thread::spawn(move || {
        let mut reader = PeerInputReader::new(read_stream);
        loop {
            match reader.read_message() {
                Ok(Some(message)) => {
                    if tx.send(SocketEvent::Message(message)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    if tx
                        .send(SocketEvent::Disconnected("eof".to_owned()))
                        .is_err()
                    {
                        return;
                    }
                    return;
                }
                Err(error) => {
                    if tx
                        .send(SocketEvent::Disconnected(error.to_string()))
                        .is_err()
                    {
                        return;
                    }
                    return;
                }
            }
        }
    });

    Ok(writer)
}

pub(crate) fn send_message(writer: &Writer, message: &HarnessInputMessage) -> Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| anyhow!("socket writer mutex poisoned"))?;
    writer
        .write_message(message)
        .map_err(|error| anyhow!(error))?;
    writer.flush().context("failed to flush socket message")
}
