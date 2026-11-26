//! The machine part of the agent host, [`rho_rpc::protocol::Protocol::Host`]:
//! Git transport and administration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use rho_agent_hosts::protocol::{
    DebugReport, ForgetSavepoints, GccpApprove, GccpContact, GccpHost, GccpList, GccpRemove,
    GccpState, GitDecision, GitProvided, GitProviderFrame, GitTransportPolicy, GuiTelemetryUpload,
    IrohApprove, IrohPair, IrohRevoke, IrohTrustInMemory, Open, PlatformSecretsSet, PlatformStatus,
    Request, Snapshot,
};
use rho_rpc::protocol::{Answer, Call, Opened, write_frame};
use tokio::sync::mpsc;

use crate::git::{GitProviderClaim, GitTransportBroker, requester_left};
use crate::{Services, debug};

/// Serves one host stream, whichever kind its opening frame asked for.
pub(crate) async fn serve<R, W>(
    services: Arc<Services>,
    iroh_auth: rho_iroh_auth::IrohAuth,
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
        Open::Request(request) => serve_call(&services, &iroh_auth, request, &mut writer).await,
        Open::GitTransport { request } => {
            serve_git_transport_request(&services.git_transport, reader, writer, request).await
        }
        Open::GitProvide {
            request_id,
            provider_id,
            decision,
        } => {
            serve_git_transport_provider(
                &services.git_transport,
                reader,
                writer,
                request_id,
                provider_id,
                decision,
            )
            .await
        }
    }
}

/// A GUI carrying SSH Git transport: registered with the broker for as
/// long as its stream is open, and told each request that waits and its
/// end.
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
    services.git_transport.register(frames_tx);
    loop {
        tokio::select! {
            closed = rho_rpc::protocol::read_frame_optional::<_, ()>(&mut reader) => {
                return closed.map(|_| ());
            }
            Some(frame) = frames_rx.recv() => write_frame(&mut writer, &frame).await?,
        }
    }
}

/// A Git remote helper's request: it waits until the user answers on a
/// GUI or the helper goes away, then becomes the transport or a refusal.
pub(crate) async fn serve_git_transport_request<R, W>(
    broker: &GitTransportBroker,
    reader: R,
    mut writer: W,
    request: rho_agent_hosts::protocol::GitTransportRequest,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = tokio::io::BufReader::new(reader);
    let answer = match broker.request(request) {
        Ok(mut asked) => {
            tokio::select! {
                answer = asked.answer() => answer,
                // Dropping `asked` withdraws the request from every GUI.
                () = requester_left(&mut reader) => return Ok(()),
            }
        }
        Err(error) => Err(error.to_string()),
    };
    let provider = match answer {
        Ok(provider) => provider,
        Err(reason) => {
            write_frame(&mut writer, &Opened::Refused { reason }).await?;
            return Ok(());
        }
    };
    write_frame(&mut writer, &Opened::Ready).await?;
    let requester = tokio::io::join(reader, writer);
    rho_rpc::relay_bidirectional(requester, provider).await?;
    Ok(())
}

