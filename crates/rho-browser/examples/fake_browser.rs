//! A standalone fake browser, for the isolated GUI QA run.
//!
//! Point `RHO_CUSTOM_BRAVE_BIN` at this and the unmodified client launches
//! it the way it launches Brave: it speaks the extension's native-messaging
//! framing over `RHO_BROWSER_SOCKET_PATH`, answers create/focus/close/list,
//! and posts the `page-metadata` events a real extension posts, including
//! the `opened_from` that says a tab was opened from another page.
//!
//! The rig drives tabs the reader would open by hand through a control
//! socket at `RHO_FAKE_BROWSER_CONTROL`, one JSON line per command:
//!
//! ```text
//! {"open": {"url": "https://docs.rs/tokio", "title": "tokio", "opened_from": "last"}}
//! {"close": "web-<uuid>"}
//! {"list": true}
//! ```
//!
//! `opened_from` takes a page id, or "last" for the page most recently
//! opened without one, which is what a burst of ctrl-clicks off a search
//! page looks like.
//!
//! It also opens one Wayland top-level on the client's private compositor
//! and keeps painting it. That is not decoration: the compositor drops a
//! page whose window never binds, so without it the client tears the
//! browser down ten seconds in.

use std::io::{BufRead as _, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use uuid::Uuid;

const CONTROL_PATH_ENV: &str = "RHO_FAKE_BROWSER_CONTROL";

struct Tab {
    id: Uuid,
    url: String,
    title: String,
    opened_from: Option<Uuid>,
    created_at_ms: u64,
}

struct Browser {
    socket: Mutex<UnixStream>,
    tabs: Mutex<Vec<Tab>>,
}

impl Browser {
    /// Opens a tab and tells the client about it, in that order: the client
    /// learns a tab exists only from the metadata event.
    fn open(&self, url: String, title: Option<String>, opened_from: Option<Uuid>) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let title = title.unwrap_or_else(|| title_for(&url));
        self.tabs.lock().unwrap().push(Tab {
            id,
            url: url.clone(),
            title: title.clone(),
            opened_from,
            created_at_ms: now_ms(),
        });
        self.send(&json!({
            "event": "page-metadata",
            "page_id": format!("web-{id}"),
            "title": title,
            "url": url,
            "opened_from": opened_from.map(|origin| format!("web-{origin}")).unwrap_or_default(),
        }))?;
        Ok(id)
    }

    fn close(&self, id: Uuid) -> Result<()> {
        self.tabs.lock().unwrap().retain(|tab| tab.id != id);
        self.send(&json!({
            "event": "page-metadata-removed",
            "page_id": format!("web-{id}"),
        }))
    }

    /// The page id a ctrl-click would carry: the last tab opened for its own
    /// sake, which is the page the reader is reading.
    fn last_origin(&self) -> Option<Uuid> {
        let tabs = self.tabs.lock().unwrap();
        tabs.iter()
            .rev()
            .find(|tab| tab.opened_from.is_none())
            .map(|tab| tab.id)
    }

    fn record(&self, id: Uuid) -> Option<Value> {
        let tabs = self.tabs.lock().unwrap();
        let tab = tabs.iter().find(|tab| tab.id == id)?;
        Some(json!({
            "id": tab.id,
            "launch_url": tab.url,
            "created_at_ms": tab.created_at_ms,
        }))
    }

    fn send(&self, message: &Value) -> Result<()> {
        let body = serde_json::to_vec(message)?;
        let mut socket = self.socket.lock().unwrap();
        socket.write_all(&(body.len() as u32).to_le_bytes())?;
        socket.write_all(&body)?;
        socket.flush()?;
        Ok(())
    }

    fn handle(&self, request: &Value) -> Result<()> {
        let Some(id) = request.get("id").and_then(Value::as_u64) else {
            return Ok(());
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let reply = match self.result(method, &params) {
            Ok(result) => json!({"id": id, "ok": true, "result": result}),
            Err(error) => json!({"id": id, "ok": false, "error": error.to_string()}),
        };
        self.send(&reply)
    }

    fn result(&self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "create" => {
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .context("create needs a url")?;
                let id = self.open(url.to_owned(), None, None)?;
                self.record(id).context("created tab vanished")
            }
            "focus" | "close" => {
                let id = params
                    .get("id")
                    .and_then(Value::as_str)
                    .context("needs an id")?
                    .parse()?;
                if method == "close" {
                    self.close(id)?;
                }
                Ok(Value::Null)
            }
            "list" => {
                let tabs = self.tabs.lock().unwrap();
                Ok(Value::Array(
                    tabs.iter()
                        .map(|tab| {
                            json!({
                                "id": tab.id,
                                "launch_url": tab.url,
                                "created_at_ms": tab.created_at_ms,
                            })
                        })
                        .collect(),
                ))
            }
            other => bail!("unknown method {other}"),
        }
    }

    fn control(&self, command: &Value) -> Result<Value> {
        if let Some(open) = command.get("open") {
            let url = open
                .get("url")
                .and_then(Value::as_str)
                .context("open needs a url")?;
            let origin = match open.get("opened_from").and_then(Value::as_str) {
                None => None,
                Some("last") => Some(self.last_origin().context("no page to open a tab from")?),
                Some(id) => Some(id.strip_prefix("web-").unwrap_or(id).parse()?),
            };
            let title = open.get("title").and_then(Value::as_str).map(str::to_owned);
            let id = self.open(url.to_owned(), title, origin)?;
            return Ok(json!({"page_id": format!("web-{id}")}));
        }
        if let Some(id) = command.get("close").and_then(Value::as_str) {
            self.close(id.strip_prefix("web-").unwrap_or(id).parse()?)?;
            return Ok(json!({"closed": id}));
        }
        let tabs = self.tabs.lock().unwrap();
        Ok(Value::Array(
            tabs.iter()
                .map(|tab| {
                    json!({
                        "page_id": format!("web-{}", tab.id),
                        "title": tab.title,
                        "url": tab.url,
                        "opened_from": tab.opened_from.map(|origin| format!("web-{origin}")),
                    })
                })
                .collect(),
        ))
    }
}

