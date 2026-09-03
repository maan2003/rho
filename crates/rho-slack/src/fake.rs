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

/// Two 16x16 PNGs, so avatar fetching has real bytes to decode without a
/// fixture file on disk.
const AVATAR_BLUE: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 16, 0, 0, 0, 16, 8, 2,
    0, 0, 0, 144, 145, 104, 54, 0, 0, 0, 22, 73, 68, 65, 84, 120, 218, 99, 112, 107, 58, 65, 18,
    98, 24, 213, 48, 170, 97, 248, 106, 0, 0, 36, 236, 144, 16, 219, 143, 0, 170, 0, 0, 0, 0, 73,
    69, 78, 68, 174, 66, 96, 130,
];
const AVATAR_GREEN: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 16, 0, 0, 0, 16, 8, 2,
    0, 0, 0, 144, 145, 104, 54, 0, 0, 0, 22, 73, 68, 65, 84, 120, 218, 99, 136, 90, 149, 71, 18,
    98, 24, 213, 48, 170, 97, 248, 106, 0, 0, 160, 58, 114, 16, 42, 13, 108, 35, 0, 0, 0, 0, 73,
    69, 78, 68, 174, 66, 96, 130,
];

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
    /// Custom workspace emoji, as `emoji.list` returns them.
    emoji: BTreeMap<String, String>,
    /// The threads Slack follows for the user, as
    /// `subscriptions.thread.getView` lists them: (channel, thread_ts).
    followed: Vec<(String, String)>,
    /// Requests that should fail with `ok: false`, by method name and how
    /// many times. This is how a poll-failure notice gets tested.
    failures: BTreeMap<String, usize>,
    calls: BTreeMap<String, usize>,
    /// The form fields of every call to each method, in order, so a test can
    /// assert what rho asked for and not merely that it asked.
    forms: BTreeMap<String, Vec<BTreeMap<String, String>>>,
    /// How long the bytes of a file take to arrive. Slack is not instant and
    /// a QA run has to be able to look at what the reader sees while a
    /// picture is still coming.
    file_delay_ms: u64,
    /// How long a send takes to be accepted, and whether it is accepted at
    /// all. Both are what a QA run needs to see the states a real network
    /// puts the composer in: a message on its way, and one refused.
    send_delay_ms: u64,
    send_fails: bool,
    /// Bytes that arrived through the upload URL, by the file id the
    /// upload was reserved under. Serving them back at `/files/` is what
    /// makes a sent picture the same round trip as a received one.
    uploads: BTreeMap<String, Vec<u8>>,
    /// Where the fake is listening, because Slack hands out an absolute
    /// upload URL and the client posts the bytes wherever it is told.
    api_base: String,
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
        state.lock().unwrap().api_base = api_base.clone();
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

    /// Where a QA run POSTs live events. Same host as the API, because the
    /// mocking is entirely server side.
    pub fn control_url(&self) -> String {
        format!("{}/control", self.api_base.trim_end_matches("/api"))
    }

    /// The user id the fake signs rho in as.
    pub fn self_id(&self) -> &str {
        &self.self_id
    }

    pub fn add_user(&self, id: &str, name: &str) {
        self.add_user_named(id, name, name);
    }

    /// A user whose display name differs from the handle, which is the
    /// normal case in a real workspace and the one that catches a client
    /// rendering the handle.
    pub fn add_user_named(&self, id: &str, handle: &str, display: &str) {
        // Every user carries a picture and the hash that decides whether a
        // cached copy is still current, because that pair is what the
        // avatar cache keys on.
        let hash = format!("{}av", handle);
        self.state.lock().unwrap().users.push(json!({
            "id": id,
            "name": handle,
            "profile": {
                "display_name": display,
                "real_name": display,
                "image_48": format!("{}/avatars/{hash}.png", self.api_base.trim_end_matches("/api")),
            },
            "avatar_hash": hash,
        }));
    }

    /// The picture a bot posts under, which lives in a different place in
    /// the payload than a person's.
    pub fn bot_icon_url(&self) -> String {
        format!(
            "{}/avatars/botav.png",
            self.api_base.trim_end_matches("/api")
        )
    }

    pub fn add_channel(&self, id: &str, name: &str) {
        self.state
            .lock()
            .unwrap()
            .conversations
            .push(json!({"id": id, "name": name}));
    }

    /// A private channel: the same as a channel to everything except the
    /// flag rho keys nothing on yet, which is exactly why it is seeded.
    pub fn add_private_channel(&self, id: &str, name: &str) {
        self.state
            .lock()
            .unwrap()
            .conversations
            .push(json!({"id": id, "name": name, "is_private": true}));
    }

    /// A group DM, named the way Slack names one: an `mpdm-` string nobody
    /// should ever see.
    pub fn add_group(&self, id: &str, name: &str, members: &[&str]) {
        self.state.lock().unwrap().conversations.push(json!({
            "id": id,
            "name": name,
            "is_mpim": true,
            "is_group": true,
            "members": members,
        }));
    }

    pub fn add_dm(&self, id: &str, user: &str) {
        self.state
            .lock()
            .unwrap()
            .conversations
            .push(json!({"id": id, "is_im": true, "user": user}));
    }

    /// Where the unread rule goes: Slack's own read cursor for the
    /// conversation, which `conversations.info` carries.
    pub fn set_last_read(&self, channel: &str, ts: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(conversation) = state
            .conversations
            .iter_mut()
            .find(|conversation| conversation["id"] == json!(channel))
        {
            conversation["last_read"] = json!(ts);
        }
    }

    pub fn add_emoji(&self, name: &str, url: &str) {
        self.state
            .lock()
            .unwrap()
            .emoji
            .insert(name.to_owned(), url.to_owned());
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

    /// Slack follows a thread for the user when they post in it or are
    /// mentioned in it. Seeding one is how a test reaches the state a phone
    /// reply leaves behind: followed, with rho having never seen a thing.
    pub fn follow_thread(&self, channel: &str, thread_ts: &str) {
        let mut state = self.state.lock().unwrap();
        let entry = (channel.to_owned(), thread_ts.to_owned());
        if !state.followed.contains(&entry) {
            state.followed.push(entry);
        }
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

    /// Drives one live event: the fake mutates the workspace and pushes the
    /// frame Slack would push. Every QA run and every live test goes through
    /// here, over the `/control` route or this method, so the client under
    /// test is never modified to produce a state.
    pub fn live(&self, request: Value) -> Value {
        apply_live(&mut self.state.lock().unwrap(), &self.frames, &request)
    }

    /// A new message in a conversation. Returns its timestamp, which the
    /// reaction, edit, and delete events take as their target.
    pub fn live_message(&self, channel: &str, user: &str, text: &str) -> String {
        self.live_ts(json!({"kind": "message", "channel": channel, "user": user, "text": text}))
    }

    pub fn live_reply(&self, channel: &str, thread_ts: &str, user: &str, text: &str) -> String {
        self.live_ts(json!({
            "kind": "reply",
            "channel": channel,
            "thread_ts": thread_ts,
            "user": user,
            "text": text,
        }))
    }

    pub fn live_reaction(&self, channel: &str, ts: &str, user: &str, name: &str) {
        self.live(json!({
            "kind": "reaction",
            "channel": channel,
            "ts": ts,
            "user": user,
            "name": name,
        }));
    }

    pub fn live_edit(&self, channel: &str, ts: &str, text: &str) {
        self.live(json!({"kind": "edit", "channel": channel, "ts": ts, "text": text}));
    }

    pub fn live_delete(&self, channel: &str, ts: &str) {
        self.live(json!({"kind": "delete", "channel": channel, "ts": ts}));
    }

    fn live_ts(&self, request: Value) -> String {
        self.live(request)["ts"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
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

    /// A field of the last call to `method`, for asserting that a refresh
    /// bounded itself by what the mirror already holds.
    pub fn last_field(&self, method: &str, field: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .forms
            .get(method)?
            .last()?
            .get(field)
            .cloned()
    }

    /// A field of every call to `method`, in the order they were made. A
    /// request budget is a shape, not a number, so a test asserts both.
    pub fn fields(&self, method: &str, field: &str) -> Vec<Option<String>> {
        self.state
            .lock()
            .unwrap()
            .forms
            .get(method)
            .map(|calls| {
                calls
                    .iter()
                    .map(|form| form.get(field).cloned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
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
    let mut socket = tokio_tungstenite::accept_hdr_async(stream, check_handshake).await?;
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

/// Slack refuses an `xoxc` websocket handshake that does not carry the web
/// session, and has since 2023. The fake refuses one too, so a client that
/// forgets a header fails here rather than going silent in front of the user.
fn check_handshake(
    request: &tokio_tungstenite::tungstenite::handshake::server::Request,
    response: tokio_tungstenite::tungstenite::handshake::server::Response,
) -> Result<
    tokio_tungstenite::tungstenite::handshake::server::Response,
    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
> {
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let refused = header("authorization").is_empty()
        || !header("cookie").contains("d=")
        || header("user-agent").is_empty()
        || header("origin").is_empty();
    if refused {
        let mut error =
            tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(Some(
                json!({"type": "error", "error": {"msg": "invalid_auth", "code": 401}}).to_string(),
            ));
        *error.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
        return Err(error);
    }
    Ok(response)
}

async fn serve_api(
    mut stream: TcpStream,
    state: Arc<Mutex<State>>,
    ws_url: String,
    frames: broadcast::Sender<Frame>,
) -> anyhow::Result<()> {
    loop {
        let Some((path, body)) = read_request(&mut stream).await? else {
            return Ok(());
        };
        // Avatars and file downloads are bytes, not JSON: the same server
        // serves them because that is how Slack does it, on the same host
        // and behind the same credentials.
        // The upload URL Slack hands out: the bytes go straight to it, and
        // the fake keeps them so the picture it serves afterwards is the
        // one that was sent.
        if let Some(id) = path.strip_prefix("/upload/") {
            state
                .lock()
                .unwrap()
                .uploads
                .insert(id.to_owned(), body.clone());
            write_json(&mut stream, &json!({"ok": true})).await?;
            continue;
        }
        if let Some(bytes) = binary_route(&path, &state) {
            let delay = match path.starts_with("/files/") {
                true => state.lock().unwrap().file_delay_ms,
                false => 0,
            };
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                bytes.len()
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(&bytes).await?;
            stream.flush().await?;
            continue;
        }
        // The control route is not Slack's; it is how a QA run asks the fake
        // to behave like a workspace someone else is typing in. It stays on
        // the server so nothing about the client changes under test.
        let body = String::from_utf8_lossy(&body).into_owned();
        if path == "/control" {
            let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let response = apply_live(&mut state.lock().unwrap(), &frames, &request);
            write_json(&mut stream, &response).await?;
            continue;
        }
        let method = path.rsplit('/').next().unwrap_or_default().to_owned();
        // Slack is not instant either: the wait is here, before anything is
        // stored or echoed, so the client shows a sent message the way it
        // does when the network is slow rather than absent.
        if method == "chat.postMessage" {
            let delay = state.lock().unwrap().send_delay_ms;
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
        let response = handle(&method, &body, &state, &ws_url, &frames);
        write_json(&mut stream, &response).await?;
    }
}

async fn write_json(stream: &mut TcpStream, response: &Value) -> anyhow::Result<()> {
    let payload = response.to_string();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(payload.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// The byte routes: avatars, and the file an attachment points at. Two
/// colours so a screenshot shows that the right picture reached the right
/// author.
fn binary_route(path: &str, state: &Arc<Mutex<State>>) -> Option<Vec<u8>> {
    if path.starts_with("/thumbs/") {
        // Slack's smallest thumbnail: the placeholder, so it is tiny on
        // purpose and blurs when the box blows it up.
        static THUMB: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        return Some(THUMB.get_or_init(|| preview_png(64, 40)).clone());
    }
    if let Some(name) = path.strip_prefix("/files/") {
        // A picture the reader sent themselves comes back as the bytes they
        // sent, so the message they see is not a different picture.
        let id = name.split('/').next().unwrap_or_default();
        if let Some(bytes) = state.lock().unwrap().uploads.get(id) {
            return Some(bytes.clone());
        }
        // A file is what a preview is judged on, so it is big enough to
        // look at rather than a 16-pixel square.
        static PREVIEW: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        return Some(PREVIEW.get_or_init(|| preview_png(320, 200)).clone());
    }
    let name = path.strip_prefix("/avatars/")?.trim_end_matches(".png");
    Some(
        match name {
            "davidav" | "adaav" | "botav" => AVATAR_BLUE,
            _ => AVATAR_GREEN,
        }
        .to_vec(),
    )
}

/// Reads one HTTP request, returning its path and body. Enough of HTTP/1.1
/// for a client we control: a request line, headers, and a `Content-Length`
/// body.
async fn read_request(stream: &mut TcpStream) -> anyhow::Result<Option<(String, Vec<u8>)>> {
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
    Ok(Some((path, body)))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// One live event: the workspace changes and the frame Slack would send goes
/// out together, so what the socket announces and what a reopened
/// conversation shows can never disagree.
fn apply_live(state: &mut State, frames: &broadcast::Sender<Frame>, request: &Value) -> Value {
    let field = |name: &str| request[name].as_str().unwrap_or_default().to_owned();
    let channel = field("channel");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    // Slack's own event_ts is a hair later than the message it is about.
    let event_ts = format!("{now}.000100");
    let push = |frame: Value| {
        let _ = frames.send(Frame::Text(frame.to_string().into()));
    };
    match request["kind"].as_str().unwrap_or_default() {
        // What the budget is judged on: how many calls each method has
        // taken, so a QA run can say what one keypress cost.
        "calls" => {
            let calls = state
                .calls
                .iter()
                .map(|(method, count)| (method.clone(), json!(count)))
                .collect::<serde_json::Map<_, _>>();
            return json!({"ok": true, "calls": calls});
        }
        kind @ ("message" | "reply") => {
            let text = field("text");
            let user = match field("user").as_str() {
                "" => "UD".to_owned(),
                user => user.to_owned(),
            };
            let ts = next_ts(state, &channel, now);
            let mut message = json!({"type": "message", "ts": ts, "user": user, "text": text});
            if kind == "reply" {
                message["thread_ts"] = json!(field("thread_ts"));
            }
            state
                .history
                .entry(channel.clone())
                .or_default()
                .push(message.clone());
            if kind == "reply" {
                // Slack keeps the thread's shape on its parent, so anything
                // that re-sends the parent later carries the count with it.
                let thread_ts = field("thread_ts");
                if let Some(parent) = message_mut(state, &channel, &thread_ts) {
                    let count = parent["reply_count"].as_u64().unwrap_or_default() + 1;
                    parent["reply_count"] = json!(count);
                    parent["latest_reply"] = json!(ts);
                }
            }
            let mentions_me = text.contains("<@ME>");
            bump_count(state, &channel, &ts, mentions_me);
            // The feed is the truth rho falls back on, so a live event that
            // would oblige the user has to appear there too, not only on the
            // socket.
            if mentions_me {
                state.feed.push(json!({
                    "is_unread": true,
                    "item": {"type": "at_user", "message": {"ts": ts, "channel": channel}},
                }));
            } else if kind == "reply" {
                let thread_ts = field("thread_ts");
                state.feed.push(json!({
                    "is_unread": true,
                    "item": {"type": "thread_v2", "bundle_info": {"payload": {"thread_entry": {
                        "channel_id": channel, "thread_ts": thread_ts, "latest_ts": ts,
                    }}}},
                }));
            }
            let mut frame = message;
            frame["channel"] = json!(channel);
            push(frame);
            json!({"ok": true, "ts": ts})
        }
        // Following and unfollowing, the way any Slack client does it: the
        // list the next connect reads and the live frame move together.
        // How slowly a picture arrives, so a QA run can look at the box
        // while it is still on its way.
        "file_delay" => {
            state.file_delay_ms = request["ms"].as_u64().unwrap_or_default();
            return json!({"ok": true});
        }
        "send_delay" => {
            state.send_delay_ms = request["ms"].as_u64().unwrap_or_default();
            return json!({"ok": true});
        }
        "send_fail" => {
            state.send_fails = request["fail"].as_bool().unwrap_or(true);
            return json!({"ok": true});
        }
        kind @ ("subscribe" | "unsubscribe") => {
            let thread_ts = field("thread_ts");
            let entry = (channel.clone(), thread_ts.clone());
            state.followed.retain(|held| held != &entry);
            if kind == "subscribe" {
                state.followed.push(entry);
            }
            push(json!({
                "type": if kind == "subscribe" { "thread_subscribed" } else { "thread_unsubscribed" },
                "subscription": {
                    "type": "thread",
                    "channel": channel,
                    "thread_ts": thread_ts,
                    "last_read": "0000000000.000000",
                },
                "event_ts": event_ts,
            }));
            return json!({"ok": true});
        }
        "reaction" => {
            let ts = field("ts");
            let name = field("name");
            let user = match field("user").as_str() {
                "" => "UD".to_owned(),
                user => user.to_owned(),
            };
            // Taking one off is the same route: Slack has one reaction on a
            // message from the client's side, added or removed.
            let removing = request["remove"].as_bool().unwrap_or(false);
            let Some(message) = message_mut(state, &channel, &ts) else {
                return json!({"ok": false, "error": "message_not_found"});
            };
            let mut reactions = message["reactions"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|mut reaction| {
                    if reaction["name"] == json!(name) {
                        let mut users = reaction["users"].as_array().cloned().unwrap_or_default();
                        if removing {
                            users.retain(|held| held != &json!(user));
                        } else {
                            users.push(json!(user));
                        }
                        reaction["count"] = json!(users.len());
                        reaction["users"] = json!(users);
                    }
                    reaction
                })
                .filter(|reaction| reaction["count"] != json!(0))
                .collect::<Vec<_>>();
            let had = reactions
                .iter()
                .any(|reaction| reaction["name"] == json!(name));
            if !had && !removing {
                reactions.push(json!({"name": name, "users": [user], "count": 1}));
            }
            message["reactions"] = json!(reactions);
            push(json!({
                "type": if removing { "reaction_removed" } else { "reaction_added" },
                "user": user,
                "reaction": name,
                "item": {"type": "message", "channel": channel, "ts": ts},
                "event_ts": event_ts,
            }));
            json!({"ok": true, "ts": ts})
        }
        "edit" => {
            let ts = field("ts");
            let text = field("text");
            let Some(message) = message_mut(state, &channel, &ts) else {
                return json!({"ok": false, "error": "message_not_found"});
            };
            let previous = message.clone();
            message["text"] = json!(text);
            message["edited"] = json!({"user": previous["user"].clone(), "ts": event_ts});
            let edited = message.clone();
            push(json!({
                "type": "message",
                "subtype": "message_changed",
                "channel": channel,
                "ts": event_ts,
                "message": edited,
                "previous_message": previous,
            }));
            json!({"ok": true, "ts": ts})
        }
        "delete" => {
            let ts = field("ts");
            let Some(messages) = state.history.get_mut(&channel) else {
                return json!({"ok": false, "error": "channel_not_found"});
            };
            let before = messages.len();
            messages.retain(|message| message["ts"] != json!(ts));
            if messages.len() == before {
                return json!({"ok": false, "error": "message_not_found"});
            }
            push(json!({
                "type": "message",
                "subtype": "message_deleted",
                "channel": channel,
                "ts": event_ts,
                "deleted_ts": ts,
            }));
            json!({"ok": true, "ts": ts})
        }
        _ => json!({"ok": false, "error": "unknown_kind"}),
    }
}

/// A timestamp newer than everything already in the conversation, so a live
/// message always lands at the end of the transcript. Seeded history can sit
/// in the future of the wall clock when a fixture is minutes old.
fn next_ts(state: &State, channel: &str, now: u64) -> String {
    let newest = state
        .history
        .get(channel)
        .and_then(|messages| messages.last())
        .and_then(|message| message["ts"].as_str())
        .and_then(|ts| ts.split('.').next()?.parse::<u64>().ok())
        .unwrap_or(0);
    format!("{}.000000", now.max(newest + 1))
}

/// The size in the PNG header, which is what Slack would have measured for
/// itself: the box a picture is drawn in comes from these two numbers.
fn png_size(bytes: &[u8]) -> (u32, u32) {
    let number = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|slice| u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
            .unwrap_or_default()
    };
    match bytes.starts_with(b"\x89PNG") {
        true => (number(16), number(20)),
        false => (0, 0),
    }
}

fn message_mut<'a>(state: &'a mut State, channel: &str, ts: &str) -> Option<&'a mut Value> {
    state
        .history
        .get_mut(channel)?
        .iter_mut()
        .find(|message| message["ts"] == json!(ts))
}

/// Moves the unread counter the conversation list reads, the way the server
/// does when someone else posts.
fn bump_count(state: &mut State, channel: &str, ts: &str, mentions_me: bool) {
    let Some(count) = state
        .counts
        .iter_mut()
        .find(|count| count["id"] == json!(channel))
    else {
        state.counts.push(json!({
            "id": channel,
            "has_unreads": true,
            "mention_count": u32::from(mentions_me),
            "latest": ts,
        }));
        return;
    };
    count["has_unreads"] = json!(true);
    count["latest"] = json!(ts);
    if mentions_me {
        let mentions = count["mention_count"].as_u64().unwrap_or(0) + 1;
        count["mention_count"] = json!(mentions);
    }
}

fn handle(
    method: &str,
    body: &str,
    state: &Arc<Mutex<State>>,
    ws_url: &str,
    frames: &broadcast::Sender<Frame>,
) -> Value {
    let mut state = state.lock().unwrap();
    *state.calls.entry(method.to_owned()).or_default() += 1;
    if let Some(remaining) = state.failures.get_mut(method) {
        if *remaining > 0 {
            *remaining -= 1;
            return json!({"ok": false, "error": "fatal_error"});
        }
    }
    let form = parse_form(body);
    state
        .forms
        .entry(method.to_owned())
        .or_default()
        .push(form.clone());
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
        "emoji.list" => json!({"ok": true, "emoji": state.emoji}),
        // Slack's own client sends this when a reader leaves a thread; it is
        // the only way to quiet a thread's unread badge without posting.
        "subscriptions.thread.mark" => json!({"ok": true}),
        "subscriptions.thread.add" => {
            let entry = (field("channel"), field("thread_ts"));
            if !state.followed.contains(&entry) {
                state.followed.push(entry);
            }
            json!({"ok": true})
        }
        // Ignore thread. The follow list is Slack's, so the removal shows up
        // in `getView` afterwards exactly as it would live.
        "subscriptions.thread.remove" => {
            let (channel, thread_ts) = (field("channel"), field("thread_ts"));
            state
                .followed
                .retain(|followed| *followed != (channel.clone(), thread_ts.clone()));
            json!({"ok": true})
        }
        "subscriptions.thread.getView" => json!({
            "ok": true,
            "threads": state
                .followed
                .iter()
                .map(|(channel, thread_ts)| json!({
                    "root_msg": {"channel": channel, "ts": thread_ts, "thread_ts": thread_ts},
                    "last_read": "0000000000.000000",
                    "unread_replies": 0,
                }))
                .collect::<Vec<_>>(),
        }),
        "client.counts" => {
            // Slack carries the read cursor here as well as in
            // `conversations.info`; it is the same fact, and this is the
            // call every client makes at startup.
            let counts = state
                .counts
                .iter()
                .map(|count| {
                    let mut count = count.clone();
                    let read = state
                        .conversations
                        .iter()
                        .find(|conversation| conversation["id"] == count["id"])
                        .and_then(|conversation| conversation["last_read"].as_str())
                        .unwrap_or_default()
                        .to_owned();
                    count["last_read"] = json!(read);
                    count
                })
                .collect::<Vec<_>>();
            json!({"ok": true, "channels": counts, "mpims": [], "ims": []})
        }
        "activity.feed" => {
            let items = state.feed.clone();
            json!({"ok": true, "items": items})
        }
        "conversations.history" => {
            let channel = field("channel");
            let mut messages = state.history.get(&channel).cloned().unwrap_or_default();
            // Slack hands history back newest first, and pages backwards from
            // there, so the cursor counts messages already handed out.
            messages.reverse();
            // `oldest` is how a mirrored conversation refreshes: only what
            // it does not already hold.
            // `latest` is the window a ping opens: everything up to and
            // including the message the notification named.
            let latest = field("latest");
            if !latest.is_empty() {
                let ceiling = latest.parse::<f64>().unwrap_or(f64::MAX);
                let inclusive = field("inclusive") == "true";
                messages.retain(|message| {
                    message["ts"]
                        .as_str()
                        .and_then(|ts| ts.parse::<f64>().ok())
                        .is_some_and(|ts| {
                            if inclusive {
                                ts <= ceiling
                            } else {
                                ts < ceiling
                            }
                        })
                });
            }
            let oldest = field("oldest");
            if !oldest.is_empty() {
                let floor = oldest.parse::<f64>().unwrap_or(0.0);
                messages.retain(|message| {
                    message["ts"]
                        .as_str()
                        .and_then(|ts| ts.parse::<f64>().ok())
                        .is_some_and(|ts| ts > floor)
                });
            }
            let limit = field("limit").parse::<usize>().unwrap_or(50).max(1);
            let start = field("cursor").parse::<usize>().unwrap_or(0);
            // `oldest` on its own pages forward: Slack answers with the
            // messages closest to it, not with the newest in the range. It
            // is what a catch-up and the after-half of a ping's window ask
            // for, and on a long conversation the two are nowhere near each
            // other. With `latest` set the window is bounded above instead,
            // and paging runs backwards from there as usual.
            let forward = !oldest.is_empty() && latest.is_empty();
            let (page, has_more, next) = if forward {
                let from = messages.len().saturating_sub(limit);
                (
                    messages.get(from..).unwrap_or_default().to_vec(),
                    from > 0,
                    from,
                )
            } else {
                let end = (start + limit).min(messages.len());
                (
                    messages.get(start..end).unwrap_or_default().to_vec(),
                    end < messages.len(),
                    end,
                )
            };
            json!({
                "ok": true,
                "messages": page,
                "has_more": has_more,
                "response_metadata": {
                    "next_cursor": if has_more { next.to_string() } else { String::new() },
                },
            })
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
            let (channel, ts) = (field("channel"), field("ts"));
            // Reading moves the cursor, here as on the server: the next
            // client to ask sees the conversation read up to this message.
            if let Some(conversation) = state
                .conversations
                .iter_mut()
                .find(|conversation| conversation["id"] == json!(channel))
            {
                conversation["last_read"] = json!(ts);
            }
            state.marked.push((channel, ts));
            json!({"ok": true})
        }
        // Slack's two-step upload: reserve a URL, POST the bytes to it, then
        // say where the file goes. The fake follows the same order, so a
        // client that skips a step gets nothing.
        "files.getUploadURLExternal" => {
            let id = format!("FUP{}", state.uploads.len() + state.calls.len());
            let url = format!("{}/upload/{id}", state.api_base.trim_end_matches("/api"));
            json!({"ok": true, "upload_url": url, "file_id": id})
        }
        "files.completeUploadExternal" => {
            let channel = field("channel_id");
            let thread_ts = match field("thread_ts") {
                empty if empty.is_empty() => None,
                thread_ts => Some(thread_ts),
            };
            let files: Value = serde_json::from_str(&field("files")).unwrap_or(Value::Null);
            let id = files[0]["id"].as_str().unwrap_or_default().to_owned();
            let title = files[0]["title"].as_str().unwrap_or("file").to_owned();
            let bytes = state.uploads.get(&id).cloned().unwrap_or_default();
            if bytes.is_empty() {
                return json!({"ok": false, "error": "upload_not_found"});
            }
            let (width, height) = png_size(&bytes);
            let base = state.api_base.trim_end_matches("/api").to_owned();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or_default();
            let ts = next_ts(&state, &channel, now);
            let mut message = json!({
                "type": "message",
                "ts": ts,
                "user": "ME",
                "channel": channel,
                "text": field("initial_comment"),
                "files": [{
                    "id": id,
                    "name": title,
                    "title": title,
                    "mimetype": "image/png",
                    "filetype": "png",
                    "size": bytes.len(),
                    "url_private": format!("{base}/files/{id}/{title}"),
                    "original_w": width,
                    "original_h": height,
                    "thumb_64": format!("{base}/thumbs/{id}.png"),
                }],
            });
            if let Some(thread_ts) = &thread_ts {
                message["thread_ts"] = json!(thread_ts);
            }
            state
                .history
                .entry(channel.clone())
                .or_default()
                .push(message.clone());
            let _ = frames.send(Frame::Text(message.to_string().into()));
            json!({"ok": true, "files": [{"id": id, "title": title}]})
        }
        // An edit is `chat.update` plus the socket event every other client
        // sees, so the round trip a reader makes here is the live one.
        "chat.update" => {
            let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let channel = payload["channel"].as_str().unwrap_or_default().to_owned();
            let ts = payload["ts"].as_str().unwrap_or_default().to_owned();
            let text = payload["text"].as_str().unwrap_or_default().to_owned();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or_default();
            let event_ts = format!("{now}.000100");
            let Some(message) = message_mut(&mut state, &channel, &ts) else {
                return json!({"ok": false, "error": "message_not_found"});
            };
            let previous = message.clone();
            message["text"] = json!(text);
            message["edited"] = json!({"user": previous["user"].clone(), "ts": event_ts});
            let edited = message.clone();
            let _ = frames.send(Frame::Text(
                json!({
                    "type": "message",
                    "subtype": "message_changed",
                    "channel": channel,
                    "ts": event_ts,
                    "message": edited,
                    "previous_message": previous,
                })
                .to_string()
                .into(),
            ));
            json!({"ok": true, "ts": ts, "text": text})
        }
        "chat.postMessage" => {
            if state.send_fails {
                return json!({"ok": false, "error": "message_not_sent"});
            }
            let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let channel = payload["channel"].as_str().unwrap_or_default().to_owned();
            let thread_ts = payload["thread_ts"].as_str().map(str::to_owned);
            let text = payload["text"].as_str().unwrap_or_default().to_owned();
            // The fake assigns timestamps the way Slack does: monotonically,
            // and newer than anything already in the conversation, so a sent
            // message lands at the end of the transcript rather than in the
            // middle of a fixture seeded to today's office hours.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or_default()
                + state.posted.len() as u64;
            let ts = next_ts(&state, &channel, now);
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
            // Slack follows a thread for whoever posts in it, and says so on
            // the socket. That is the whole of how a reply, from here or from
            // the phone, makes the thread the user's.
            if let Some(root) = &thread_ts {
                let entry = (channel.clone(), root.clone());
                if !state.followed.contains(&entry) {
                    state.followed.push(entry);
                }
                let _ = frames.send(Frame::Text(
                    json!({
                        "type": "thread_subscribed",
                        "subscription": {
                            "type": "thread",
                            "channel": channel,
                            "thread_ts": root,
                            "last_read": ts,
                        },
                    })
                    .to_string()
                    .into(),
                ));
            }
            state.posted.push(Posted {
                channel,
                thread_ts,
                text,
            });
            // Slack echoes the sender's own message back down the socket;
            // rho has to survive seeing it twice, and a client that only
            // paints on the echo has to paint at all.
            let _ = frames.send(Frame::Text(message.to_string().into()));
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

/// The same picture the fake serves, for a test that needs bytes to send.
pub fn sample_png(width: u32, height: u32) -> Vec<u8> {
    preview_png(width, height)
}

/// A recognisable picture, built rather than embedded: a colour wash with a
/// diagonal through it, so a screenshot shows whether the image reached the
/// screen the right way up and at the right size.
fn preview_png(width: u32, height: u32) -> Vec<u8> {
    let mut raw = Vec::new();
    for y in 0..height {
        raw.push(0); // no per-row filter
        for x in 0..width {
            let diagonal = (x * height).abs_diff(y * width) < width * 2;
            if diagonal {
                raw.extend_from_slice(&[20, 20, 30]);
            } else {
                raw.extend_from_slice(&[(x * 255 / width) as u8, (y * 255 / height) as u8, 200]);
            }
        }
    }
    let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit truecolour
    push_chunk(&mut png, b"IHDR", &header);
    push_chunk(&mut png, b"IDAT", &stored_zlib(&raw));
    push_chunk(&mut png, b"IEND", &[]);
    png
}

fn push_chunk(png: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    png.extend_from_slice(&(body.len() as u32).to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(body);
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(body);
    png.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Deflate's "stored" mode: no compression, which needs no library.
fn stored_zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for (index, block) in data.chunks(0xffff).enumerate() {
        let last = (index + 1) * 0xffff >= data.len();
        out.push(u8::from(last));
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
