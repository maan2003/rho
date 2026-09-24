use std::io::Read as _;

use anyhow::bail;
use rho_agent_host_proto::agents::{Reply, Request};
use rho_visualizations::{MAX_VISUALIZATION_BYTES, SVG_MIME_TYPE};

use crate::{RecordVisualizationArgs, agents_request};

pub(crate) async fn run(args: RecordVisualizationArgs) -> anyhow::Result<()> {
    let mut content = Vec::new();
    std::io::stdin()
        .take((MAX_VISUALIZATION_BYTES + 1) as u64)
        .read_to_end(&mut content)?;
    if content.len() > MAX_VISUALIZATION_BYTES {
        bail!("visualization is too large (maximum {MAX_VISUALIZATION_BYTES} bytes)");
    }

    let socket_path = rho_agent_host_proto::RuntimePaths::resolve(args.socket_path)?
        .socket()
        .to_owned();
    let request = Request::RecordVisualization {
        mime_type: SVG_MIME_TYPE.to_owned(),
        content,
    };
    let Reply::VisualizationRecorded { id } = agents_request(&socket_path, request).await? else {
        bail!("unexpected reply from the daemon");
    };
    println!("{id}");
    Ok(())
}
