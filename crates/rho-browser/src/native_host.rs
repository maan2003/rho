use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{fs, thread};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

pub const EXTENSION_ID: &str = "jampgbfcmidekmapffhhlcfjaflcdpho";
pub const EXTENSION_ORIGIN: &str = "chrome-extension://jampgbfcmidekmapffhhlcfjaflcdpho/";
pub const HOST_NAME: &str = "dev.rho.browser";
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

pub const SOCKET_PATH_ENV: &str = "RHO_BROWSER_SOCKET_PATH";

pub fn is_invocation(arguments: &[String]) -> bool {
    arguments
        .get(1)
        .is_some_and(|argument| argument == EXTENSION_ORIGIN)
}

/// Runs in the Chrome-spawned copy of `rho-gui`. Chrome speaks its native
/// messaging framing on stdio; the host relays the same bounded frames over
/// Rho's private Unix socket.
pub fn run() -> Result<()> {
    let socket_path = std::env::var_os(SOCKET_PATH_ENV)
        .map(PathBuf::from)
        .context("RHO_BROWSER_SOCKET_PATH is not set")?;
    let mut socket = UnixStream::connect(socket_path).context("connect Rho browser socket")?;
    let mut to_socket = socket.try_clone()?;
    let _input = thread::spawn(move || -> Result<()> {
        let mut stdin = io::stdin().lock();
        while let Some(frame) = read_frame(&mut stdin)? {
            write_frame(&mut to_socket, &frame)?;
        }
        let _ = to_socket.shutdown(std::net::Shutdown::Write);
        Ok(())
    });

    let mut stdout = io::stdout().lock();
    while let Some(frame) = read_frame(&mut socket)? {
        write_frame(&mut stdout, &frame)?;
        stdout.flush()?;
    }
    Ok(())
}

pub struct Bridge {
    inner: Arc<BridgeInner>,
    socket_path: PathBuf,
    thread: Option<thread::JoinHandle<()>>,
}

struct BridgeInner {
    connection: Mutex<Option<Arc<Connection>>>,
    connected: Condvar,
    next_request: AtomicU64,
    stop: AtomicBool,
}

struct Connection {
    writer: Mutex<UnixStream>,
    pending: Mutex<PendingRequests>,
}

type PendingRequests = HashMap<u64, mpsc::Sender<Result<(Value, Option<u32>), String>>>;

impl Bridge {
    pub fn bind(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove stale browser socket"),
        }
        let listener = UnixListener::bind(&path).context("bind Rho browser socket")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let inner = Arc::new(BridgeInner {
            connection: Mutex::new(None),
            connected: Condvar::new(),
            next_request: AtomicU64::new(1),
            stop: AtomicBool::new(false),
        });
        let server = inner.clone();
        let thread = thread::Builder::new()
            .name("rho-browser-extension".into())
            .spawn(move || accept_connections(listener, server))?;
        Ok(Self {
            inner,
            socket_path: path,
            thread: Some(thread),
        })
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let connection = {
            let mut current = self.inner.connection.lock().unwrap();
            loop {
                if let Some(connection) = current.clone() {
                    break connection;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    bail!(extension_setup_error())
                }
                let (next, timeout) = self
                    .inner
                    .connected
                    .wait_timeout(current, remaining)
                    .unwrap();
                current = next;
                if timeout.timed_out() && current.is_none() {
                    bail!(extension_setup_error())
                }
            }
        };

        let id = self.inner.next_request.fetch_add(1, Ordering::Relaxed);
        let (reply, receive) = mpsc::channel();
        connection.pending.lock().unwrap().insert(id, reply);
        let message = serde_json::to_vec(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))?;
        let sent_at = Instant::now();
        if let Err(error) = write_frame(&mut *connection.writer.lock().unwrap(), &message) {
            connection.pending.lock().unwrap().remove(&id);
            return Err(error).context("send browser extension request");
        }
        match receive.recv_timeout(RESPONSE_TIMEOUT) {
            Ok(Ok((value, handler_us))) => {
                record_extension_command(method, sent_at, handler_us, true);
                Ok(value)
            }
            Ok(Err(error)) => {
                record_extension_command(method, sent_at, None, false);
                bail!("browser extension: {error}")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                connection.pending.lock().unwrap().remove(&id);
                record_extension_command(method, sent_at, None, false);
                bail!("browser extension request timed out: {method}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                record_extension_command(method, sent_at, None, false);
                bail!("browser extension disconnected during {method}")
            }
        }
    }
}

