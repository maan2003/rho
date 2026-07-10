//! JSON session for the static web UI.
//!
//! The page itself is static (`webui/` in the repository, hostable anywhere)
//! and connects over iroh with the [`rho_webui_messages::ALPN`] ALPN. Each
//! session speaks newline-delimited JSON and becomes a normal UI protocol
//! session through an in-process duplex pipe into
//! [`crate::serve_connection_io`], so the browser surface reuses the daemon's
//! message handling wholesale and this module only translates JSON to
//! protocol frames.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rho_core::ContentPart;
use rho_ui_proto::remote::UiAgentState;
use rho_ui_proto::{
    AgentConfig, AgentId, ClientMessage, MessageDelivery, ServerMessage, StartMode, UiTopic,
    UiWorkdir, read_frame, write_frame,
};
use rho_webui_messages::{FromBrowser, MAX_LINE_LEN, ToBrowser};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
};
use tokio::sync::mpsc;

use crate::AgentRegistry;

mod json;

/// Streamed transcript frames are coalesced onto this tick before the
/// selected agent's full state goes to the browser.
const PUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// One browser tab: an in-process UI protocol session, agent state mirrored
/// here, and only the selected agent's transcript forwarded.
pub(crate) async fn serve_json_session<R, W>(
    agents: Arc<AgentRegistry>,
    reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let (browser_side, daemon_side) = tokio::io::duplex(rho_ui_proto::MAX_FRAME_LEN.min(1 << 20));
    let (daemon_read, daemon_write) = tokio::io::split(daemon_side);
    tokio::spawn(async move {
        let counters = rho_ui_proto::IoCounters::default();
        // Ends when the browser side drops; disconnect errors are routine.
        let _ = crate::serve_connection_io(agents, daemon_read, daemon_write, counters, None).await;
    });
    let (session_read, mut session_write) = tokio::io::split(browser_side);
    write_frame(&mut session_write, &ClientMessage::Subscribe).await?;

    // Reads happen on their own tasks: neither `read_frame` nor a bounded
    // line read is cancellation-safe inside `select!`.
    let (server_tx, mut server_rx) = mpsc::unbounded_channel::<ServerMessage>();
    tokio::spawn(async move {
        let mut session_read = session_read;
        while let Ok(message) = read_frame::<_, ServerMessage>(&mut session_read).await {
            if server_tx.send(message).is_err() {
                break;
            }
        }
    });
    let (line_tx, mut line_rx) = mpsc::channel::<anyhow::Result<Option<String>>>(1);
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(reader);
        loop {
            let line = read_bounded_line(&mut reader).await;
            let done = matches!(line, Err(_) | Ok(None));
            if line_tx.send(line).await.is_err() || done {
                break;
            }
        }
    });

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
                        send_json(&mut writer, &json::hello(&topics, &workdirs)).await?;
                    }
                    ServerMessage::TopicCreated { topic } => {
                        topics.push(topic);
                        index_agent_ids(&topics, &mut agent_ids);
                        send_json(&mut writer, &json::hello(&topics, &workdirs)).await?;
                    }
                    ServerMessage::AgentAttention { agent_id, attention } => {
                        for topic in &mut topics {
                            for agent in &mut topic.agents {
                                if agent.agent_id == agent_id {
                                    agent.attention = attention;
                                }
                            }
                        }
                        send_json(&mut writer, &json::hello(&topics, &workdirs)).await?;
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
                        send_json(&mut writer, &ToBrowser::AgentCreated { agent_id: agent_id.encoded() }).await?;
                    }
                    ServerMessage::Error { message } => {
                        send_json(&mut writer, &ToBrowser::Error { message }).await?;
                    }
                    _ => {}
                }
            }
            line = line_rx.recv() => {
                let text = match line {
                    Some(Ok(Some(text))) => text,
                    Some(Ok(None)) | None => return Ok(()),
                    Some(Err(error)) => return Err(error).context("web UI stream"),
                };
                // Clients send a bare newline to materialize the QUIC stream.
                if text.trim().is_empty() {
                    continue;
                }
                let command = match serde_json::from_str::<FromBrowser>(&text) {
                    Ok(command) => command,
                    Err(error) => {
                        send_json(&mut writer, &ToBrowser::Error { message: format!("bad command: {error}") }).await?;
                        continue;
                    }
                };
                match command {
                    FromBrowser::Select { agent_id } => {
                        let Some(&agent_id) = agent_ids.get(&agent_id) else { continue };
                        selected = Some(agent_id);
                        write_frame(&mut session_write, &ClientMessage::LoadAgent { agent_id }).await?;
                        if let Some(state) = states.get(&agent_id) {
                            send_json(&mut writer, &ToBrowser::Agent {
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
                            mode: AgentConfig::default(),
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
                    send_json(&mut writer, &ToBrowser::Agent {
                        agent_id: agent_id.encoded(),
                        state: json::agent_state(state),
                    }).await?;
                }
            }
        }
    }
}

/// One text line, or `None` on clean end of stream. Bounded so a client
/// cannot grow the buffer without limit.
async fn read_bounded_line<R>(reader: &mut R) -> anyhow::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    let read = (&mut *reader)
        .take(MAX_LINE_LEN as u64 + 1)
        .read_until(b'\n', &mut line)
        .await
        .context("read web UI line")?;
    if read == 0 {
        return Ok(None);
    }
    anyhow::ensure!(line.len() <= MAX_LINE_LEN, "web UI line too long");
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    let text = String::from_utf8(line).context("web UI line is not UTF-8")?;
    Ok(Some(text))
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

async fn send_json<W>(writer: &mut W, message: &ToBrowser) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut text = serde_json::to_string(message).context("encode browser message")?;
    text.push('\n');
    writer
        .write_all(text.as_bytes())
        .await
        .context("send browser message")?;
    writer.flush().await.context("flush browser message")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_line_reader_splits_and_limits() {
        let mut reader = tokio::io::BufReader::new(&b"{\"a\":1}\ntrailing"[..]);
        assert_eq!(
            read_bounded_line(&mut reader).await.unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(
            read_bounded_line(&mut reader).await.unwrap().as_deref(),
            Some("trailing")
        );
        assert_eq!(read_bounded_line(&mut reader).await.unwrap(), None);

        let long = vec![b'x'; MAX_LINE_LEN + 1];
        let mut reader = tokio::io::BufReader::new(long.as_slice());
        assert!(read_bounded_line(&mut reader).await.is_err());
    }
}
