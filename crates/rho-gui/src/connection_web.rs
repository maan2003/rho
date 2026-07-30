//! Direct browser iroh connection to the daemon's `rho/ui/3` protocol.

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr as _;
use std::sync::Arc;

use futures::StreamExt as _;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use hkdf::Hkdf;
use iroh::EndpointId;
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use rho_registry::session::AgentStreamGenerations;
use rho_ui_proto::{ClientMessage, ServerMessage};
use sha2::{Digest as _, Sha256};
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use zeroize::Zeroize as _;

const CREDENTIAL_KEY: &str = "rho-gui-web-passkey-credential";
const LEGACY_SECRET_KEY: &str = "rho-gui-web-secret";
const DAEMON_KEY: &str = "rho-gui-web-daemon";
const AUTHENTICATOR_KEY: &str = "rho-gui-web-authenticator";
const PRF_LABEL: &[u8] = b"rho webui iroh prf v1";
const HKDF_INFO: &[u8] = b"rho webui iroh ed25519 seed v1";
const MAX_CREDENTIAL_ID_LEN: usize = 1024;

#[derive(Clone, Debug)]
pub enum Event {
    Phase(Phase),
    Message(ServerMessage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Unlock(String),
    Connecting,
    Enroll(String),
    Online,
    Failed(String),
}

pub struct Connection {
    commands: UnboundedSender<ClientMessage>,
    receiver: Rc<RefCell<Option<UnboundedReceiver<ClientMessage>>>>,
    events: UnboundedSender<Event>,
}

impl Connection {
    pub fn new() -> (Self, UnboundedReceiver<Event>) {
        let (commands, receiver) = mpsc::unbounded();
        let (events, event_rx) = mpsc::unbounded();
        (
            Self {
                commands,
                receiver: Rc::new(RefCell::new(Some(receiver))),
                events,
            },
            event_rx,
        )
    }

    pub fn send(&self, message: ClientMessage) {
        let _ = self.commands.unbounded_send(message);
    }

    pub fn connect(&self, daemon: String) {
        let Some(receiver) = self.receiver.borrow_mut().take() else {
            return;
        };
        remember_daemon(&daemon);
        let events = self.events.clone();
        let _ = events.unbounded_send(Event::Phase(Phase::Connecting));
        spawn_local(async move {
            if let Err(error) = run(&daemon, receiver, events.clone()).await {
                let _ = events.unbounded_send(Event::Phase(Phase::Failed(format!("{error:#}"))));
            }
        });
    }
}

pub fn daemon_id_from_page() -> Option<String> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok()??;
    let location = window.location();
    let daemon = [
        location.hash().ok().map(|s| (s, '#')),
        location.search().ok().map(|s| (s, '?')),
    ]
    .into_iter()
    .flatten()
    .find_map(|(part, prefix)| {
        part.trim_start_matches(prefix)
            .split('&')
            .find_map(|pair| pair.strip_prefix("daemon="))
            .filter(|daemon| !daemon.is_empty())
            .map(str::to_owned)
    });
    if let Some(daemon) = daemon {
        let _ = storage.set_item(DAEMON_KEY, &daemon);
        Some(daemon)
    } else {
        storage.get_item(DAEMON_KEY).ok().flatten()
    }
}

fn remember_daemon(daemon: &str) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(DAEMON_KEY, daemon);
    }
}

/// Progress breadcrumbs for the connect path; "Connecting…" is otherwise a
/// black box when any await in here stalls.
fn conn_log(message: &str) {
    web_sys::console::log_1(&JsValue::from_str(&format!("[rho-conn] {message}")));
}

