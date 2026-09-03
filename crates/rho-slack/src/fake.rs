//! An in-process stand-in for Slack: the endpoints rho calls, plus the RTM
//! websocket.
//!
//! It exists so the transport can be tested and demonstrated without a real
//! session — the transport tests drive it directly, and the isolated GUI QA
//! run points a whole rho-gui at it. It is deliberately literal about the
//! parts that bite: `ok: false` bodies, the multipart activity feed, and a
//! socket that answers Slack's JSON ping rather than the protocol one.
//!
//! The API and the websocket listen on two ports so the websocket handshake
//! is never preceded by bytes this server has already consumed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message as Frame;

/// A message rho sent, as the fake received it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posted {
    pub channel: String,
    pub thread_ts: Option<String>,
    pub text: String,
}

#[derive(Default)]
struct State {
    users: Vec<Value>,
    conversations: Vec<Value>,
    counts: Vec<Value>,
    feed: Vec<Value>,
    history: BTreeMap<String, Vec<Value>>,
    posted: Vec<Posted>,
    marked: Vec<(String, String)>,
    /// Requests that should fail with `ok: false`, by method name and how
    /// many times. This is how a poll-failure notice gets tested.
    failures: BTreeMap<String, usize>,
    calls: BTreeMap<String, usize>,
}

pub struct Fake {
    state: Arc<Mutex<State>>,
    frames: broadcast::Sender<Frame>,
    api_base: String,
    ws_url: String,
    self_id: String,
}

impl Fake {
    /// Starts the fake on two ephemeral ports and returns once both are
    /// accepting, so a test can connect immediately.
    pub async fn start() -> anyhow::Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (frames, _) = broadcast::channel(64);

        let api = TcpListener::bind("127.0.0.1:0").await?;
        let api_base = format!("http://{}/api", api.local_addr()?);
        let sockets = TcpListener::bind("127.0.0.1:0").await?;
        let ws_url = format!("ws://{}", sockets.local_addr()?);

        let fake = Self {
            state: state.clone(),
            frames: frames.clone(),
            api_base,
            ws_url: ws_url.clone(),
            self_id: "ME".to_owned(),
        };

        let api_state = state.clone();
        let api_ws = ws_url.clone();
        let api_frames = frames.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = api.accept().await {
                let state = api_state.clone();
                let ws_url = api_ws.clone();
                let frames = api_frames.clone();
                tokio::spawn(async move {
                    let _ = serve_api(stream, state, ws_url, frames).await;
                });
            }
        });
        tokio::spawn(async move {
            while let Ok((stream, _)) = sockets.accept().await {
                let frames = frames.subscribe();
                tokio::spawn(async move {
                    let _ = serve_socket(stream, frames).await;
                });
            }
        });
        Ok(fake)
    }

    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    /// The user id the fake signs rho in as.
    pub fn self_id(&self) -> &str {
        &self.self_id
    }

    pub fn add_user(&self, id: &str, name: &str) {
        self.state.lock().unwrap().users.push(json!({
            "id": id,
            "name": name,
            "profile": {"display_name": name},
        }));
    }

    pub fn add_channel(&self, id: &str, name: &str) {
        self.state
            .lock()
            .unwrap()
            .conversations
            .push(json!({"id": id, "name": name}));
    }

    pub fn add_dm(&self, id: &str, user: &str) {
        self.state
            .lock()
            .unwrap()
            .conversations
            .push(json!({"id": id, "is_im": true, "user": user}));
    }

    pub fn set_count(&self, channel: &str, has_unreads: bool, mentions: u32, latest: &str) {
        self.state.lock().unwrap().counts.push(json!({
            "id": channel,
            "has_unreads": has_unreads,
            "mention_count": mentions,
            "latest": latest,
        }));
    }

    /// Adds a message to a conversation's history, and to the activity feed
    /// when it is something the feed would carry.
    pub fn add_message(&self, channel: &str, message: Value) {
        self.state
            .lock()
            .unwrap()
            .history
            .entry(channel.to_owned())
            .or_default()
            .push(message);
    }

    pub fn add_feed_mention(&self, channel: &str, ts: &str) {
        self.state.lock().unwrap().feed.push(json!({
            "is_unread": true,
            "item": {"type": "at_user", "message": {"ts": ts, "channel": channel}},
        }));
    }

    pub fn add_feed_thread_reply(&self, channel: &str, thread_ts: &str, ts: &str) {
        self.state.lock().unwrap().feed.push(json!({
            "is_unread": true,
            "item": {"type": "thread_v2", "bundle_info": {"payload": {"thread_entry": {
                "channel_id": channel, "thread_ts": thread_ts, "latest_ts": ts,
            }}}},
        }));
    }

    /// Pushes a live frame to every connected socket.
    pub fn push_frame(&self, frame: Value) {
        let _ = self.frames.send(Frame::Text(frame.to_string().into()));
    }

    /// Drops every open socket, the way a network blip does.
    pub fn drop_sockets(&self) {
        let _ = self.frames.send(Frame::Close(None));
    }

    /// Makes the next `times` calls to `method` answer `ok: false`.
    pub fn fail_next(&self, method: &str, times: usize) {
        self.state
            .lock()
            .unwrap()
            .failures
            .insert(method.to_owned(), times);
    }

    pub fn posted(&self) -> Vec<Posted> {
        self.state.lock().unwrap().posted.clone()
    }

    pub fn marked(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().marked.clone()
    }

    pub fn calls(&self, method: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .calls
            .get(method)
            .copied()
            .unwrap_or(0)
    }
}