fn extension_setup_error() -> String {
    let extension = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|state| state.join("rho/chromium-extension"));
    match extension {
        Some(extension) => format!(
            "Rho component extension did not connect from {}; verify the Nix-built Brave wrapper",
            extension.display()
        ),
        None => {
            "Rho component extension did not connect; verify the Nix-built Brave wrapper".to_owned()
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(connection) = self.inner.connection.lock().unwrap().take() {
            let _ = connection
                .writer
                .lock()
                .unwrap()
                .shutdown(std::net::Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn accept_connections(listener: UnixListener, inner: Arc<BridgeInner>) {
    while !inner.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => install_connection(stream, &inner),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

fn install_connection(stream: UnixStream, inner: &Arc<BridgeInner>) {
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let connection = Arc::new(Connection {
        writer: Mutex::new(stream),
        pending: Mutex::new(HashMap::new()),
    });
    if let Some(old) = inner.connection.lock().unwrap().replace(connection.clone()) {
        fail_pending(&old, "browser extension reconnected");
        let _ = old
            .writer
            .lock()
            .unwrap()
            .shutdown(std::net::Shutdown::Both);
    }
    inner.connected.notify_all();
    record_extension_command("__connect", Instant::now(), None, true);
    tracing::info!("browser extension connected");
    let weak = Arc::downgrade(inner);
    thread::spawn(move || read_responses(reader, connection, weak));
}

fn read_responses(
    mut reader: UnixStream,
    connection: Arc<Connection>,
    inner: std::sync::Weak<BridgeInner>,
) {
    while let Ok(Some(frame)) = read_frame(&mut reader) {
        let Ok(message) = serde_json::from_slice::<Value>(&frame) else {
            continue;
        };
        if record_page_metadata(&message) {
            continue;
        }
        if log_tab_state(&message) {
            continue;
        }
        if record_frame_telemetry(&message) {
            continue;
        }
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let Some(reply) = connection.pending.lock().unwrap().remove(&id) else {
            continue;
        };
        let result = if message.get("ok").and_then(Value::as_bool) == Some(true) {
            let handler_us = message
                .get("handler_us")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            Ok((
                message.get("result").cloned().unwrap_or(Value::Null),
                handler_us,
            ))
        } else {
            Err(message
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown extension error")
                .to_owned())
        };
        let _ = reply.send(result);
    }
    fail_pending(&connection, "browser extension disconnected");
    record_extension_command("__disconnect", Instant::now(), None, false);
    tracing::warn!("browser extension disconnected");
    if let Some(inner) = inner.upgrade() {
        let mut current = inner.connection.lock().unwrap();
        if current
            .as_ref()
            .is_some_and(|candidate| Arc::ptr_eq(candidate, &connection))
        {
            *current = None;
        }
    }
}

/// Per-request timing for the native-messaging bridge. round_trip includes
/// MV3 service-worker wakeup; handler_us is the worker-measured execution
/// time, so the difference localizes transport plus cold-start latency.
#[derive(Clone, Debug)]
pub struct ExtensionCommandStats {
    pub method: String,
    pub at: Instant,
    pub round_trip_us: u32,
    pub handler_us: Option<u32>,
    pub ok: bool,
}

const MAX_EXTENSION_COMMAND_STATS: usize = 1024;
static EXTENSION_COMMAND_STATS: Mutex<VecDeque<ExtensionCommandStats>> =
    Mutex::new(VecDeque::new());

fn record_extension_command(method: &str, at: Instant, handler_us: Option<u32>, ok: bool) {
    let round_trip_us = at.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
    let mut ring = EXTENSION_COMMAND_STATS.lock().unwrap();
    if ring.len() >= MAX_EXTENSION_COMMAND_STATS {
        ring.pop_front();
    }
    ring.push_back(ExtensionCommandStats {
        method: method.to_owned(),
        at,
        round_trip_us,
        handler_us,
        ok,
    });
}

/// Returns a non-destructive copy of the bounded bridge command-stats ring.
pub fn snapshot_extension_command_stats() -> Vec<ExtensionCommandStats> {
    EXTENSION_COMMAND_STATS
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageMetadata {
    pub title: String,
    pub url: String,
    /// The page this tab was opened from, when the reader opened it from
    /// one: a ctrl-click, or a link that asked for a new tab.
    pub opened_from: Option<String>,
}

static PAGE_METADATA: LazyLock<Mutex<HashMap<String, PageMetadata>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PAGE_METADATA_REVISION: AtomicU64 = AtomicU64::new(0);

pub fn page_metadata(page_id: &str) -> Option<PageMetadata> {
    PAGE_METADATA.lock().unwrap().get(page_id).cloned()
}

/// Every page the browser has told us about, which is every tab it has
/// open. Nothing here is stored: the map holds it only while the browser
/// is running.
pub fn page_metadata_entries() -> Vec<(String, PageMetadata)> {
    PAGE_METADATA
        .lock()
        .unwrap()
        .iter()
        .map(|(id, metadata)| (id.clone(), metadata.clone()))
        .collect()
}

pub fn page_metadata_revision() -> u64 {
    PAGE_METADATA_REVISION.load(Ordering::Acquire)
}

/// Takes one message from the extension. Public because it is the seam
/// the browser talks through: a test drives the browser side by sending
/// the events the extension really sends, rather than reaching past it.
pub fn record_page_metadata(message: &Value) -> bool {
    if message.get("event").and_then(Value::as_str) == Some("page-metadata-removed") {
        if let Some(page_id) = message
            .get("page_id")
            .and_then(Value::as_str)
            .filter(|value| value.len() <= 64)
        {
            PAGE_METADATA.lock().unwrap().remove(page_id);
            PAGE_METADATA_REVISION.fetch_add(1, Ordering::Release);
        }
        return true;
    }
    if message.get("event").and_then(Value::as_str) != Some("page-metadata") {
        return false;
    }
    let bounded = |field: &str, max: usize| {
        message
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| value.len() <= max)
            .unwrap_or("")
            .to_owned()
    };
    let page_id = bounded("page_id", 64);
    if page_id.is_empty() {
        return true;
    }
    PAGE_METADATA.lock().unwrap().insert(
        page_id,
        PageMetadata {
            title: bounded("title", 1024),
            url: bounded("url", 4096),
            opened_from: Some(bounded("opened_from", 64)).filter(|id| !id.is_empty()),
        },
    );
    PAGE_METADATA_REVISION.fetch_add(1, Ordering::Release);
    true
}

/// Tab lifecycle report from the extension: activation, page visibility as
/// the renderer sees it, discard/freeze state. The direct signal for "the
/// tab never actually became visible" handoff stalls.
#[derive(Clone, Debug)]
pub struct TabStateEvent {
    pub at: Instant,
    pub state: String,
    pub reason: String,
    pub page_id: String,
    pub tab_id: i64,
    pub active: Option<bool>,
    pub discarded: Option<bool>,
    pub frozen: Option<bool>,
    pub status: String,
}

const MAX_TAB_STATE_EVENTS: usize = 512;
static TAB_STATE_EVENTS: Mutex<VecDeque<TabStateEvent>> = Mutex::new(VecDeque::new());

/// Returns a non-destructive copy of the bounded tab-state event ring.
pub fn snapshot_tab_state_events() -> Vec<TabStateEvent> {
    TAB_STATE_EVENTS.lock().unwrap().iter().cloned().collect()
}

fn record_frame_telemetry(message: &Value) -> bool {
    if message.get("event").and_then(Value::as_str) != Some("frame-telemetry") {
        return false;
    }
    let number = |field: &str| {
        message
            .get(field)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0)
    };
    rho_browser_wayland::record_extension_frame_stats(rho_browser_wayland::ExtensionFrameStats {
        tab_id: message.get("tab_id").and_then(Value::as_i64).unwrap_or(0),
        at: Instant::now(),
        frames: number("frames"),
        window_ms: number("window_ms"),
        mean_interval_us: number("mean_interval_us"),
        p95_interval_us: number("p95_interval_us"),
        max_interval_us: number("max_interval_us"),
        long_frames: number("long_frames"),
    });
    true
}

fn log_tab_state(message: &Value) -> bool {
    if message.get("event").and_then(Value::as_str) != Some("tab-state") {
        return false;
    }
    let text = |field: &str, max: usize| {
        message
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| value.len() <= max)
            .unwrap_or("")
    };
    tracing::info!(
        state = text("state", 32),
        reason = text("reason", 64),
        page_id = text("page_id", 64),
        tab_id = message.get("tab_id").and_then(|value| value.as_i64()),
        active = message.get("active").and_then(|value| value.as_bool()),
        audible = message.get("audible").and_then(|value| value.as_bool()),
        auto_discardable = message
            .get("auto_discardable")
            .and_then(|value| value.as_bool()),
        discarded = message.get("discarded").and_then(|value| value.as_bool()),
        frozen = message.get("frozen").and_then(|value| value.as_bool()),
        status = text("status", 16),
        "browser extension tab lifecycle"
    );
    let mut ring = TAB_STATE_EVENTS.lock().unwrap();
    if ring.len() >= MAX_TAB_STATE_EVENTS {
        ring.pop_front();
    }
    ring.push_back(TabStateEvent {
        at: Instant::now(),
        state: text("state", 32).to_owned(),
        reason: text("reason", 64).to_owned(),
        page_id: text("page_id", 64).to_owned(),
        tab_id: message.get("tab_id").and_then(Value::as_i64).unwrap_or(0),
        active: message.get("active").and_then(Value::as_bool),
        discarded: message.get("discarded").and_then(Value::as_bool),
        frozen: message.get("frozen").and_then(Value::as_bool),
        status: text("status", 16).to_owned(),
    });
    true
}

fn fail_pending(connection: &Connection, error: &str) {
    for (_, reply) in connection.pending.lock().unwrap().drain() {
        let _ = reply.send(Err(error.to_owned()));
    }
}

fn read_frame(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut header = [0_u8; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_MESSAGE_BYTES {
        bail!("browser native message exceeds {MAX_MESSAGE_BYTES} bytes")
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_frame(writer: &mut impl Write, body: &[u8]) -> Result<()> {
    if body.len() > MAX_MESSAGE_BYTES {
        bail!("browser native message exceeds {MAX_MESSAGE_BYTES} bytes")
    }
    writer.write_all(&(body.len() as u32).to_le_bytes())?;
    writer.write_all(body)?;
    writer.flush()?;
    Ok(())
}

pub fn write_installation(brave_config: &Path, extension: &Path, executable: &Path) -> Result<()> {
    fs::create_dir_all(extension)?;
    fs::write(
        extension.join("manifest.json"),
        include_str!("../extension/manifest.json"),
    )?;
    fs::write(
        extension.join("service-worker.js"),
        include_str!("../extension/service-worker.js"),
    )?;
    fs::write(
        extension.join("content-script.js"),
        include_str!("../extension/content-script.js"),
    )?;
    fs::write(
        extension.join("VIMFX-LICENSE-MIT"),
        include_str!("../extension/VIMFX-LICENSE-MIT"),
    )?;
    fs::write(
        extension.join("parking.html"),
        include_str!("../extension/parking.html"),
    )?;
    let manifest = serde_json::to_vec_pretty(&json!({
        "name": HOST_NAME,
        "description": "Rho browser Unix-socket bridge",
        "path": executable,
        "type": "stdio",
        "allowed_origins": [EXTENSION_ORIGIN],
    }))?;
    let hosts = brave_config.join("BraveSoftware/Brave-Origin/NativeMessagingHosts");
    fs::create_dir_all(&hosts)?;
    fs::write(hosts.join(format!("{HOST_NAME}.json")), &manifest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use super::*;

    #[test]
    fn native_frames_are_bounded_and_round_trip() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, br#"{"ok":true}"#).unwrap();
        let mut input = bytes.as_slice();
        assert_eq!(
            read_frame(&mut input).unwrap(),
            Some(br#"{"ok":true}"#.to_vec())
        );
        assert_eq!(read_frame(&mut input).unwrap(), None);
    }

    #[test]
    fn only_the_fixed_extension_origin_selects_native_host_mode() {
        assert!(is_invocation(&["rho-gui".into(), EXTENSION_ORIGIN.into()]));
        assert!(!is_invocation(&["rho-gui".into()]));
        assert!(!is_invocation(&[
            "rho-gui".into(),
            "chrome-extension://other/".into()
        ]));
    }

    #[test]
    fn page_metadata_events_are_bounded_and_replace_by_page() {
        assert!(record_page_metadata(&json!({
            "event": "page-metadata",
            "page_id": "web-a",
            "title": "First",
            "url": "https://example.com/one",
        })));
        assert!(record_page_metadata(&json!({
            "event": "page-metadata",
            "page_id": "web-a",
            "title": "Second",
            "url": "https://example.com/two",
        })));
        assert_eq!(
            page_metadata("web-a"),
            Some(PageMetadata {
                title: "Second".into(),
                url: "https://example.com/two".into(),
                opened_from: None,
            })
        );
        // A tab the reader opened from another one says so, and the
        // origin survives the next metadata event for the same page.
        assert!(record_page_metadata(&json!({
            "event": "page-metadata",
            "page_id": "web-b",
            "title": "Opened from a link",
            "url": "https://example.com/three",
            "opened_from": "web-a",
        })));
        assert_eq!(
            page_metadata("web-b").unwrap().opened_from.as_deref(),
            Some("web-a")
        );
        assert!(record_page_metadata(&json!({
            "event": "page-metadata",
            "page_id": "web-too-long",
            "title": "x".repeat(1025),
            "url": "https://example.com",
        })));
        assert_eq!(page_metadata("web-too-long").unwrap().title, "");
        assert!(record_page_metadata(&json!({
            "event": "page-metadata-removed",
            "page_id": "web-a",
        })));
        assert_eq!(page_metadata("web-a"), None);
    }

    #[test]
    fn recognizes_only_url_free_tab_lifecycle_diagnostics() {
        assert!(log_tab_state(&json!({
            "event": "tab-state",
            "state": "updated",
            "page_id": "00000000-0000-0000-0000-000000000000",
            "tab_id": 7,
            "discarded": true,
        })));
        assert!(!log_tab_state(&json!({ "id": 7, "ok": true })));
    }

    #[test]
    fn installation_uses_the_fixed_extension_identity() {
        let temp = tempfile::tempdir().unwrap();
        let brave_config = temp.path().join("brave-config");
        let extension = temp.path().join("extension");
        let executable = temp.path().join("rho-gui");
        write_installation(&brave_config, &extension, &executable).unwrap();
        let host: Value = serde_json::from_slice(
            &fs::read(
                brave_config
                    .join("BraveSoftware/Brave-Origin/NativeMessagingHosts/dev.rho.browser.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(host["allowed_origins"][0], EXTENSION_ORIGIN);
        assert_eq!(host["path"], executable.to_string_lossy().as_ref());
        let manifest: Value =
            serde_json::from_slice(&fs::read(extension.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["manifest_version"], 3);
        assert_eq!(manifest["content_scripts"][0]["run_at"], "document_start");
        assert_eq!(manifest["content_scripts"][0]["all_frames"], true);
        assert!(
            manifest["permissions"]
                .as_array()
                .unwrap()
                .contains(&Value::String("clipboardWrite".into()))
        );
        let worker = fs::read_to_string(extension.join("service-worker.js")).unwrap();
        assert!(worker.contains("chrome.rhoPrivate"));
        assert!(worker.contains("reload-all-force"));
        assert!(!worker.contains("chrome.tabGroups"));
        assert!(!worker.contains("fallbackTabKey"));
        assert!(!worker.contains("onCommand"));
        assert!(!worker.contains("completeHints"));
        let content = fs::read_to_string(extension.join("content-script.js")).unwrap();
        assert!(content.contains("addEventListener(\"keydown\", onKeyDown, true)"));
        assert!(content.contains("const INPUT_TIMEOUT_MS = 2000"));
        assert!(content.contains("class ScrollableElements"));
        assert!(content.contains("function scrollTarget()"));
        assert!(content.contains("const behavior = \"smooth\""));
        assert!(content.contains("enteredText"));
        assert!(content.contains("VimFx parity TODO(rhoPrivate.vim.caret)"));
        assert!(!content.contains("function handleCaretKey(event)"));
        assert!(content.contains("VimFx parity TODO(rhoPrivate.vim.find)"));
        assert!(!content.contains("function updateFindMatches()"));
        assert!(content.contains("function toggleComplementaryHints()"));
        assert!(content.contains("function jumpToScrollMark(key)"));
        assert!(content.contains("VIMFX PARITY TODO(rhoPrivate.vim)"));
        assert!(!content.contains("__rhoHandleCommand"));
        assert!(!content.contains("__rhoComponentController"));
        assert!(extension.join("VIMFX-LICENSE-MIT").is_file());
    }

    #[test]
    fn bridge_uses_a_private_unix_socket_for_requests() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("browser.sock");
        let bridge = Bridge::bind(path.clone()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);

        let client = thread::spawn(move || {
            let mut stream = UnixStream::connect(path).unwrap();
            let request: Value =
                serde_json::from_slice(&read_frame(&mut stream).unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "focus");
            assert_eq!(request["params"]["id"], "page-id");
            let response = serde_json::to_vec(&json!({
                "id": request["id"],
                "ok": true,
                "result": { "focused": true },
            }))
            .unwrap();
            write_frame(&mut stream, &response).unwrap();
        });
        assert_eq!(
            bridge.request("focus", json!({ "id": "page-id" })).unwrap()["focused"],
            true
        );
        client.join().unwrap();
    }
}