async fn run(
    daemon: &str,
    mut receiver: UnboundedReceiver<ClientMessage>,
    events: UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let daemon = EndpointId::from_str(daemon.trim())
        .map_err(|error| anyhow::anyhow!("invalid daemon endpoint id: {error}"))?;
    conn_log("unlocking passkey identity");
    let secret = passkey_secret(daemon).await?;
    conn_log("passkey identity ready; binding browser iroh endpoint");
    let endpoint = rho_rpc::bind_browser_iroh_client(secret).await?;
    conn_log(&format!(
        "endpoint bound as {}; dialing daemon",
        endpoint.id()
    ));
    let connection = endpoint
        .connect(daemon, rho_ui_proto::IROH_ALPN)
        .await
        .map_err(|error| anyhow::anyhow!("connect to daemon: {error}"))?;
    conn_log("iroh connection established; authenticating");
    match rho_rpc::authenticate_iroh_client(&connection, endpoint.id()).await? {
        rho_iroh_auth::ClientAuthResult::Approved => conn_log("authenticated as enrolled client"),
        rho_iroh_auth::ClientAuthResult::EnrollmentRequired(code) => {
            conn_log(&format!("enrollment required, code {code}"));
            let _ = events.unbounded_send(Event::Phase(Phase::Enroll(code.to_string())));
            return Ok(());
        }
        rho_iroh_auth::ClientAuthResult::Unavailable => {
            anyhow::bail!("daemon cannot accept another enrollment right now")
        }
    }
    let (send, recv) = connection
        .open_bi()
        .await
        .map_err(|error| anyhow::anyhow!("open stream: {error}"))?;
    conn_log("control stream open; subscribing");
    send.set_priority(1)
        .map_err(|error| anyhow::anyhow!("set control stream priority: {error}"))?;
    let mut send = rho_rpc::Writer::new(send);
    let mut recv = rho_rpc::Reader::new(recv);
    rho_ui_proto::write_frame(&mut send, &ClientMessage::Subscribe).await?;
    spawn_local(async move {
        while let Some(message) = receiver.next().await {
            if rho_ui_proto::write_frame(&mut send, &message)
                .await
                .is_err()
            {
                break;
            }
        }
    });
    futures::try_join!(
        read_loop(&events, &mut recv),
        read_agent_streams(events.clone(), connection)
    )?;
    Ok(())
}

async fn read_loop(
    events: &UnboundedSender<Event>,
    recv: &mut rho_rpc::Reader,
) -> anyhow::Result<()> {
    loop {
        let message = rho_ui_proto::read_frame(recv)
            .await
            .map_err(|error| anyhow::anyhow!("daemon connection lost: {error}"))?;
        let _ = events.unbounded_send(Event::Message(message));
    }
}

async fn read_agent_streams(
    events: UnboundedSender<Event>,
    connection: iroh::endpoint::Connection,
) -> anyhow::Result<()> {
    const FRAME_BUDGET: usize = 64 * 1024 * 1024;
    let budget = Arc::new(tokio::sync::Semaphore::new(FRAME_BUDGET));
    let generations = Rc::new(RefCell::new(AgentStreamGenerations::default()));
    loop {
        let recv = connection
            .accept_uni()
            .await
            .map_err(|error| anyhow::anyhow!("accept agent stream: {error}"))?;
        let events = events.clone();
        let budget = Arc::clone(&budget);
        let generations = Rc::clone(&generations);
        spawn_local(async move {
            if let Err(error) = read_agent_stream(events.clone(), recv, budget, generations).await {
                let _ = events.unbounded_send(Event::Phase(Phase::Failed(format!(
                    "agent stream closed: {error:#}"
                ))));
            }
        });
    }
}

async fn read_agent_stream(
    events: UnboundedSender<Event>,
    recv: iroh::endpoint::RecvStream,
    budget: Arc<tokio::sync::Semaphore>,
    generations: Rc<RefCell<AgentStreamGenerations>>,
) -> anyhow::Result<()> {
    let mut recv = rho_rpc::Reader::new(recv);
    let header = read_budgeted(&mut recv, &budget)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent stream closed before its header"))?;
    let ServerMessage::AgentStreamOpened { agent_id } = header else {
        anyhow::bail!("invalid agent stream header")
    };
    let generation = generations.borrow_mut().open(agent_id);
    loop {
        let Some(message) = read_budgeted(&mut recv, &budget).await? else {
            return Ok(());
        };
        let ServerMessage::Agent {
            agent_id: frame_agent_id,
            ..
        } = &message
        else {
            anyhow::bail!("invalid message on agent stream")
        };
        anyhow::ensure!(*frame_agent_id == agent_id, "agent stream id changed");
        if generations.borrow().is_current(agent_id, generation) {
            let _ = events.unbounded_send(Event::Message(message));
        }
    }
}

async fn read_budgeted(
    recv: &mut rho_rpc::Reader,
    budget: &Arc<tokio::sync::Semaphore>,
) -> anyhow::Result<Option<ServerMessage>> {
    let message =
        rho_rpc::read_frame_allocated_optional(recv, rho_ui_proto::MAX_FRAME_LEN, |len| {
            Arc::clone(budget).acquire_many_owned(len as u32)
        })
        .await?;
    Ok(message.map(|(message, _allocation, _)| message))
}

