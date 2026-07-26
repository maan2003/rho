use std::io::Read as _;

use anyhow::bail;
use rho_ui_proto::{ClientMessage, ServerMessage};
use rho_visualizations::{MAX_VISUALIZATION_BYTES, SVG_MIME_TYPE};

use crate::{RecordVisualizationArgs, connect_or_start_daemon, default_socket_path};

pub(crate) async fn run(args: RecordVisualizationArgs) -> anyhow::Result<()> {
    let mut content = Vec::new();
    std::io::stdin()
        .take((MAX_VISUALIZATION_BYTES + 1) as u64)
        .read_to_end(&mut content)?;
    if content.len() > MAX_VISUALIZATION_BYTES {
        bail!("visualization is too large (maximum {MAX_VISUALIZATION_BYTES} bytes)");
    }

    let socket_path = args.socket_path.unwrap_or(default_socket_path()?);
    let mut daemon = connect_or_start_daemon(&socket_path, &args.auth).await?;
    daemon
        .send(&ClientMessage::RecordVisualization {
            mime_type: SVG_MIME_TYPE.to_owned(),
            content,
        })
        .await?;
    loop {
        match daemon.recv().await? {
            ServerMessage::VisualizationRecorded { id } => {
                println!("{id}");
                return Ok(());
            }
            ServerMessage::Error { message } => bail!(message),
            _ => {}
        }
    }
}
