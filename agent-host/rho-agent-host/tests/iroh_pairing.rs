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
    let connection = tokio::time::timeout(
        Duration::from_secs(40),
        client.connect(host, rho_rpc::protocol::IROH_ALPN),
    )
    .await
    .context("connect timed out")??;
    rho_rpc::authenticate_iroh_client(&connection, client.id(), pairing).await
}

async fn start(temp: &Path) -> Result<(Child, std::path::PathBuf)> {
    let runtime = temp.join("runtime");
    std::fs::create_dir(&runtime)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
    let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let socket = temp.join("rho.sock");
    let child = Child(
        Command::new(bin.join("rho-agent-host"))
            .args(["--iroh", "--socket-path"])
            .arg(&socket)
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
    while rho_rpc::connect_unix(&socket).await.is_err() {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "agent host startup timed out"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok((child, socket))
}

#[tokio::test]
async fn a_pairing_link_trusts_the_first_client_for_good_and_no_other() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (_host, socket) = start(temp.path()).await?;
    let link: PairingLink = rho_rpc::protocol::client::call(&socket, host::IrohPair)
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
