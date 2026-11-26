//! A real agent host over iroh: a pairing link it prints trusts the client
//! that opens it, for good, and nobody after.
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use rho_agent_hosts::protocol as host;
use rho_iroh_auth::{ClientAuthResult, PairingLink};

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn auth(
    host: iroh::EndpointId,
    client: &iroh::Endpoint,
    pairing: Option<&rho_iroh_auth::PairingKey>,
) -> Result<ClientAuthResult> {
    Ok(dial(host, client, pairing).await?.1)
}

async fn dial(
    host: iroh::EndpointId,
    client: &iroh::Endpoint,
    pairing: Option<&rho_iroh_auth::PairingKey>,
) -> Result<(iroh::endpoint::Connection, ClientAuthResult)> {
    let connection = tokio::time::timeout(
        Duration::from_secs(40),
        client.connect(host, rho_rpc::protocol::IROH_ALPN),
    )
    .await
    .context("connect timed out")??;
    let result = rho_rpc::authenticate_iroh_client(&connection, client.id(), pairing).await?;
    Ok((connection, result))
}

/// One call over a client's own connection, as a window makes it.
async fn call_over<C: rho_rpc::protocol::Call>(
    connection: &iroh::endpoint::Connection,
    call: C,
) -> Result<C::Reply> {
    let (send, recv) = connection.open_bi().await?;
    let mut client =
        rho_rpc::protocol::client::Client::from_stream(rho_rpc::Stream::new(recv, send));
    client.open(&call.open()).await?;
    client
        .recv::<rho_rpc::protocol::Answer<C::Reply>>()
        .await?
        .into_result()
}

fn endpoint_text(endpoint: &iroh::Endpoint) -> String {
    rho_agent_types::EndpointId(*endpoint.id().as_bytes()).to_string()
}

async fn pair(paths: &rho_rpc::protocol::RuntimePaths) -> Result<(PairingLink, iroh::Endpoint)> {
    let link: PairingLink = rho_rpc::protocol::client::call(paths, host::IrohPair)
        .await?
        .parse()
        .map_err(anyhow::Error::msg)?;
    let device = rho_rpc::bind_ephemeral_iroh_client().await?;
    ensure!(auth(link.endpoint_id, &device, Some(&link.key)).await? == ClientAuthResult::Approved);
    Ok((link, device))
}

async fn start(temp: &Path) -> Result<(Child, rho_rpc::protocol::RuntimePaths)> {
    let runtime = temp.join("runtime");
    std::fs::create_dir(&runtime)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let paths = rho_rpc::protocol::RuntimePaths::new(Some(temp.join("host")))?;
    let child = Child(
        Command::new(bin.join("rho-agent-host"))
            .arg("--runtime-dir")
            .arg(paths.directory())
            .arg("--claude-config-dir")
            .arg(temp.join("claude"))
            .env("XDG_STATE_HOME", temp.join("state"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(temp.join("agent-host.log"))?)
            .spawn()
            .context("build rho-agent-host first")?,
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while rho_rpc::protocol::client::connect(&paths).await.is_err() {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "agent host startup timed out"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok((child, paths))
}

#[tokio::test]
async fn a_pairing_link_trusts_the_first_client_for_good_and_no_other() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (_host, paths) = start(temp.path()).await?;
    let link: PairingLink = rho_rpc::protocol::client::call(&paths, host::IrohPair)
        .await?
        .parse()
        .map_err(anyhow::Error::msg)?;

    let stranger = rho_rpc::bind_ephemeral_iroh_client().await?;
    ensure!(
        matches!(
            auth(link.endpoint_id, &stranger, None).await?,
            ClientAuthResult::EnrollmentRequired(_)
        ),
        "a client without the key is not trusted"
    );

    let paired = rho_rpc::bind_ephemeral_iroh_client().await?;
    ensure!(
        auth(link.endpoint_id, &paired, Some(&link.key)).await? == ClientAuthResult::Approved,
        "the key trusts the client that shows it"
    );
    ensure!(
        matches!(
            auth(link.endpoint_id, &stranger, Some(&link.key)).await?,
            ClientAuthResult::EnrollmentRequired(_)
        ),
        "a spent key trusts nobody else"
    );
    ensure!(
        auth(link.endpoint_id, &paired, None).await? == ClientAuthResult::Approved,
        "the paired client needs no key again"
    );
    Ok(())
}

#[tokio::test]
async fn a_revoked_device_is_trusted_no_longer() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (_host, paths) = start(temp.path()).await?;
    let (link, laptop) = pair(&paths).await?;
    let (_, lost) = pair(&paths).await?;

    let (connection, approved) = dial(link.endpoint_id, &laptop, None).await?;
    ensure!(approved == ClientAuthResult::Approved);
    let mut listed = call_over(&connection, host::IrohDevices).await?;
    listed.sort_by_key(|device| !device.asking);
    let ids: Vec<_> = listed
        .iter()
        .map(|device| (device.endpoint_id.clone(), device.asking))
        .collect();
    ensure!(
        ids == [
            (endpoint_text(&laptop), true),
            (endpoint_text(&lost), false)
        ],
        "both devices, the one that asks marked: {ids:?}"
    );
    ensure!(listed.iter().all(|device| device.trusted_at_unix_secs > 0));

    let revoked = call_over(
        &connection,
        host::IrohRevoke {
            endpoint_id: endpoint_text(&lost),
        },
    )
    .await?;
    ensure!(revoked == endpoint_text(&lost));
    ensure!(
        matches!(
            auth(link.endpoint_id, &lost, None).await?,
            ClientAuthResult::EnrollmentRequired(_)
        ),
        "the lost device must enroll again"
    );
    ensure!(
        auth(link.endpoint_id, &laptop, None).await? == ClientAuthResult::Approved,
        "the device that revoked it is still trusted"
    );
    let left: Vec<_> = call_over(&connection, host::IrohDevices)
        .await?
        .into_iter()
        .map(|device| device.endpoint_id)
        .collect();
    ensure!(left == [endpoint_text(&laptop)], "{left:?}");
    Ok(())
}
