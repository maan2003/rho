use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
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

pub fn socket_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is not available for the browser socket")?;
    Ok(runtime.join("rho-browser.sock"))
}

pub fn is_invocation(arguments: &[String]) -> bool {
    arguments
        .get(1)
        .is_some_and(|argument| argument == EXTENSION_ORIGIN)
}

/// Runs in the Chrome-spawned copy of `rho-gui`. Chrome speaks its native
/// messaging framing on stdio; the host relays the same bounded frames over
/// Rho's private Unix socket.
pub fn run() -> Result<()> {
    let mut socket = UnixStream::connect(socket_path()?).context("connect Rho browser socket")?;
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
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
}

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
        if let Err(error) = write_frame(&mut *connection.writer.lock().unwrap(), &message) {
            connection.pending.lock().unwrap().remove(&id);
            return Err(error).context("send browser extension request");
        }
        match receive.recv_timeout(RESPONSE_TIMEOUT) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => bail!("browser extension: {error}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                connection.pending.lock().unwrap().remove(&id);
                bail!("browser extension request timed out: {method}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
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
            "Rho browser extension did not connect; in chrome://extensions enable Developer mode \
             and Load unpacked {}",
            extension.display()
        ),
        None => "Rho browser extension did not connect; in chrome://extensions enable Developer \
                 mode and Load unpacked from the Rho client state directory"
            .to_owned(),
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
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let Some(reply) = connection.pending.lock().unwrap().remove(&id) else {
            continue;
        };
        let result = if message.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(message.get("result").cloned().unwrap_or(Value::Null))
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

pub fn write_installation(
    profile: &Path,
    brave_config: &Path,
    extension: &Path,
    executable: &Path,
) -> Result<()> {
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
    for hosts in [
        profile.join("NativeMessagingHosts"),
        brave_config.join("BraveSoftware/Brave-Browser/NativeMessagingHosts"),
    ] {
        fs::create_dir_all(&hosts)?;
        fs::write(hosts.join(format!("{HOST_NAME}.json")), &manifest)?;
    }
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
    fn installation_uses_the_fixed_extension_identity() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        let brave_config = temp.path().join("brave-config");
        let extension = temp.path().join("extension");
        let executable = temp.path().join("rho-gui");
        write_installation(&profile, &brave_config, &extension, &executable).unwrap();
        let host: Value = serde_json::from_slice(
            &fs::read(profile.join("NativeMessagingHosts/dev.rho.browser.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(host["allowed_origins"][0], EXTENSION_ORIGIN);
        assert_eq!(host["path"], executable.to_string_lossy().as_ref());
        assert_eq!(
            fs::read(
                brave_config
                    .join("BraveSoftware/Brave-Browser/NativeMessagingHosts/dev.rho.browser.json")
            )
            .unwrap(),
            serde_json::to_vec_pretty(&host).unwrap()
        );
        let manifest: Value =
            serde_json::from_slice(&fs::read(extension.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["manifest_version"], 3);
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
