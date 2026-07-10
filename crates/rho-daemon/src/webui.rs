//! Browser WebSocket endpoint for the static web UI.
//!
//! The page itself is static (`webui/` in the repository, hostable anywhere);
//! the daemon only upgrades `/ws` to a WebSocket. Each WebSocket becomes a
//! normal UI protocol session through an in-process duplex pipe into
//! [`crate::serve_connection_io`], so the browser surface reuses the daemon's
//! message handling wholesale and this module only translates JSON to
//! protocol frames.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use futures_util::{SinkExt as _, StreamExt as _};
use rho_core::ContentPart;
use rho_ui_proto::remote::UiAgentState;
use rho_ui_proto::{
    AgentId, AgentMode, ClientMessage, MessageDelivery, ServerMessage, StartMode, UiTopic,
    UiWorkdir, read_frame, write_frame,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Message, Role};

use crate::AgentRegistry;
use json::{FromBrowser, ToBrowser};

mod json;

/// Streamed transcript frames are coalesced onto this tick before the
/// selected agent's full state goes to the browser.
const PUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    agents: Arc<AgentRegistry>,
    allowed_origin: Option<String>,
) {
    let allowed_origin = allowed_origin.map(std::sync::Arc::new);
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                eprintln!("rho web UI accept error: {error:#}");
                continue;
            }
        };
        let agents = agents.clone();
        let allowed_origin = allowed_origin.clone();
        tokio::spawn(async move {
            let allowed_origin = allowed_origin.as_deref().map(String::as_str);
            if let Err(error) = handle_http(stream, agents, allowed_origin).await {
                eprintln!("rho web UI connection error: {error:#}");
            }
        });
    }
}

/// Just enough HTTP: a WebSocket at `/ws`; the UI page is hosted elsewhere.
async fn handle_http(
    mut stream: TcpStream,
    agents: Arc<AgentRegistry>,
    allowed_origin: Option<&str>,
) -> anyhow::Result<()> {
    let head = read_request_head(&mut stream).await?;
    let request = parse_request_head(&head)?;
    // Browsers always send Origin on WebSocket handshakes; when pinned,
    // reject other websites driving this unauthenticated endpoint from the
    // user's browser.
    if let Some(allowed) = allowed_origin
        && request.origin.as_deref() != Some(allowed)
    {
        stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await?;
        anyhow::bail!("web UI origin {:?} not allowed", request.origin);
    }
    match (request.path.as_str(), request.websocket_key) {
        ("/ws", Some(key)) => {
            let accept = derive_accept_key(key.as_bytes());
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\
                 connection: upgrade\r\nsec-websocket-accept: {accept}\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await?;
            let ws = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
            ws_session(ws, agents).await
        }
        _ => {
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await?;
            Ok(())
        }
    }
}

struct RequestHead {
    path: String,
    websocket_key: Option<String>,
    origin: Option<String>,
}

async fn read_request_head(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    const MAX_HEAD: usize = 16 * 1024;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        anyhow::ensure!(head.len() < MAX_HEAD, "request head too large");
        let read = stream.read(&mut byte).await?;
        anyhow::ensure!(read == 1, "connection closed mid-request");
        head.push(byte[0]);
    }
    Ok(head)
}

fn parse_request_head(head: &[u8]) -> anyhow::Result<RequestHead> {
    let head = std::str::from_utf8(head).context("request head is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().context("empty request")?;
    let mut parts = request_line.split(' ');
    let method = parts.next().context("missing method")?;
    anyhow::ensure!(method == "GET", "unsupported method {method}");
    let path = parts.next().context("missing path")?;
    let path = path.split('?').next().unwrap_or(path).to_owned();
    let mut websocket_key = None;
    let mut origin = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("sec-websocket-key") {
            websocket_key = Some(value.trim().to_owned());
        } else if name.trim().eq_ignore_ascii_case("origin") {
            origin = Some(value.trim().to_owned());
        }
    }
    Ok(RequestHead {
        path,
        websocket_key,
        origin,
    })
}