pub(crate) async fn serve_git_transport_provider<R, W>(
    broker: &GitTransportBroker,
    reader: R,
    mut writer: W,
    request_id: u64,
    provider_id: u64,
    decision: GitDecision,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match broker.decide(request_id, provider_id, decision) {
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
    iroh_auth: &rho_iroh_auth::IrohAuth,
    request: Request,
    writer: &mut W,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
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
        Request::Snapshot(call) => {
            respond(writer, call, |Snapshot| debug::host_snapshot(&services.db)).await
        }
        Request::ForgetSavepoints(call) => {
            respond(writer, call, |ForgetSavepoints| async {
                Ok(rho_agent::db::forget_savepoints(&services.db).await)
            })
            .await
        }
        Request::CompactDatabase(call) => {
            respond(
                writer,
                call,
                |_: rho_agent_hosts::protocol::CompactDatabase| async {
                    services.db.compact_online().await
                },
            )
            .await
        }
        Request::DebugReport(call) => {
            respond(writer, call, |report: DebugReport| {
                debug::host_report(&services.db, &services.claude, report)
            })
            .await
        }
        Request::IrohApprove(call) => {
            respond(writer, call, |IrohApprove { code }| async move {
                let auth = iroh_auth;
                let code = code
                    .parse::<rho_iroh_auth::EnrollmentCode>()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let endpoint_id = auth
                    .approve_code(&code)
                    .await
                    .map_err(|_| anyhow::anyhow!("no pending enrollment has this code"))?;
                Ok(endpoint_text(endpoint_id))
            })
            .await
        }
        Request::IrohTrustInMemory(call) => {
            respond(
                writer,
                call,
                |IrohTrustInMemory { endpoint_id }| async move {
                    let auth = iroh_auth;
                    let endpoint_id = iroh_endpoint(&endpoint_id)?;
                    auth.trust_in_memory(endpoint_id).await;
                    Ok(())
                },
            )
            .await
        }
        Request::IrohPair(call) => {
            respond(writer, call, |IrohPair| async move {
                let auth = iroh_auth;
                let link = rho_iroh_auth::PairingLink {
                    endpoint_id: auth.server_endpoint_id(),
                    key: auth.mint_pairing_key().await,
                };
                Ok(link.to_string())
            })
            .await
        }
        Request::IrohRevoke(call) => {
            respond(writer, call, |IrohRevoke { endpoint_id }| async move {
                let auth = iroh_auth;
                let endpoint_id = iroh_endpoint(&endpoint_id)?;
                // Whatever the endpoint was approved as.
                let client = auth.revoke(endpoint_id).await;
                let host = rho_agent_types::EndpointId(*endpoint_id.as_bytes());
                let host = services.pool.gccp_revoke(host).await;
                anyhow::ensure!(client || host, "this iroh endpoint is not approved");
                Ok(endpoint_text(endpoint_id))
            })
            .await
        }
        Request::GccpList(call) => {
            respond(writer, call, |GccpList| async move {
                let pool = &services.pool;
                Ok(GccpState {
                    endpoint: pool.gccp_endpoint().map(|endpoint| endpoint.to_string()),
                    hosts: pool
                        .gccp_hosts()
                        .into_iter()
                        .map(|(endpoint, name)| GccpHost {
                            endpoint: endpoint.to_string(),
                            name,
                        })
                        .collect(),
                    contacts: pool
                        .gccp_contacts()
                        .into_iter()
                        .map(|contact| GccpContact {
                            agent: contact.agent.encoded(),
                            address: contact.peer.to_string(),
                            name: contact.name,
                        })
                        .collect(),
                })
            })
            .await
        }
        Request::GccpApprove(call) => {
            respond(writer, call, |GccpApprove { endpoint, name }| async move {
                services
                    .pool
                    .gccp_approve(gccp_endpoint(&endpoint)?, &name)
                    .await
            })
            .await
        }
        Request::GccpRemove(call) => {
            respond(writer, call, |GccpRemove { agent, address }| async move {
                let agent = rho_agent_types::AgentId::from_encoded(&agent)
                    .map_err(|_| anyhow::anyhow!("invalid agent id {agent:?}"))?;
                let address = address
                    .parse::<rho_agent_types::PeerAddress>()
                    .map_err(|error| anyhow::anyhow!("invalid GCCP address: {error}"))?;
                services.pool.gccp_remove(agent, &address).await
            })
            .await
        }
    }
}

/// A harness's endpoint id as the user gives it: the z-base-32 of an
/// address, and an id an iroh endpoint can have.
fn gccp_endpoint(text: &str) -> anyhow::Result<rho_agent_types::EndpointId> {
    let endpoint = iroh_endpoint(text)?;
    Ok(rho_agent_types::EndpointId(*endpoint.as_bytes()))
}

/// An iroh endpoint id as the user gives it, in the one text form rho uses
/// ([`rho_agent_types::EndpointId`]).
fn iroh_endpoint(text: &str) -> anyhow::Result<iroh::EndpointId> {
    let endpoint: rho_agent_types::EndpointId = text
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid iroh endpoint id: {error}"))?;
    iroh::EndpointId::from_bytes(&endpoint.0)
        .with_context(|| format!("invalid iroh endpoint id: {text} is no public key"))
}

