//! Iroh connection to the daemon: newline-delimited JSON on the
//! [`rho_webui_messages::ALPN`] bi-stream, enrollment code display while the
//! daemon waits for `rho iroh approve`.

use std::cell::RefCell;
use std::str::FromStr as _;

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use hkdf::Hkdf;
use iroh::EndpointId;
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use leptos::prelude::*;
use leptos::task::spawn_local;
use rho_iroh_auth::EnrollmentCodeExt as _;
use rho_webui_messages::{FromBrowser, MAX_LINE_LEN, ToBrowser};
use sha2::{Digest as _, Sha256};
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;
use zeroize::Zeroize as _;

use crate::{App, Phase};

const CREDENTIAL_KEY: &str = "rho-webui-passkey-credential";
const LEGACY_SECRET_KEY: &str = "rho-webui-secret";
const DAEMON_KEY: &str = "rho-webui-daemon";
const PRF_LABEL: &[u8] = b"rho webui iroh prf v1";
const HKDF_INFO: &[u8] = b"rho webui iroh ed25519 seed v1";

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
        .secret_key(passkey_secret(daemon).await?)
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
            app.show_new_agent.set(false);
            app.select(agent_id);
        }
        ToBrowser::Error { message } => {
            app.show_toast(message);
        }
    }
}

/// Derive this browser's stable, per-daemon iroh identity from a passkey PRF.
/// Only the non-secret credential id is persisted; the derived seed lives in
/// memory for the lifetime of the iroh endpoint.
async fn passkey_secret(daemon: EndpointId) -> anyhow::Result<iroh::SecretKey> {
    let storage = local_storage().ok_or_else(|| anyhow::anyhow!("local storage unavailable"))?;
    let credential_id = match storage.get_item(CREDENTIAL_KEY) {
        Ok(Some(hex)) => decode_hex_vec(&hex)
            .ok_or_else(|| anyhow::anyhow!("stored passkey credential id is invalid"))?,
        _ => {
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
    let crypto = web_sys::window()
        .ok_or_else(|| anyhow::anyhow!("browser window unavailable"))?
        .crypto()
        .map_err(|_| anyhow::anyhow!("browser cryptography unavailable"))?;
    let mut bytes = vec![0u8; len];
    crypto
        .get_random_values_with_u8_array(&mut bytes)
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
