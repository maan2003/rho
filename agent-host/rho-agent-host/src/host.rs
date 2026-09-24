//! The machine part of the agent host, [`rho_rpc::protocol::Protocol::Host`]:
//! Git transport and administration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use rho_agent_hosts::protocol::{
    GitProvided, GitProviderFrame, GitTransportPolicy, GuiTelemetryUpload, IrohApprove, IrohRevoke,
    IrohTrustInMemory, Open, PlatformSecretsSet, PlatformStatus, Pr, PrOutput, Request, Snapshot,
};
use rho_rpc::protocol::{Answer, Call, Opened, write_frame};
use tokio::sync::mpsc;

use crate::{GitProviderClaim, Services, debug};

/// Serves one host stream, whichever kind its opening frame asked for.
pub(crate) async fn serve<R, W>(
    services: Arc<Services>,
    iroh_auth: Option<rho_iroh_auth::IrohAuth>,
    open: Open,
    reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match open {
        Open::GitProvider => serve_git_provider(services, reader, writer).await,
        Open::Request(request) => {
            serve_call(&services, iroh_auth.as_ref(), request, &mut writer).await
        }
        Open::GitTransport { request } => {
            serve_git_transport_request(services, reader, writer, request).await
        }
        Open::GitProvide {
            request_id,
            provider_id,
            claim,
        } => {
            serve_git_transport_provider(services, reader, writer, request_id, provider_id, claim)
                .await
        }
    }
}

/// A GUI carrying SSH Git transport: registered with the broker for as
/// long as its stream is open, and told each request and its end.
async fn serve_git_provider<R, W>(
    services: Arc<Services>,
    mut reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (frames_tx, mut frames_rx) = mpsc::unbounded_channel::<GitProviderFrame>();
    services.git_transport.register(frames_tx).await;
    loop {
        tokio::select! {
            closed = rho_rpc::protocol::read_frame_optional::<_, ()>(&mut reader) => {
                return closed.map(|_| ());
            }
            Some(frame) = frames_rx.recv() => write_frame(&mut writer, &frame).await?,
        }
    }
}

