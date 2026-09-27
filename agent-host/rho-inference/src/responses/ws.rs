//! Shared ChatGPT socket dialing for model steps and route probes.
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::oauth::ResolvedAuth;
use super::route::DialRoute;

pub(crate) fn request(
    base: &str,
    thread_id: Option<&str>,
    auth: &ResolvedAuth,
) -> anyhow::Result<Request<()>> {
    let url = format!("{}/codex/responses", base.trim_end_matches('/'));
    let url = if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("websocket base URL must start with http:// or https://")
    };
    let mut request = url.into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("OpenAI-Beta", "responses_websockets=2026-02-06".parse()?);
    headers.insert(
        "Authorization",
        format!("Bearer {}", auth.bearer_token).parse()?,
    );
    if let Some(thread_id) = thread_id {
        headers.insert("session-id", thread_id.parse()?);
        headers.insert("thread-id", thread_id.parse()?);
    }
    if let Some(account) = &auth.account_id {
        headers.insert("chatgpt-account-id", account.parse()?);
    }
    Ok(request)
}

pub(crate) async fn connect(
    request: Request<()>,
    route: DialRoute,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
    ),
    tokio_tungstenite::tungstenite::Error,
> {
    match route.ip() {
        None => connect_async(request).await,
        Some(ip) => {
            let tcp = TcpStream::connect((ip, 443)).await?;
            tokio_tungstenite::client_async_tls_with_config(request, tcp, None, None).await
        }
    }
}