fn endpoint_text(endpoint: iroh::EndpointId) -> String {
    rho_agent_types::EndpointId(*endpoint.as_bytes()).to_string()
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

#[cfg(test)]
mod git_tests {
    use rho_agent_hosts::protocol::{
        GitDecision, GitProvided, GitProviderFrame, GitService, GitTransportRequest,
    };
    use rho_rpc::protocol::{Opened, read_frame};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::{serve_git_transport_provider, serve_git_transport_request};
    use crate::git::GitTransportBroker;

    fn push() -> GitTransportRequest {
        GitTransportRequest {
            host: "github.com".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "acme/repo".to_owned(),
            service: GitService::ReceivePack,
            planned_refs: Some(vec![
                "refs/heads/main".to_owned(),
                "refs/tags/v1".to_owned(),
            ]),
            agent: None,
        }
    }

    /// A window, registered with `broker`; its frames arrive on the receiver.
    fn window(
        broker: &GitTransportBroker,
    ) -> tokio::sync::mpsc::UnboundedReceiver<GitProviderFrame> {
        let (frames, received) = tokio::sync::mpsc::unbounded_channel();
        broker.register(frames);
        received
    }

    async fn requested(
        window: &mut tokio::sync::mpsc::UnboundedReceiver<GitProviderFrame>,
    ) -> (u64, u64) {
        match window.recv().await {
            Some(GitProviderFrame::Requested {
                request_id,
                provider_id,
                ..
            }) => (request_id, provider_id),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    /// The agent's git is stopped while its push waits: the window hears
    /// the request is done, and the card goes.
    #[tokio::test]
    async fn a_helper_that_goes_away_while_it_waits_ends_its_card() {
        let broker = GitTransportBroker::default();
        let mut window = window(&broker);
        let (helper, host) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(host);
        let serving = serve_git_transport_request(&broker, reader, writer, push());
        let helper_goes = async {
            let (request_id, _) = requested(&mut window).await;
            drop(helper);
            request_id
        };
        let (served, request_id) = tokio::join!(serving, helper_goes);
        served.unwrap();
        assert_eq!(
            window.recv().await,
            Some(GitProviderFrame::Done { request_id })
        );
    }

    /// The user denies: the agent's git reads why, in words that name
    /// the push it asked for.
    #[tokio::test]
    async fn a_denial_reaches_the_agent_as_a_refusal() {
        let broker = GitTransportBroker::default();
        let mut window = window(&broker);
        let (mut helper, host) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(host);
        let serving = serve_git_transport_request(&broker, reader, writer, push());
        let deny = async {
            let (request_id, provider_id) = requested(&mut window).await;
            let (mut answer, host) = tokio::io::duplex(1024);
            let (reader, writer) = tokio::io::split(host);
            serve_git_transport_provider(
                &broker,
                reader,
                writer,
                request_id,
                provider_id,
                GitDecision::Deny,
            )
            .await
            .unwrap();
            let provided: GitProvided = read_frame(&mut answer).await.unwrap();
            provided
        };
        let (served, provided) = tokio::join!(serving, deny);
        served.unwrap();
        assert_eq!(provided, GitProvided::Done);
        let opened: Opened = read_frame(&mut helper).await.unwrap();
        assert_eq!(
            opened,
            Opened::Refused {
                reason: "the user denied this push to github.com:acme/repo \
                         (refs/heads/main, refs/tags/v1)"
                    .to_owned()
            }
        );
    }

    /// The user approves: the window that approved carries the agent's
    /// git, both ways.
    #[tokio::test]
    async fn an_approval_joins_the_agent_to_the_window_that_approved() {
        let broker = std::sync::Arc::new(GitTransportBroker::default());
        let mut window = window(&broker);
        let (mut helper, host) = tokio::io::duplex(1024);
        let serving = {
            let broker = broker.clone();
            tokio::spawn(async move {
                let (reader, writer) = tokio::io::split(host);
                serve_git_transport_request(&broker, reader, writer, push()).await
            })
        };
        let (request_id, provider_id) = requested(&mut window).await;
        let (mut carrier, host) = tokio::io::duplex(1024);
        let providing = {
            let broker = broker.clone();
            tokio::spawn(async move {
                let (reader, writer) = tokio::io::split(host);
                serve_git_transport_provider(
                    &broker,
                    reader,
                    writer,
                    request_id,
                    provider_id,
                    GitDecision::Approve,
                )
                .await
            })
        };
        let provided: GitProvided = read_frame(&mut carrier).await.unwrap();
        assert_eq!(provided, GitProvided::Ready);
        providing.await.unwrap().unwrap();
        let opened: Opened = read_frame(&mut helper).await.unwrap();
        assert_eq!(opened, Opened::Ready);
        helper.write_all(b"0000").await.unwrap();
        let mut sent = [0; 4];
        carrier.read_exact(&mut sent).await.unwrap();
        assert_eq!(&sent, b"0000");
        carrier.write_all(b"ok").await.unwrap();
        let mut back = [0; 2];
        helper.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ok");
        drop(helper);
        drop(carrier);
        serving.await.unwrap().unwrap();
    }
}