async fn serve_socket(
    stream: TcpStream,
    mut frames: broadcast::Receiver<Frame>,
) -> anyhow::Result<()> {
    let mut socket = tokio_tungstenite::accept_async(stream).await?;
    socket
        .send(Frame::Text(json!({"type": "hello"}).to_string().into()))
        .await?;
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(Frame::Close(_)) | Err(broadcast::error::RecvError::Closed) => return Ok(()),
                Ok(frame) => socket.send(frame).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            },
            incoming = socket.next() => {
                let Some(Ok(Frame::Text(text))) = incoming else {
                    return Ok(());
                };
                let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if value["type"] == json!("ping") {
                    let pong = json!({"type": "pong", "reply_to": value["id"]});
                    socket.send(Frame::Text(pong.to_string().into())).await?;
                }
            }
        }
    }
}

async fn serve_api(
    mut stream: TcpStream,
    state: Arc<Mutex<State>>,
    ws_url: String,
    _frames: broadcast::Sender<Frame>,
) -> anyhow::Result<()> {
    loop {
        let Some((path, body)) = read_request(&mut stream).await? else {
            return Ok(());
        };
        let method = path.rsplit('/').next().unwrap_or_default().to_owned();
        let response = handle(&method, &body, &state, &ws_url);
        let payload = response.to_string();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            payload.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(payload.as_bytes()).await?;
        stream.flush().await?;
    }
}

