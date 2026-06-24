use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Sink, Stream};
use rho_core::{
    ContextBlock, ContextItem, InferenceResponse, InferenceUpdate, ItemId, ItemKind, Message,
    MessagePhase, ProviderItem, ProviderItemKind, ReasoningTextKind, Role, TokenUsage, ToolCall,
    ToolCallId, ToolFormat, ToolGrammarSyntax, ToolResult, ToolSpec, ToolType,
};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::oauth::{InferenceAuth, OAuthFile, ResponsesOAuthCredentials};
use super::wire::{ResponseState, ResponsesRequest};
use super::ws::{WsResponseCreate, build_ws_request, next_ws_message};
use super::*;

fn first_message(response: &InferenceResponse) -> &Message {
    response
        .items
        .iter()
        .find_map(|item| match item {
            ItemKind::Message(message) => Some(message),
            _ => None,
        })
        .expect("message item")
}

fn first_tool_call(response: &InferenceResponse) -> &ToolCall {
    response
        .items
        .iter()
        .find_map(|item| match item {
            ItemKind::ToolCall(call) => Some(call),
            _ => None,
        })
        .expect("tool call item")
}

fn test_oauth_file(
    access_token: &str,
    account_id: Option<&str>,
) -> (tempfile::TempDir, InferenceAuth) {
    let temp = tempfile::tempdir().unwrap();
    let file = test_oauth_file_in(temp.path(), "chatgpt").unwrap();
    file.save(&ResponsesOAuthCredentials {
        access_token: access_token.to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: account_id.map(str::to_owned),
    })
    .unwrap();
    let auth = InferenceAuth::oauth_file(file.path());
    (temp, auth)
}

fn test_inference_service(model: impl Into<String>) -> InferenceSession {
    let (_temp, auth) = test_oauth_file("token", None);
    InferenceSession::new(model, auth)
}

fn test_oauth_file_in(
    state_dir: impl AsRef<Path>,
    name: impl AsRef<str>,
) -> std::io::Result<OAuthFile> {
    OAuthFile::open_at(state_dir.as_ref().join("auth.d"), name)
}

#[derive(Default)]
struct PendingSocket {
    sent: Vec<WsMessage>,
}

impl Stream for PendingSocket {
    type Item = std::result::Result<WsMessage, tungstenite::Error>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Sink<WsMessage> for PendingSocket {
    type Error = tungstenite::Error;

    fn poll_ready(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: WsMessage) -> std::result::Result<(), Self::Error> {
        self.get_mut().sent.push(item);
        Ok(())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

mod parser;
mod request;
mod session;
mod websocket;