fn main() -> Result<()> {
    let socket_path =
        std::env::var("RHO_BROWSER_SOCKET_PATH").context("RHO_BROWSER_SOCKET_PATH is not set")?;
    // The client binds the socket just before spawning us, so a first
    // connect can lose the race with its own listener.
    let deadline = Instant::now() + Duration::from_secs(10);
    let socket = loop {
        match UnixStream::connect(&socket_path) {
            Ok(socket) => break socket,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("connect the Rho browser socket"),
        }
    };
    let browser: &'static Browser = Box::leak(Box::new(Browser {
        socket: Mutex::new(socket.try_clone()?),
        tabs: Mutex::new(Vec::new()),
    }));

    std::thread::spawn(move || {
        let mut reader = socket;
        while let Ok(Some(frame)) = read_frame(&mut reader) {
            let Ok(request) = serde_json::from_slice::<Value>(&frame) else {
                continue;
            };
            if let Err(error) = browser.handle(&request) {
                eprintln!("fake browser: {error:#}");
                break;
            }
        }
        // The client shut the bridge: a real browser would be quitting too.
        std::process::exit(0);
    });

    // The compositor gives a page ten seconds to bind a top-level.
    std::thread::spawn(window::run);

    eprintln!("fake browser: ready");
    let Some(control_path) = std::env::var_os(CONTROL_PATH_ENV) else {
        // No rig driving it, so it is only here to answer the client.
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    };
    let _ = std::fs::remove_file(&control_path);
    let control = UnixListener::bind(&control_path).context("bind the fake browser control")?;
    for stream in control.incoming() {
        let stream = stream?;
        let mut writer = stream.try_clone()?;
        for line in BufReader::new(stream).lines() {
            let line = line?;
            let reply = match serde_json::from_str(&line).map_err(anyhow::Error::from) {
                Ok(command) => browser.control(&command),
                Err(error) => Err(error),
            };
            let reply = match reply {
                Ok(value) => json!({"ok": true, "result": value}),
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            };
            writeln!(writer, "{reply}")?;
            writer.flush()?;
        }
    }
    Ok(())
}

