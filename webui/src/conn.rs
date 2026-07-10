//! Iroh connection to the daemon: newline-delimited JSON on the
//! [`rho_webui_messages::ALPN`] bi-stream, enrollment code display while the
//! daemon waits for `rho iroh approve`.

use std::cell::RefCell;
use std::str::FromStr as _;

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use iroh::EndpointId;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rho_iroh_auth::EnrollmentCodeExt as _;
use rho_webui_messages::{FromBrowser, MAX_LINE_LEN, ToBrowser};

use crate::{App, Phase};

const SECRET_KEY: &str = "rho-webui-secret";
const DAEMON_KEY: &str = "rho-webui-daemon";

thread_local! {
    /// Held until a daemon id is known (typed in or from the URL).
    static PENDING_RECEIVER: RefCell<Option<UnboundedReceiver<FromBrowser>>> =
        const { RefCell::new(None) };
}

/// Start connecting if the page already knows a daemon id, otherwise wait
/// for [`set_daemon`].
pub fn init(app: App, receiver: UnboundedReceiver<FromBrowser>) {
    PENDING_RECEIVER.with(|cell| *cell.borrow_mut() = Some(receiver));
    if let Some(daemon) = daemon_id_from_page() {
        start(app, daemon);
    }
}

/// Daemon endpoint id from `?daemon=` (also remembered) or local storage.
fn daemon_id_from_page() -> Option<String> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok()??;
    if let Ok(query) = window.location().search()
        && let Some(daemon) = query
            .trim_start_matches('?')
            .split('&')
            .find_map(|pair| pair.strip_prefix("daemon="))
        && !daemon.is_empty()
    {
        let _ = storage.set_item(DAEMON_KEY, daemon);
        return Some(daemon.to_owned());
    }
    storage.get_item(DAEMON_KEY).ok()?
}

/// Remembered daemon endpoint id, for display.
pub fn daemon_id() -> Option<String> {
    local_storage()?.get_item(DAEMON_KEY).ok()?
}

/// Called from the connect screen; remembers the id and starts connecting.
pub fn set_daemon(app: App, daemon: String) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(DAEMON_KEY, &daemon);
    }
    start(app, daemon);
}

fn start(app: App, daemon: String) {
    let Some(receiver) = PENDING_RECEIVER.with(|cell| cell.borrow_mut().take()) else {
        return;
    };
    app.phase.set(Phase::Connecting);
    spawn_local(async move {
        if let Err(error) = run(app, &daemon, receiver).await {
            app.phase.set(Phase::Failed(format!("{error:#}")));
        }
    });
}

async fn run(
    app: App,
    daemon: &str,
    mut receiver: UnboundedReceiver<FromBrowser>,
) -> anyhow::Result<()> {
    let daemon = EndpointId::from_str(daemon.trim())
        .map_err(|error| anyhow::anyhow!("invalid daemon endpoint id: {error}"))?;
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(load_or_create_secret()?)
        .bind()
        .await
        .map_err(|error| anyhow::anyhow!("bind iroh endpoint: {error}"))?;
    let connection = endpoint
        .connect(daemon, rho_webui_messages::ALPN)
        .await
        .map_err(|error| anyhow::anyhow!("connect to daemon: {error}"))?;

    // Until the daemon trusts this browser's key, its auth hook holds the
    // connection while `rho iroh approve <code>` is pending. Show the code;
    // the first Hello replaces this screen. An empty line materializes the
    // QUIC stream so the daemon sees the connection.
    let code = connection.enrollment_code(endpoint.id());
    app.phase.set(Phase::Enroll(code.to_string()));
    let (mut send, recv) = connection
        .open_bi()
        .await
        .map_err(|error| anyhow::anyhow!("open stream: {error}"))?;
    send.write_all(b"\n")
        .await
        .map_err(|error| anyhow::anyhow!("start stream: {error}"))?;

    spawn_local(async move {
        while let Some(message) = receiver.next().await {
            let mut line = match serde_json::to_string(&message) {
                Ok(line) => line,
                Err(_) => continue,
            };
            line.push('\n');
            if send.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    read_loop(app, recv).await
}

async fn read_loop(app: App, mut recv: iroh::endpoint::RecvStream) -> anyhow::Result<()> {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = recv
            .read(&mut buf)
            .await
            .map_err(|error| anyhow::anyhow!("daemon connection lost: {error}"))?;
        let Some(read) = read else {
            anyhow::bail!("daemon closed the connection");
        };
        pending.extend_from_slice(&buf[..read]);
        anyhow::ensure!(pending.len() <= MAX_LINE_LEN, "daemon message too long");
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=newline).collect();
            let line = std::str::from_utf8(&line[..newline])?;
            if line.trim().is_empty() {
                continue;
            }
            let message: ToBrowser = serde_json::from_str(line)
                .map_err(|error| anyhow::anyhow!("bad daemon message: {error}"))?;
            handle(app, message);
        }
    }
}

fn handle(app: App, message: ToBrowser) {
    match message {
        ToBrowser::Hello { topics, workdirs } => {
            app.topics.set(topics);
            app.workdirs.set(workdirs);
            if app.phase.get_untracked() != Phase::Online {
                app.phase.set(Phase::Online);
            }
        }
        ToBrowser::Agent { agent_id, state } => {
            if app.selected.get_untracked().as_deref() == Some(agent_id.as_str()) {
                app.state.set(Some(state));
            }
        }
        ToBrowser::AgentCreated { agent_id } => {
            app.select(agent_id);
        }
        ToBrowser::Error { message } => {
            app.show_toast(message);
        }
    }
}

/// This browser's iroh identity, persisted so enrollment survives reloads.
fn load_or_create_secret() -> anyhow::Result<iroh::SecretKey> {
    let storage = local_storage().ok_or_else(|| anyhow::anyhow!("local storage unavailable"))?;
    if let Ok(Some(hex)) = storage.get_item(SECRET_KEY)
        && let Some(bytes) = decode_hex(&hex)
    {
        return Ok(iroh::SecretKey::from_bytes(&bytes));
    }
    let secret = iroh::SecretKey::generate();
    let _ = storage.set_item(SECRET_KEY, &encode_hex(&secret.to_bytes()));
    Ok(secret)
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let chunk = std::str::from_utf8(chunk).ok()?;
        bytes[index] = u8::from_str_radix(chunk, 16).ok()?;
    }
    Some(bytes)
}