async fn passkey_secret(daemon: EndpointId) -> anyhow::Result<iroh::SecretKey> {
    let storage = local_storage().ok_or_else(|| anyhow::anyhow!("local storage unavailable"))?;
    let credential_id = match storage
        .get_item(CREDENTIAL_KEY)
        .ok()
        .flatten()
        .and_then(|hex| decode_hex_vec(&hex))
        .filter(|id| id.len() <= MAX_CREDENTIAL_ID_LEN)
    {
        Some(id) => {
            conn_log("using stored passkey credential");
            id
        }
        None => {
            let _ = storage.remove_item(CREDENTIAL_KEY);
            conn_log("no stored credential; creating passkey (browser prompt)");
            let id = create_passkey().await?;
            storage
                .set_item(CREDENTIAL_KEY, &encode_hex(&id))
                .map_err(|_| anyhow::anyhow!("store passkey credential id"))?;
            id
        }
    };

    let mut input = Sha256::new();
    input.update(PRF_LABEL);
    input.update(daemon.as_bytes());
    conn_log("evaluating passkey PRF (browser prompt)");
    let mut prf = evaluate_prf(&credential_id, &input.finalize()).await?;
    let hkdf = Hkdf::<Sha256>::new(Some(daemon.as_bytes()), &prf);
    let mut seed = [0u8; 32];
    hkdf.expand(HKDF_INFO, &mut seed)
        .map_err(|_| anyhow::anyhow!("derive iroh key from passkey PRF"))?;
    let secret = iroh::SecretKey::from_bytes(&seed);
    prf.zeroize();
    seed.zeroize();
    // Do not leave identities created by older builds readable by page script.
    let _ = storage.remove_item(LEGACY_SECRET_KEY);
    Ok(secret)
}

async fn create_passkey() -> anyhow::Result<Vec<u8>> {
    let challenge = random_bytes(32)?;
    let user_id = random_bytes(32)?;
    let public_key = Object::new();
    set(
        &public_key,
        "challenge",
        &Uint8Array::from(challenge.as_slice()),
    )?;

    let rp = Object::new();
    set(&rp, "name", &JsValue::from_str("Rho Web UI"))?;
    set(&public_key, "rp", &rp)?;

    let user = Object::new();
    set(&user, "id", &Uint8Array::from(user_id.as_slice()))?;
    set(&user, "name", &JsValue::from_str("rho-webui"))?;
    set(&user, "displayName", &JsValue::from_str("Rho Web UI"))?;
    set(&public_key, "user", &user)?;

    let parameter = Object::new();
    set(&parameter, "type", &JsValue::from_str("public-key"))?;
    set(&parameter, "alg", &JsValue::from_f64(-7.0))?;
    let parameters = Array::new();
    parameters.push(&parameter);
    set(&public_key, "pubKeyCredParams", &parameters)?;
    set(&public_key, "attestation", &JsValue::from_str("none"))?;

    let selection = Object::new();
    set(
        &selection,
        "userVerification",
        &JsValue::from_str("required"),
    )?;
    set(&selection, "residentKey", &JsValue::from_str("preferred"))?;
    if let Some(attachment) = local_storage().and_then(|s| s.get_item(AUTHENTICATOR_KEY).ok()?) {
        set(
            &selection,
            "authenticatorAttachment",
            &JsValue::from_str(&attachment),
        )?;
    }
    set(&public_key, "authenticatorSelection", &selection)?;
    let extensions = Object::new();
    set(&extensions, "prf", &Object::new())?;
    set(&public_key, "extensions", &extensions)?;

    let options = Object::new();
    set(&options, "publicKey", &public_key)?;
    let credential = credentials_call("create", &options).await?;
    extension_prf_enabled(&credential)?;
    let raw_id = Reflect::get(&credential, &JsValue::from_str("rawId"))
        .map_err(|_| anyhow::anyhow!("passkey response has no credential id"))?;
    Ok(Uint8Array::new(&raw_id).to_vec())
}