/// The title a browser would show, from the URL alone: a search keeps its
/// query, anything else keeps its last path segment.
fn title_for(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    let host = parsed.host_str().unwrap_or(url);
    let stem = host.strip_prefix("www.").unwrap_or(host);
    if let Some((_, query)) = parsed.query_pairs().find(|(key, _)| key == "q") {
        return format!("{query} - {stem} search");
    }
    match parsed
        .path_segments()
        .and_then(|segments| segments.filter(|segment| !segment.is_empty()).next_back())
    {
        Some(segment) => format!("{} · {stem}", segment.replace(['-', '_'], " ")),
        None => stem.to_owned(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn read_frame(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_le_bytes(header) as usize;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

/// The one browser window. A real Chromium binds a top-level on the private
/// compositor and keeps a buffer on it; the client waits on that before it
/// will show a page, so the fake paints a flat surface and nothing else.
mod window {
    use std::fs::File;
    use std::io::{Seek as _, SeekFrom, Write as _};
    use std::os::fd::AsFd as _;

    use anyhow::Result;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    };
    use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    pub fn run() {
        if let Err(error) = open() {
            eprintln!("fake browser window: {error:#}");
        }
    }

    fn open() -> Result<()> {
        let connection = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<Window>(&connection)?;
        let handle = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&handle, 4..=6, ())?;
        let shm: wl_shm::WlShm = globals.bind(&handle, 1..=2, ())?;
        let shell: xdg_wm_base::XdgWmBase = globals.bind(&handle, 1..=6, ())?;

        let surface = compositor.create_surface(&handle, ());
        let xdg = shell.get_xdg_surface(&surface, &handle, ());
        let toplevel = xdg.get_toplevel(&handle, ());
        toplevel.set_title("Rho fake browser".into());
        toplevel.set_app_id("brave-browser".into());
        surface.commit();

        let mut window = Window {
            shm,
            surface,
            size: (1280, 720),
            frame: tempfile::tempfile()?,
        };
        loop {
            queue.blocking_dispatch(&mut window)?;
        }
    }

    struct Window {
        shm: wl_shm::WlShm,
        surface: wl_surface::WlSurface,
        size: (i32, i32),
        frame: File,
    }

    impl Window {
        /// One flat frame at the configured size. A pool per frame costs
        /// nothing here and keeps the buffer's lifetime obvious.
        fn draw(&mut self, handle: &QueueHandle<Self>) -> Result<()> {
            let (width, height) = self.size;
            let stride = width * 4;
            let pixels = vec![0x1b_u8; (stride * height) as usize];
            self.frame.seek(SeekFrom::Start(0))?;
            self.frame.write_all(&pixels)?;
            self.frame.flush()?;
            let pool = self
                .shm
                .create_pool(self.frame.as_fd(), pixels.len() as i32, handle, ());
            let buffer = pool.create_buffer(
                0,
                width,
                height,
                stride,
                wl_shm::Format::Xrgb8888,
                handle,
                (),
            );
            self.surface.attach(Some(&buffer), 0, 0);
            self.surface.damage_buffer(0, 0, width, height);
            self.surface.commit();
            pool.destroy();
            Ok(())
        }
    }

    impl Dispatch<xdg_surface::XdgSurface, ()> for Window {
        fn event(
            window: &mut Self,
            xdg: &xdg_surface::XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            handle: &QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = event {
                xdg.ack_configure(serial);
                if let Err(error) = window.draw(handle) {
                    eprintln!("fake browser window: {error:#}");
                }
            }
        }
    }

    impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Window {
        fn event(
            window: &mut Self,
            _: &xdg_toplevel::XdgToplevel,
            event: xdg_toplevel::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                    window.size = (width, height);
                }
                xdg_toplevel::Event::Close => std::process::exit(0),
                _ => {}
            }
        }
    }

    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Window {
        fn event(
            _: &mut Self,
            shell: &xdg_wm_base::XdgWmBase,
            event: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_wm_base::Event::Ping { serial } = event {
                shell.pong(serial);
            }
        }
    }

    impl Dispatch<wl_buffer::WlBuffer, ()> for Window {
        fn event(
            _: &mut Self,
            buffer: &wl_buffer::WlBuffer,
            event: wl_buffer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_buffer::Event::Release = event {
                buffer.destroy();
            }
        }
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Window {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    delegate_noop!(Window: ignore wl_compositor::WlCompositor);
    delegate_noop!(Window: ignore wl_shm::WlShm);
    delegate_noop!(Window: ignore wl_shm_pool::WlShmPool);
    delegate_noop!(Window: ignore wl_surface::WlSurface);
}
