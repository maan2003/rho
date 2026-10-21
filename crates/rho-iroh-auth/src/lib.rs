//! Enrollment/authentication helpers for iroh-style public-key connections.
//!
//! This crate intentionally does not decide what an authenticated client may
//! do. It only provides the auth-only first-stream exchange, enrollment and
//! code parsing, and client helpers.

mod server;
mod shared;

pub use server::{ApproveError, IrohAuth, PAIRING_TTL, PreapprovedEndpoint, ServerAuthDecision};
pub use shared::{
    EnrollmentCode, PAIRING_SCHEME, PairingKey, PairingLink, ParseEnrollmentCodeError,
};

/// Then 32 bytes: a [`PairingKey`], or zeros for none.
const AUTH_REQUEST: &[u8] = b"rho-auth-v2\n";
/// A client from before pairing keys: no key follows.
const AUTH_REQUEST_V1: &[u8] = b"rho-auth-v1\n";
const AUTH_ACK: &[u8] = b"ack\n";
const MAX_AUTH_RESPONSE_LEN: usize = 128;

/// Recovery window granted after both peers authenticate the connection.
pub const AUTHENTICATED_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Result of the mandatory auth-only first stream on an iroh connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientAuthResult {
    Approved,
    EnrollmentRequired(EnrollmentCode),
    Unavailable,
}

/// Open the mandatory first stream and read the server's bounded auth result.
/// A `pairing` key pairs a client the host does not trust yet.
pub async fn authenticate_client(
    connection: &iroh::endpoint::Connection,
    client_endpoint_id: iroh::EndpointId,
    pairing: Option<&PairingKey>,
) -> anyhow::Result<ClientAuthResult> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(AUTH_REQUEST).await?;
    let key: &[u8; 32] = pairing.map_or(&[0; 32], PairingKey::as_bytes);
    send.write_all(key).await?;
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
    if result == ClientAuthResult::Approved {
        connection.set_max_idle_timeout(Some(AUTHENTICATED_IDLE_TIMEOUT));
    }
    Ok(result)
}

/// Serve the mandatory first-stream auth exchange. Application streams must
/// not be accepted unless this returns `Approved`.
pub async fn authenticate_server_connection(
    auth: &IrohAuth,
    connection: &iroh::endpoint::Connection,
    preapproved: Option<PreapprovedEndpoint>,
) -> anyhow::Result<ServerAuthDecision> {
    use std::time::Duration;

    tokio::time::timeout(Duration::from_secs(10), async {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let mut request = [0; AUTH_REQUEST.len()];
        recv.read_exact(&mut request).await?;
        let mut key = [0; 32];
        if request == AUTH_REQUEST {
            recv.read_exact(&mut key).await?;
        } else {
            anyhow::ensure!(request == AUTH_REQUEST_V1, "invalid iroh auth request");
        }
        let pairing = (key != [0; 32]).then(|| PairingKey::from_bytes(key));
        // Pin trust established at admission for this connection. Revocation
        // affects later connections, not one whose authenticated handshake
        // already passed admission. Unknown peers may still become approved
        // while waiting for enrollment capacity.
        let decision = match preapproved {
            Some(preapproved) => {
                anyhow::ensure!(
                    preapproved.matches(connection.remote_id()),
                    "preapproved endpoint does not match connection"
                );
                ServerAuthDecision::Approved
            }
            None => auth.authenticate_connection(connection, pairing).await,
        };
        let response = match &decision {
            ServerAuthDecision::Approved => "approved\n".to_owned(),
            ServerAuthDecision::EnrollmentRequired(_) => "enrollment-required\n".to_owned(),
            ServerAuthDecision::Unavailable => "unavailable\n".to_owned(),
        };
        send.write_all(response.as_bytes()).await?;
        send.finish()?;
        let ack = recv.read_to_end(AUTH_ACK.len()).await?;
        anyhow::ensure!(ack == AUTH_ACK, "invalid iroh auth acknowledgement");
        if decision == ServerAuthDecision::Approved {
            connection.set_max_idle_timeout(Some(AUTHENTICATED_IDLE_TIMEOUT));
        }
        Ok(decision)
    })
    .await
    .map_err(|_| anyhow::anyhow!("iroh auth stream timeout"))?
}