async fn evaluate_prf(credential_id: &[u8], input: &[u8]) -> anyhow::Result<[u8; 32]> {
    let public_key = Object::new();
    set(
        &public_key,
        "challenge",
        &Uint8Array::from(random_bytes(32)?.as_slice()),
    )?;
    set(
        &public_key,
        "userVerification",
        &JsValue::from_str("required"),
    )?;
    let descriptor = Object::new();
    set(&descriptor, "type", &JsValue::from_str("public-key"))?;
    set(&descriptor, "id", &Uint8Array::from(credential_id))?;
    let allowed = Array::new();
    allowed.push(&descriptor);
    set(&public_key, "allowCredentials", &allowed)?;

    let eval = Object::new();
    set(&eval, "first", &Uint8Array::from(input))?;
    let prf = Object::new();
    set(&prf, "eval", &eval)?;
    let extensions = Object::new();
    set(&extensions, "prf", &prf)?;
    set(&public_key, "extensions", &extensions)?;
    let options = Object::new();
    set(&options, "publicKey", &public_key)?;

    let credential = credentials_call("get", &options).await?;
    let results = extension_results(&credential)?;
    let prf = Reflect::get(&results, &JsValue::from_str("prf"))
        .map_err(|_| anyhow::anyhow!("passkey did not return PRF results"))?;
    let results = Reflect::get(&prf, &JsValue::from_str("results"))
        .map_err(|_| anyhow::anyhow!("passkey did not evaluate its PRF"))?;
    let first = Reflect::get(&results, &JsValue::from_str("first"))
        .map_err(|_| anyhow::anyhow!("passkey did not return the requested PRF output"))?;
    let output = Uint8Array::new(&first).to_vec();
    output
        .try_into()
        .map_err(|_| anyhow::anyhow!("passkey PRF output is not 32 bytes"))
}

async fn credentials_call(method: &str, options: &Object) -> anyhow::Result<JsValue> {
    let navigator = web_sys::window()
        .ok_or_else(|| anyhow::anyhow!("browser window unavailable"))?
        .navigator();
    let credentials = Reflect::get(navigator.as_ref(), &JsValue::from_str("credentials"))
        .map_err(|_| anyhow::anyhow!("WebAuthn is unavailable"))?;
    let function: Function = Reflect::get(&credentials, &JsValue::from_str(method))
        .map_err(|_| anyhow::anyhow!("WebAuthn credentials.{method} is unavailable"))?
        .dyn_into()
        .map_err(|_| anyhow::anyhow!("WebAuthn credentials.{method} is unavailable"))?;
    let promise: Promise = function
        .call1(&credentials, options)
        .map_err(|error| anyhow::anyhow!("WebAuthn {method} failed: {error:?}"))?
        .dyn_into()
        .map_err(|_| anyhow::anyhow!("WebAuthn credentials.{method} returned no promise"))?;
    JsFuture::from(promise)
        .await
        .map_err(|error| anyhow::anyhow!("WebAuthn {method} failed: {error:?}"))
}

fn extension_results(credential: &JsValue) -> anyhow::Result<JsValue> {
    let function: Function =
        Reflect::get(credential, &JsValue::from_str("getClientExtensionResults"))
            .map_err(|_| anyhow::anyhow!("passkey extension results unavailable"))?
            .dyn_into()
            .map_err(|_| anyhow::anyhow!("passkey extension results unavailable"))?;
    function
        .call0(credential)
        .map_err(|_| anyhow::anyhow!("read passkey extension results"))
}

fn extension_prf_enabled(credential: &JsValue) -> anyhow::Result<()> {
    let results = extension_results(credential)?;
    let prf = Reflect::get(&results, &JsValue::from_str("prf"))
        .map_err(|_| anyhow::anyhow!("this browser or passkey does not support WebAuthn PRF"))?;
    let enabled = Reflect::get(&prf, &JsValue::from_str("enabled"))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    anyhow::ensure!(
        enabled,
        "this browser or passkey does not support WebAuthn PRF"
    );
    Ok(())
}

fn random_bytes(len: usize) -> anyhow::Result<Vec<u8>> {
    // Not `crypto.getRandomValues` on the wasm memory directly: GPUI-web
    // builds with threads, so wasm memory is a SharedArrayBuffer, and the
    // WebCrypto spec rejects views of shared buffers. The getrandom crate
    // copies through a non-shared scratch buffer.
    let mut bytes = vec![0u8; len];
    getrandom_02::getrandom(&mut bytes)
        .map_err(|_| anyhow::anyhow!("browser random number generation failed"))?;
    Ok(bytes)
}

fn set(target: &Object, name: &str, value: &JsValue) -> anyhow::Result<()> {
    Reflect::set(target, &JsValue::from_str(name), value)
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("build WebAuthn options"))
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex_vec(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    for chunk in text.as_bytes().chunks(2) {
        let chunk = std::str::from_utf8(chunk).ok()?;
        bytes.push(u8::from_str_radix(chunk, 16).ok()?);
    }
    Some(bytes)
}