/// Reads one HTTP request, returning its path and body. Enough of HTTP/1.1
/// for a client we control: a request line, headers, and a `Content-Length`
/// body.
async fn read_request(stream: &mut TcpStream) -> anyhow::Result<Option<(String, String)>> {
    let mut buffer = Vec::new();
    let head_end = loop {
        if let Some(index) = find(&buffer, b"\r\n\r\n") {
            break index;
        }
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(Some((path, String::from_utf8_lossy(&body).into_owned())))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn handle(method: &str, body: &str, state: &Arc<Mutex<State>>, ws_url: &str) -> Value {
    let mut state = state.lock().unwrap();
    *state.calls.entry(method.to_owned()).or_default() += 1;
    if let Some(remaining) = state.failures.get_mut(method) {
        if *remaining > 0 {
            *remaining -= 1;
            return json!({"ok": false, "error": "fatal_error"});
        }
    }
    let form = parse_form(body);
    let field = |name: &str| form.get(name).cloned().unwrap_or_default();
    match method {
        "rtm.connect" => json!({
            "ok": true,
            "url": ws_url,
            "self": {"id": "ME", "name": "you"},
            "team": {"name": "acme"},
        }),
        "users.list" => json!({"ok": true, "members": state.users}),
        "users.info" => {
            let id = field("user");
            let user = state
                .users
                .iter()
                .find(|user| user["id"] == json!(id))
                .cloned();
            match user {
                Some(user) => json!({"ok": true, "user": user}),
                None => json!({"ok": false, "error": "user_not_found"}),
            }
        }
        "users.conversations" => json!({"ok": true, "channels": state.conversations}),
        "conversations.info" => {
            let id = field("channel");
            match state
                .conversations
                .iter()
                .find(|conversation| conversation["id"] == json!(id))
                .cloned()
            {
                Some(conversation) => json!({"ok": true, "channel": conversation}),
                None => json!({"ok": false, "error": "channel_not_found"}),
            }
        }
        "client.counts" => json!({"ok": true, "channels": state.counts, "mpims": [], "ims": []}),
        "activity.feed" => {
            let items = state.feed.clone();
            json!({"ok": true, "items": items})
        }
        "conversations.history" => {
            let channel = field("channel");
            let mut messages = state.history.get(&channel).cloned().unwrap_or_default();
            // Slack hands history back newest first.
            messages.reverse();
            json!({"ok": true, "messages": messages, "has_more": false})
        }
        "conversations.replies" => {
            let channel = field("channel");
            let root = field("ts");
            let messages = state
                .history
                .get(&channel)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|message| {
                    message["ts"] == json!(root) || message["thread_ts"] == json!(root)
                })
                .collect::<Vec<_>>();
            json!({"ok": true, "messages": messages, "has_more": false})
        }
        "conversations.mark" => {
            state.marked.push((field("channel"), field("ts")));
            json!({"ok": true})
        }
        "chat.postMessage" => {
            let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let channel = payload["channel"].as_str().unwrap_or_default().to_owned();
            let thread_ts = payload["thread_ts"].as_str().map(str::to_owned);
            let text = payload["text"].as_str().unwrap_or_default().to_owned();
            // The fake assigns timestamps the way Slack does: monotonically,
            // so a reply is always newer than what it answers.
            // Newer than anything seeded, so a reply lands at the end of the
            // transcript exactly as Slack's own timestamps would put it.
            let ts = format!(
                "{}.000000",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| since.as_secs())
                    .unwrap_or_default()
                    + state.posted.len() as u64
            );
            let mut message = json!({
                "ts": ts,
                "user": "ME",
                "text": text,
                "channel": channel,
                "type": "message",
            });
            if let Some(thread_ts) = &thread_ts {
                message["thread_ts"] = json!(thread_ts);
            }
            state
                .history
                .entry(channel.clone())
                .or_default()
                .push(message.clone());
            state.posted.push(Posted {
                channel,
                thread_ts,
                text,
            });
            json!({"ok": true, "ts": ts, "message": message})
        }
        _ => json!({"ok": false, "error": "unknown_method"}),
    }
}

/// Parses both `application/x-www-form-urlencoded` and the multipart body the
/// activity feed uses, since the fake only needs the field values.
fn parse_form(body: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    if body.contains("Content-Disposition: form-data") {
        for part in body.split("--") {
            let Some((head, value)) = part.split_once("\r\n\r\n") else {
                continue;
            };
            let Some(name) = head
                .split("name=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
            else {
                continue;
            };
            fields.insert(
                name.to_owned(),
                value.trim_end_matches("\r\n").trim_end().to_owned(),
            );
        }
        return fields;
    }
    for pair in body.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        fields.insert(decode(name), decode(value));
    }
    fields
}

fn decode(value: &str) -> String {
    let bytes = value.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