async fn serve_git_transport_request<R, W>(
    services: Arc<Services>,
    reader: R,
    mut writer: W,
    request: rho_agent_hosts::protocol::GitTransportRequest,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let provider = match services.git_transport.request(request).await {
        Ok(provider) => provider,
        Err(error) => {
            write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: error.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    write_frame(&mut writer, &Opened::Ready).await?;
    let requester = tokio::io::join(reader, writer);
    rho_rpc::relay_bidirectional(requester, provider).await?;
    Ok(())
}

async fn serve_git_transport_provider<R, W>(
    services: Arc<Services>,
    reader: R,
    mut writer: W,
    request_id: u64,
    provider_id: u64,
    claim: bool,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match services
        .git_transport
        .claim(request_id, provider_id, claim)
        .await?
    {
        GitProviderClaim::Done => {
            write_frame(&mut writer, &GitProvided::Done).await?;
        }
        GitProviderClaim::Selected(response) => {
            if let Err(error) = write_frame(&mut writer, &GitProvided::Ready).await {
                let _ = response.send(Err(format!(
                    "selected GUI SSH Git client disconnected: {error}"
                )));
                return Err(error);
            }
            let stream = Box::new(tokio::io::join(reader, writer));
            response
                .send(Ok(stream))
                .map_err(|_| anyhow::anyhow!("Git transport requester disconnected"))?;
        }
    }
    Ok(())
}

/// Answers one call with its handler's reply: the call names the reply's
/// type, so no arm can answer with another call's. `Err` becomes
/// [`Answer::Failed`].
async fn respond<C, W, F>(
    writer: &mut W,
    call: C,
    handle: impl FnOnce(C) -> F,
) -> anyhow::Result<()>
where
    C: Call,
    W: tokio::io::AsyncWrite + Unpin,
    F: Future<Output = anyhow::Result<C::Reply>>,
{
    let reply = handle(call).await;
    write_frame(writer, &Answer::from(reply)).await
}

/// One call stream's call.
async fn serve_call<W>(
    services: &Arc<Services>,
    iroh_auth: Option<&rho_iroh_auth::IrohAuth>,
    request: Request,
    writer: &mut W,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let iroh = || iroh_auth.context("agent host is not listening over iroh (start it with --iroh)");
    match request {
        Request::GitTransportPolicy(call) => {
            respond(writer, call, |GitTransportPolicy { host }| async move {
                Ok(host == "github.com"
                    && services.platform_secrets.contains_nonempty("GITHUB_TOKEN"))
            })
            .await
        }
        Request::GuiTelemetryUpload(call) => {
            respond(writer, call, |GuiTelemetryUpload { snapshot }| {
                store_gui_telemetry(snapshot)
            })
            .await
        }
        Request::PlatformSecretsSet(call) => {
            respond(writer, call, |PlatformSecretsSet { secrets }| async move {
                install_platform_secrets(services, secrets)
            })
            .await
        }
        Request::Pr(call) => {
            respond(
                writer,
                call,
                |Pr {
                     agent_id: _,
                     command,
                 }| async move { Ok(pr(services, command).await) },
            )
            .await
        }
        Request::Snapshot(call) => {
            respond(writer, call, |Snapshot| debug::host_snapshot(&services.db)).await
        }
        Request::IrohApprove(call) => {
            respond(writer, call, |IrohApprove { code }| async move {
                let auth = iroh()?;
                let code = code
                    .parse::<rho_iroh_auth::EnrollmentCode>()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let endpoint_id = auth
                    .approve_code(&code)
                    .await
                    .map_err(|_| anyhow::anyhow!("no pending enrollment has this code"))?;
                Ok(endpoint_id.to_string())
            })
            .await
        }
        Request::IrohTrustInMemory(call) => {
            respond(
                writer,
                call,
                |IrohTrustInMemory { endpoint_id }| async move {
                    let auth = iroh()?;
                    let endpoint_id = endpoint_id
                        .parse::<iroh::EndpointId>()
                        .context("invalid iroh client endpoint id")?;
                    auth.trust_in_memory(endpoint_id).await;
                    Ok(())
                },
            )
            .await
        }
        Request::IrohRevoke(call) => {
            respond(writer, call, |IrohRevoke { endpoint_id }| async move {
                let auth = iroh()?;
                let endpoint_id = endpoint_id
                    .parse::<iroh::EndpointId>()
                    .context("invalid iroh client endpoint id")?;
                anyhow::ensure!(
                    auth.revoke(endpoint_id).await,
                    "iroh client is not enrolled"
                );
                Ok(endpoint_id.to_string())
            })
            .await
        }
    }
}

fn install_platform_secrets(
    services: &Services,
    secrets: Vec<(String, String)>,
) -> anyhow::Result<PlatformStatus> {
    let wants_octo = secrets.iter().any(|(key, _)| key == "GITHUB_TOKEN");
    let (running, detail) = match services.platform_secrets.install_merge(secrets) {
        Ok((store, stashed)) => {
            let persistence = if stashed {
                " and stashed in the systemd fd store"
            } else {
                " (no systemd notify socket: they will not survive an agent host restart)"
            };
            if wants_octo && store.read()?.contains_key("GITHUB_TOKEN") {
                (true, format!("GitHub secrets installed{persistence}"))
            } else {
                (true, format!("platform secrets installed{persistence}"))
            }
        }
        Err(error) => (false, format!("{error:#}")),
    };
    Ok(PlatformStatus { running, detail })
}

/// A PR command's outcome. A failure is the command's own output, not a
/// refused call.
async fn pr(services: &Services, command: rho_agent_hosts::protocol::PrCommand) -> PrOutput {
    let result = async {
        match command {
            rho_agent_hosts::protocol::PrCommand::Create {
                owner,
                repo,
                head,
                base,
                title,
                body,
                review_bots: _,
            } => services
                .pr_monitor
                .create(rho_pr_monitor::CreatePullRequest {
                    owner,
                    repo,
                    head,
                    base,
                    title,
                    body,
                })
                .await
                .map(|output| (output, Vec::new())),
            rho_agent_hosts::protocol::PrCommand::Subscribe { .. } => Ok((
                "persistent PR subscriptions were removed; poll `rho pr status` instead".to_owned(),
                Vec::new(),
            )),
            rho_agent_hosts::protocol::PrCommand::Status { url } => services
                .pr_monitor
                .status(&url)
                .await
                .map(|output| (output, Vec::new())),
            rho_agent_hosts::protocol::PrCommand::List => Ok(("[]".to_owned(), Vec::new())),
            rho_agent_hosts::protocol::PrCommand::Stop { .. } => Ok((
                "persistent PR subscriptions were removed".to_owned(),
                Vec::new(),
            )),
            rho_agent_hosts::protocol::PrCommand::Comment {
                url,
                reply_comment,
                body,
            } => services
                .pr_monitor
                .comment(&url, reply_comment, &body)
                .await
                .map(|output| (output, Vec::new())),
            rho_agent_hosts::protocol::PrCommand::Comments { url } => services
                .pr_monitor
                .comments(&url)
                .await
                .map(|output| (output, Vec::new())),
            rho_agent_hosts::protocol::PrCommand::Checks { url } => services
                .pr_monitor
                .checks(&url)
                .await
                .map(|output| (output, Vec::new())),
            rho_agent_hosts::protocol::PrCommand::Edit {
                url,
                base,
                title,
                body,
            } => services
                .pr_monitor
                .edit(&url, base, title, body)
                .await
                .map(|output| (output, Vec::new())),
            rho_agent_hosts::protocol::PrCommand::Rerun { url, run_id } => services
                .pr_monitor
                .rerun(&url, run_id)
                .await
                .map(|output| (output, Vec::new())),
            rho_agent_hosts::protocol::PrCommand::Logs { url, run_id } => services
                .pr_monitor
                .logs(&url, run_id)
                .await
                .map(|data| (format!("downloaded logs for run {run_id}"), data.to_vec())),
        }
    }
    .await;
    match result {
        Ok((output, data)) => PrOutput {
            output,
            data,
            is_error: false,
        },
        Err(error) => PrOutput {
            output: format!("{error:#}"),
            data: Vec::new(),
            is_error: true,
        },
    }
}

async fn store_gui_telemetry(snapshot: Vec<u8>) -> anyhow::Result<String> {
    anyhow::ensure!(
        snapshot.len() <= rho_agent_hosts::protocol::MAX_GUI_TELEMETRY_BYTES,
        "GUI telemetry snapshot is too large ({} bytes; limit is {} bytes)",
        snapshot.len(),
        rho_agent_hosts::protocol::MAX_GUI_TELEMETRY_BYTES
    );
    let path = tokio::task::spawn_blocking(move || {
        let state = dirs::state_dir().context("state directory not available")?;
        persist_gui_telemetry(&state.join("rho"), &snapshot)
    })
    .await
    .context("GUI telemetry storage task failed")?
    .context("failed to store GUI telemetry")?;
    Ok(path.display().to_string())
}

fn persist_gui_telemetry(state_root: &std::path::Path, snapshot: &[u8]) -> anyhow::Result<PathBuf> {
    use std::io::Write as _;

    anyhow::ensure!(
        snapshot.len() <= rho_agent_hosts::protocol::MAX_GUI_TELEMETRY_BYTES,
        "GUI telemetry snapshot exceeds the {} byte limit",
        rho_agent_hosts::protocol::MAX_GUI_TELEMETRY_BYTES
    );
    let directory = state_root.join("gui-telemetry");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for suffix in 0_u16..=u16::MAX {
        let path = directory.join(format!("gui-telemetry-{timestamp}-{suffix}.json"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(snapshot)
                    .with_context(|| format!("write {}", path.display()))?;
                file.sync_all()
                    .with_context(|| format!("sync {}", path.display()))?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    anyhow::bail!("could not allocate a unique GUI telemetry filename")
}

#[cfg(test)]
mod tests {
    use super::persist_gui_telemetry;

    #[test]
    fn gui_telemetry_storage_is_private_unique_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let first = persist_gui_telemetry(temp.path(), b"one").unwrap();
        let second = persist_gui_telemetry(temp.path(), b"two").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(first.as_path()).unwrap(), b"one");
        assert_eq!(std::fs::read(second.as_path()).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(first).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(
            persist_gui_telemetry(
                temp.path(),
                &vec![0; rho_agent_hosts::protocol::MAX_GUI_TELEMETRY_BYTES + 1]
            )
            .unwrap_err()
            .to_string()
            .contains("exceeds")
        );
    }
}
