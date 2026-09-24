//! The desktops part of the agent host,
//! [`rho_rpc::protocol::Protocol::Desktop`]: which desktops the worksets run. A
//! live view of one is served where the iroh connection's media are; see
//! `run_iroh_listener`.

use std::sync::Arc;

use rho_desktop_client::protocol::Open;
use rho_rpc::protocol::{Opened, write_frame};

use crate::Services;

/// Serves one desktops stream, whichever kind its opening frame asked for.
pub(crate) async fn serve<R, W>(
    services: Arc<Services>,
    open: Open,
    reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match open {
        Open::Sessions => serve_sessions(services, reader, writer).await,
        Open::Wayland { .. } => {
            write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: "Wayland streams need an iroh connection".to_owned(),
                },
            )
            .await
        }
    }
}

/// The desktops in every workset, told whole whenever they change. Only
/// changes cross the stream; discovery never starts an encoder.
async fn serve_sessions<R, W>(
    services: Arc<Services>,
    mut reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut previous = Vec::new();
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        tokio::select! {
            // The client says nothing; its end of the stream is the end.
            closed = rho_rpc::protocol::read_frame_optional::<_, ()>(&mut reader) => {
                return closed.map(|_| ());
            }
            _ = timer.tick() => {}
        }
        let mut sessions = Vec::new();
        for process in services.pool.executions().await {
            match process.action(rho_agent::WorksetAction::DesktopList).await {
                Ok(rho_agent::WorksetReply::DesktopSessions(entries)) => sessions.extend(entries),
                Ok(_) => tracing::warn!("unexpected desktop discovery reply"),
                Err(error) => tracing::debug!(%error, "desktop discovery unavailable"),
            }
        }
        sessions.sort();
        sessions.dedup();
        if sessions != previous {
            write_frame(&mut writer, &sessions).await?;
            previous = sessions;
        }
    }
}