/// One browser tab: an in-process UI protocol session, agent state mirrored
/// here, and only the selected agent's transcript forwarded.
async fn ws_session(
    ws: WebSocketStream<TcpStream>,
    agents: Arc<AgentRegistry>,
) -> anyhow::Result<()> {
    let (ws_side, daemon_side) = tokio::io::duplex(rho_ui_proto::MAX_FRAME_LEN.min(1 << 20));
    let (daemon_read, daemon_write) = tokio::io::split(daemon_side);
    tokio::spawn(async move {
        let counters = rho_ui_proto::IoCounters::default();
        // Ends when the browser side drops; disconnect errors are routine.
        let _ = crate::serve_connection_io(agents, daemon_read, daemon_write, counters, None).await;
    });
    let (session_read, mut session_write) = tokio::io::split(ws_side);
    write_frame(&mut session_write, &ClientMessage::Subscribe).await?;

    // Reads happen on their own task: `read_frame` is not cancellation-safe
    // inside `select!`.
    let (server_tx, mut server_rx) = mpsc::unbounded_channel::<ServerMessage>();
    tokio::spawn(async move {
        let mut session_read = session_read;
        while let Ok(message) = read_frame::<_, ServerMessage>(&mut session_read).await {
            if server_tx.send(message).is_err() {
                break;
            }
        }
    });

    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut topics: Vec<UiTopic> = Vec::new();
    let mut workdirs: Vec<UiWorkdir> = Vec::new();
    let mut default_topic = None;
    let mut agent_ids: HashMap<String, AgentId> = HashMap::new();
    let mut states: HashMap<AgentId, UiAgentState> = HashMap::new();
    let mut selected: Option<AgentId> = None;
    let mut dirty = false;
    let mut push_tick = tokio::time::interval(PUSH_INTERVAL);
    push_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            message = server_rx.recv() => {
                let Some(message) = message else {
                    anyhow::bail!("daemon session closed");
                };
                match message {
                    ServerMessage::Ready { topics: new_topics, workdirs: new_workdirs, default_topic_id, .. } => {
                        topics = new_topics;
                        workdirs = new_workdirs;
                        default_topic = Some(default_topic_id);
                        index_agent_ids(&topics, &mut agent_ids);
                        send_json(&mut ws_tx, &json::hello(&topics, &workdirs)).await?;
                    }
                    ServerMessage::TopicCreated { topic } => {
                        topics.push(topic);
                        index_agent_ids(&topics, &mut agent_ids);
                        send_json(&mut ws_tx, &json::hello(&topics, &workdirs)).await?;
                    }
                    ServerMessage::AgentAttention { agent_id, attention } => {
                        for topic in &mut topics {
                            for agent in &mut topic.agents {
                                if agent.agent_id == agent_id {
                                    agent.attention = attention;
                                }
                            }
                        }
                        send_json(&mut ws_tx, &json::hello(&topics, &workdirs)).await?;
                    }
                    ServerMessage::Agent { agent_id, frame } => {
                        let state = states.entry(agent_id).or_insert_with(empty_state);
                        frame.apply_diff(state);
                        if selected == Some(agent_id) {
                            dirty = true;
                        }
                    }
                    ServerMessage::AgentCreated { agent_id, .. } => {
                        agent_ids.insert(agent_id.encoded(), agent_id);
                        send_json(&mut ws_tx, &ToBrowser::AgentCreated { agent_id: agent_id.encoded() }).await?;
                    }
                    ServerMessage::Error { message } => {
                        send_json(&mut ws_tx, &ToBrowser::Error { message }).await?;
                    }
                    _ => {}
                }
            }
            message = ws_rx.next() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => return Err(error).context("websocket"),
                    None => return Ok(()),
                };
                let Message::Text(text) = message else {
                    if matches!(message, Message::Close(_)) {
                        return Ok(());
                    }
                    continue;
                };
                let command = match serde_json::from_str::<FromBrowser>(&text) {
                    Ok(command) => command,
                    Err(error) => {
                        send_json(&mut ws_tx, &ToBrowser::Error { message: format!("bad command: {error}") }).await?;
                        continue;
                    }
                };
                match command {
                    FromBrowser::Select { agent_id } => {
                        let Some(&agent_id) = agent_ids.get(&agent_id) else { continue };
                        selected = Some(agent_id);
                        write_frame(&mut session_write, &ClientMessage::LoadAgent { agent_id }).await?;
                        if let Some(state) = states.get(&agent_id) {
                            send_json(&mut ws_tx, &ToBrowser::Agent {
                                agent_id: agent_id.encoded(),
                                state: json::agent_state(state),
                            }).await?;
                        }
                    }
                    FromBrowser::Send { agent_id, text } => {
                        let Some(&agent_id) = agent_ids.get(&agent_id) else { continue };
                        write_frame(&mut session_write, &ClientMessage::SendUserMessage {
                            agent_id,
                            content: vec![ContentPart::Text { text }],
                            delivery: MessageDelivery::Immediate,
                        }).await?;
                    }
                    FromBrowser::NewAgent { repo, text } => {
                        let Some(topic_id) = default_topic else { continue };
                        write_frame(&mut session_write, &ClientMessage::NewAgent {
                            topic_id,
                            mode: AgentMode::deep_default(),
                            start: StartMode::NewOn {
                                repo: Utf8PathBuf::from(repo),
                                revset: "@-".to_owned(),
                            },
                            content: Some(vec![ContentPart::Text { text }]),
                        }).await?;
                    }
                    FromBrowser::Cancel { agent_id } => {
                        let Some(&agent_id) = agent_ids.get(&agent_id) else { continue };
                        write_frame(&mut session_write, &ClientMessage::CancelTurn { agent_id }).await?;
                    }
                }
            }
            _ = push_tick.tick(), if dirty => {
                dirty = false;
                if let Some(agent_id) = selected
                    && let Some(state) = states.get(&agent_id)
                {
                    send_json(&mut ws_tx, &ToBrowser::Agent {
                        agent_id: agent_id.encoded(),
                        state: json::agent_state(state),
                    }).await?;
                }
            }
        }
    }
}

fn index_agent_ids(topics: &[UiTopic], agent_ids: &mut HashMap<String, AgentId>) {
    for topic in topics {
        for agent in &topic.agents {
            agent_ids.insert(agent.agent_id.encoded(), agent.agent_id);
        }
    }
}

fn empty_state() -> UiAgentState {
    UiAgentState {
        blocks: Vec::new(),
        status: rho_ui_proto::remote::UiAgentStatus::Idle,
        context_used: None,
    }
}

async fn send_json<S>(sink: &mut S, message: &ToBrowser) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let text = serde_json::to_string(message).context("encode browser message")?;
    sink.send(Message::Text(text.into()))
        .await
        .context("send browser message")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_websocket_upgrade_request() {
        let head = b"GET /ws?x=1 HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nSec-WebSocket-Key: abc123==\r\n\r\n";
        let request = parse_request_head(head).unwrap();
        assert_eq!(request.path, "/ws");
        assert_eq!(request.websocket_key.as_deref(), Some("abc123=="));
    }

    #[test]
    fn rejects_non_get_requests() {
        let head = b"POST / HTTP/1.1\r\n\r\n";
        assert!(parse_request_head(head).is_err());
    }
}
