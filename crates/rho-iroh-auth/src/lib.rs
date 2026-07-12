//! Enrollment/authentication helpers for iroh-style public-key connections.
//!
//! This crate intentionally does not decide what an authenticated client may
//! do. It only provides the auth-only first-stream exchange, enrollment and
//! code parsing, and client helpers.

#[cfg(feature = "server")]
mod server;
mod shared;

#[cfg(feature = "server")]
pub use server::{ApproveError, IrohAuth, ServerAuthDecision};
pub use shared::{EnrollmentCode, ParseEnrollmentCodeError};

const AUTH_REQUEST: &[u8] = b"rho-auth-v1\n";
const AUTH_ACK: &[u8] = b"ack\n";
const MAX_AUTH_RESPONSE_LEN: usize = 128;

/// Result of the mandatory auth-only first stream on an iroh connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientAuthResult {
    Approved,
    EnrollmentRequired(EnrollmentCode),
    Unavailable,
}

/// Open the mandatory first stream and read the server's bounded auth result.
pub async fn authenticate_client(
    connection: &iroh::endpoint::Connection,
    client_endpoint_id: iroh::EndpointId,
) -> anyhow::Result<ClientAuthResult> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(AUTH_REQUEST).await?;
    let response = recv.read_to_end(MAX_AUTH_RESPONSE_LEN).await?;
    let response = std::str::from_utf8(&response)?;
    let result = if response == "approved\n" {
        ClientAuthResult::Approved
    } else if response == "unavailable\n" {
        ClientAuthResult::Unavailable
    } else if response == "enrollment-required\n" {
        let code = shared::enrollment_code(connection, connection.remote_id(), client_endpoint_id);
        ClientAuthResult::EnrollmentRequired(code)
    } else {
        anyhow::bail!("invalid iroh auth response")
    };
    // The server does not close an unapproved connection until this confirms
    // that the application consumed its enrollment response.
    send.write_all(AUTH_ACK).await?;
    send.finish()?;
    Ok(result)
}

/// Serve the mandatory first-stream auth exchange. Application streams must
/// not be accepted unless this returns `Approved`.
#[cfg(feature = "server")]
pub async fn authenticate_server_connection(
    auth: &IrohAuth,
    connection: &iroh::endpoint::Connection,
) -> anyhow::Result<ServerAuthDecision> {
    use std::time::Duration;

    tokio::time::timeout(Duration::from_secs(10), async {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let mut request = [0; AUTH_REQUEST.len()];
        recv.read_exact(&mut request).await?;
        anyhow::ensure!(request == AUTH_REQUEST, "invalid iroh auth request");
        let decision = auth.authenticate_connection(connection).await;
        let response = match &decision {
            ServerAuthDecision::Approved => "approved\n".to_owned(),
            ServerAuthDecision::EnrollmentRequired(_) => "enrollment-required\n".to_owned(),
            ServerAuthDecision::Unavailable => "unavailable\n".to_owned(),
        };
        send.write_all(response.as_bytes()).await?;
        send.finish()?;
        let ack = recv.read_to_end(AUTH_ACK.len()).await?;
        anyhow::ensure!(ack == AUTH_ACK, "invalid iroh auth acknowledgement");
        Ok(decision)
    })
    .await
    .map_err(|_| anyhow::anyhow!("iroh auth stream timeout"))?
}
