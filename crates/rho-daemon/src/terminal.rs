//! Daemon-owned terminal sessions.
//!
//! Each session owns a PTY whose child runs inside an agent's view, plus the
//! only terminal emulator in the system (an alacritty [`Term`]). Clients are
//! dumb: they receive display state ([`rho_ui_proto::term`] rows/cursor) and
//! send input bytes; the daemon answers all terminal queries itself, so an
//! unattached terminal behaves exactly like an attached one. Sessions survive
//! client detach and die with their child process or the daemon.
//!
//! Output is synced per frame tick as row diffs against a per-client record
//! of what that client last displayed, so bandwidth is bounded by grid size ×
//! tick rate no matter how fast the PTY produces. Lines scrolling into
//! history are forwarded with exact accounting: the emulator's history cap
//! floats between [`HISTORY_TRIM`] and [`HISTORY_CAP`], and every `advance`
//! chunk is small enough that the unsaturated `history_size` delta measures
//! scrolled lines exactly. Clients that fall behind the retained window get
//! an honest `lost` count instead of silently missing lines.

use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::sync::{Arc, Mutex as StdMutex};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags as CellFlags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb, StdSyncHandler,
};
use anyhow::Context as _;
use rho_ui_proto::AgentId;
use rho_ui_proto::term::{
    TermCell, TermCellFlags, TermColor, TermCursor, TermCursorShape, TermRow, TermScreen,
    TermServerFrame,
};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, mpsc, oneshot};

mod keys;

/// Ceiling on the emulator's scrollback retention (and thus on the history
/// replayed to an attaching client).
const HISTORY_CAP: usize = 16384;
/// Retention restored when the cap is about to saturate; the gap to
/// [`HISTORY_CAP`] is the headroom that keeps scroll accounting exact.
const HISTORY_TRIM: usize = 8192;
/// Advance the emulator in chunks no larger than this so a chunk can never
/// scroll more lines than the available history headroom (worst case one
/// line per byte).
const ADVANCE_CHUNK: usize = 4096;
/// Most history lines delivered to one client per tick; older retained lines
/// keep flowing on later ticks, only unretained ones are reported `lost`.
const HISTORY_LINES_PER_TICK: usize = 1024;
/// Output sync cadence.
const TICK: std::time::Duration = std::time::Duration::from_millis(16);
/// Per-client frame queue; a client this far behind skips ticks (diffs
/// coalesce) until it drains.
const CLIENT_QUEUE: usize = 32;

const MIN_DIM: u16 = 2;
const MAX_DIM: u16 = 1000;

/// Everything needed to spawn a terminal's child process; built by the
/// caller (which knows agents and views), used when no session is running.
pub struct TerminalSpawn {
    pub view: Arc<rho_workspaces::View>,
    /// Program run through `direnv exec .` in the view's primary workdir.
    pub shell: String,
}

/// One client's half of an attach. Dropping `frames` detaches; the terminal
/// keeps running.
pub struct TerminalClient {
    pub frames: mpsc::Receiver<TermServerFrame>,
    pub input: mpsc::UnboundedSender<ClientInput>,
}

pub enum ClientInput {
    Bytes(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Encoded against the terminal's live modes when the session applies it.
    Keystroke(rho_ui_proto::term::TermKeystroke),
    Paste(String),
    Scroll {
        lines: i16,
        col: u16,
        row: u16,
        ctrl: bool,
        alt: bool,
        shift: bool,
    },
}

#[derive(Default)]
pub struct TerminalRegistry {
    sessions: Mutex<HashMap<(AgentId, u64), SessionHandle>>,
}

#[derive(Clone)]
struct SessionHandle {
    cmds: mpsc::UnboundedSender<SessionCmd>,
}

enum SessionCmd {
    Attach {
        cols: u16,
        rows: u16,
        reply: oneshot::Sender<TerminalClient>,
    },
    Describe {
        reply: oneshot::Sender<TerminalEntry>,
    },
}

/// A running terminal's identity and live state, for listings.
pub struct TerminalEntry {
    pub agent_id: AgentId,
    pub terminal_id: u64,
    pub title: Option<String>,
    pub cols: u16,
    pub rows: u16,
    /// Clients attached right now.
    pub clients: usize,
}

impl TerminalRegistry {
    /// Spawns `(agent_id, terminal_id)` and attaches to it; refuses if that
    /// terminal is already running.
    pub async fn create(
        self: &Arc<Self>,
        agent_id: AgentId,
        terminal_id: u64,
        cols: u16,
        rows: u16,
        spawn: TerminalSpawn,
    ) -> anyhow::Result<TerminalClient> {
        let cols = cols.clamp(MIN_DIM, MAX_DIM);
        let rows = rows.clamp(MIN_DIM, MAX_DIM);
        let handle = {
            let mut sessions = self.sessions.lock().await;
            anyhow::ensure!(
                !sessions.contains_key(&(agent_id, terminal_id)),
                "terminal {terminal_id} is already running (attach instead)"
            );
            let handle =
                Session::spawn(Arc::clone(self), agent_id, terminal_id, cols, rows, spawn).await?;
            sessions.insert((agent_id, terminal_id), handle.clone());
            handle
        };
        match attach_handle(&handle, cols, rows).await {
            Some(client) => Ok(client),
            None => {
                self.forget(agent_id, terminal_id, &handle.cmds).await;
                anyhow::bail!("terminal exited while attaching to it")
            }
        }
    }

    /// Attaches to `(agent_id, terminal_id)`; refuses if it is not running.
    pub async fn attach(
        &self,
        agent_id: AgentId,
        terminal_id: u64,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<TerminalClient> {
        let cols = cols.clamp(MIN_DIM, MAX_DIM);
        let rows = rows.clamp(MIN_DIM, MAX_DIM);
        loop {
            let handle = self
                .sessions
                .lock()
                .await
                .get(&(agent_id, terminal_id))
                .cloned()
                .with_context(|| format!("terminal {terminal_id} is not running"))?;
            if let Some(client) = attach_handle(&handle, cols, rows).await {
                return Ok(client);
            }
            // The session exited between lookup and attach: forget it and
            // retry, which reports "not running" unless it was respawned.
            self.forget(agent_id, terminal_id, &handle.cmds).await;
        }
    }

    /// Every running terminal, in stable (agent, id) order.
    pub async fn list(&self) -> Vec<TerminalEntry> {
        let handles: Vec<SessionHandle> = {
            let sessions = self.sessions.lock().await;
            let mut keyed: Vec<_> = sessions.iter().collect();
            keyed.sort_by_key(|(key, _)| **key);
            keyed
                .into_iter()
                .map(|(_, handle)| handle.clone())
                .collect()
        };
        let mut entries = Vec::new();
        for handle in handles {
            let (reply_tx, reply_rx) = oneshot::channel();
            if handle
                .cmds
                .send(SessionCmd::Describe { reply: reply_tx })
                .is_ok()
                && let Ok(entry) = reply_rx.await
            {
                entries.push(entry);
            }
        }
        entries
    }

    async fn forget(
        &self,
        agent_id: AgentId,
        terminal_id: u64,
        cmds: &mpsc::UnboundedSender<SessionCmd>,
    ) {
        let mut sessions = self.sessions.lock().await;
        if let Some(current) = sessions.get(&(agent_id, terminal_id))
            && current.cmds.same_channel(cmds)
        {
            sessions.remove(&(agent_id, terminal_id));
        }
    }
}

/// Attaches through a session's command channel; `None` means the session is
/// gone (channel closed or reply dropped).
async fn attach_handle(handle: &SessionHandle, cols: u16, rows: u16) -> Option<TerminalClient> {
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .cmds
        .send(SessionCmd::Attach {
            cols,
            rows,
            reply: reply_tx,
        })
        .ok()?;
    reply_rx.await.ok()
}

/// Collects emulator events raised during `Processor::advance`; drained
/// synchronously right after each chunk.
#[derive(Clone, Default)]
struct EventSink(Arc<StdMutex<Vec<TermEvent>>>);

impl EventListener for EventSink {
    fn send_event(&self, event: TermEvent) {
        self.0.lock().unwrap().push(event);
    }
}

struct ClientState {
    frames: mpsc::Sender<TermServerFrame>,
    /// Rows this client last displayed; diffs are computed against them, so
    /// a skipped tick simply coalesces into the next diff.
    screen: Vec<TermRow>,
    cursor: TermCursor,
    application_scroll: bool,
    /// History lines synced, against the session's `total_scrolled`.
    synced_scrolled: u64,
    /// Send a full snapshot instead of a diff (attach, resize).
    needs_snapshot: bool,
    title: Option<String>,
}

/// The emulator plus everything the sync algorithm reads and writes;
/// separate from the IO resources so select branches borrow disjointly.
struct SyncState {
    term: Term<EventSink>,
    events: EventSink,
    parser: Processor<StdSyncHandler>,
    clients: Vec<ClientState>,
    client_input_tx: mpsc::UnboundedSender<ClientInput>,
    /// Bytes waiting for the PTY (client input and query answers).
    outbuf: Vec<u8>,
    cols: u16,
    rows: u16,
    /// Monotonic count of lines ever scrolled into history.
    total_scrolled: u64,
    title: Option<String>,
    dirty: bool,
}

struct Session;

impl Session {
    async fn spawn(
        registry: Arc<TerminalRegistry>,
        agent_id: AgentId,
        terminal_id: u64,
        cols: u16,
        rows: u16,
        spawn: TerminalSpawn,
    ) -> anyhow::Result<SessionHandle> {
        let window_size = rustix_openpty::rustix::termios::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = rustix_openpty::openpty(None, Some(&window_size)).context("openpty")?;
        let master = pty.controller;
        let slave = pty.user;
        set_nonblocking(&master)?;

        let mut command = tokio::process::Command::new("direnv");
        command.args(["exec", ".", &spawn.shell]);
        for var in ["DIRENV_DIFF", "DIRENV_DIR", "DIRENV_FILE", "DIRENV_WATCHES"] {
            command.env_remove(var);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        spawn
            .view
            .prepare_command(&mut command, None, Vec::new())
            .await?;
        command.stdin(std::process::Stdio::from(slave.try_clone()?));
        command.stdout(std::process::Stdio::from(slave.try_clone()?));
        command.stderr(std::process::Stdio::from(slave));
        // Runs after prepare_command's namespace entry: make the child a
        // session leader with the PTY as its controlling terminal.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.kill_on_drop(true);
        let child = command.spawn().context("spawn terminal shell")?;

        let events = EventSink::default();
        let config = TermConfig {
            scrolling_history: HISTORY_CAP,
            ..Default::default()
        };
        let term = Term::new(
            config,
            &TermSize::new(cols as usize, rows as usize),
            events.clone(),
        );

        let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
        let (client_input_tx, client_input_rx) = mpsc::unbounded_channel();
        let state = SyncState {
            term,
            events,
            parser: Processor::new(),
            clients: Vec::new(),
            client_input_tx,
            outbuf: Vec::new(),
            cols,
            rows,
            total_scrolled: 0,
            title: None,
            dirty: false,
        };
        let master = AsyncFd::new(master).context("register pty master")?;
        tokio::spawn(run_session(
            registry,
            agent_id,
            terminal_id,
            cmds_tx.clone(),
            cmds_rx,
            client_input_rx,
            master,
            child,
            state,
        ));
        Ok(SessionHandle { cmds: cmds_tx })
    }
}

#[expect(clippy::too_many_arguments)]
async fn run_session(
    registry: Arc<TerminalRegistry>,
    agent_id: AgentId,
    terminal_id: u64,
    // Held so `cmds.recv()` never yields `None` while the session runs.
    _cmds_keepalive: mpsc::UnboundedSender<SessionCmd>,
    mut cmds: mpsc::UnboundedReceiver<SessionCmd>,
    mut client_input: mpsc::UnboundedReceiver<ClientInput>,
    master: AsyncFd<OwnedFd>,
    mut child: tokio::process::Child,
    mut state: SyncState,
) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pty_eof = false;
    let status = loop {
        tokio::select! {
            biased;
            Some(cmd) = cmds.recv() => match cmd {
                SessionCmd::Attach { cols, rows, reply } => {
                    let client = state.attach(&master, cols, rows);
                    let _ = reply.send(client);
                }
                SessionCmd::Describe { reply } => {
                    let _ = reply.send(TerminalEntry {
                        agent_id,
                        terminal_id,
                        title: state.title.clone(),
                        cols: state.cols,
                        rows: state.rows,
                        clients: state
                            .clients
                            .iter()
                            .filter(|client| !client.frames.is_closed())
                            .count(),
                    });
                }
            },
            Some(input) = client_input.recv() => match input {
                ClientInput::Bytes(bytes) => state.outbuf.extend_from_slice(&bytes),
                ClientInput::Resize { cols, rows } => state.resize(&master, cols, rows),
                ClientInput::Keystroke(keystroke) => {
                    if let Some(esc) = keys::to_esc_str(&keystroke, state.term.mode()) {
                        state.outbuf.extend_from_slice(esc.as_bytes());
                    } else if let Some(text) = &keystroke.key_char {
                        state.outbuf.extend_from_slice(text.as_bytes());
                    }
                }
                ClientInput::Paste(text) => {
                    let bytes = keys::encode_paste(&text, state.term.mode());
                    state.outbuf.extend_from_slice(&bytes);
                }
                ClientInput::Scroll { lines, col, row, ctrl, alt, shift } => {
                    let bytes = keys::encode_scroll(
                        lines, col, row, ctrl, alt, shift, state.term.mode(),
                    );
                    state.outbuf.extend_from_slice(&bytes);
                }
            },
            ready = master.readable(), if !pty_eof => {
                let Ok(mut guard) = ready else { break None };
                if !read_pty(&mut guard, &mut state) {
                    pty_eof = true;
                }
            },
            ready = master.writable(), if !state.outbuf.is_empty() => {
                let Ok(mut guard) = ready else { break None };
                write_pty(&mut guard, &mut state.outbuf);
            },
            _ = tick.tick(), if state.dirty => state.sync(),
            status = child.wait() => break status.ok(),
        }
    };
    // Drain whatever the child wrote before exiting, then tell everyone.
    if !pty_eof {
        loop {
            let Ok(Ok(mut guard)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), master.readable())
                    .await
            else {
                break;
            };
            if !read_pty(&mut guard, &mut state) {
                break;
            }
        }
    }
    state.sync();
    let code = status.and_then(|status| status.code());
    for client in &state.clients {
        let _ = client
            .frames
            .try_send(TermServerFrame::Exited { status: code });
    }
    registry
        .forget(agent_id, terminal_id, &_cmds_keepalive)
        .await;
}

/// Reads the PTY until it would block; false means EOF/EIO (child side gone).
fn read_pty(
    guard: &mut tokio::io::unix::AsyncFdReadyGuard<'_, OwnedFd>,
    state: &mut SyncState,
) -> bool {
    let mut buf = [0u8; 16384];
    // Bounded per wakeup so ticks and input stay interleaved under flood.
    for _ in 0..16 {
        match guard.try_io(|inner| {
            let n = unsafe { libc::read(inner.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(Ok(0)) => return false,
            Ok(Ok(n)) => state.advance(&buf[..n]),
            // EIO is the PTY's EOF once the child exits.
            Ok(Err(_)) => return false,
            Err(_would_block) => break,
        }
    }
    true
}

fn write_pty(guard: &mut tokio::io::unix::AsyncFdReadyGuard<'_, OwnedFd>, outbuf: &mut Vec<u8>) {
    while !outbuf.is_empty() {
        match guard.try_io(|inner| {
            let n = unsafe { libc::write(inner.as_raw_fd(), outbuf.as_ptr().cast(), outbuf.len()) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(Ok(n)) => {
                outbuf.drain(..n);
            }
            Ok(Err(_)) => {
                outbuf.clear();
                break;
            }
            Err(_would_block) => break,
        }
    }
}

impl SyncState {
    fn attach(&mut self, master: &AsyncFd<OwnedFd>, cols: u16, rows: u16) -> TerminalClient {
        let (frames_tx, frames_rx) = mpsc::channel(CLIENT_QUEUE);
        self.clients.push(ClientState {
            frames: frames_tx,
            screen: Vec::new(),
            cursor: TermCursor {
                row: 0,
                col: 0,
                visible: true,
                shape: TermCursorShape::Block,
            },
            application_scroll: false,
            // Start the full retained history behind so the first ticks
            // replay it.
            synced_scrolled: self
                .total_scrolled
                .saturating_sub(self.term.history_size() as u64),
            needs_snapshot: true,
            title: None,
        });
        self.resize(master, cols, rows);
        self.dirty = true;
        TerminalClient {
            frames: frames_rx,
            input: self.client_input_tx.clone(),
        }
    }

    fn resize(&mut self, master: &AsyncFd<OwnedFd>, cols: u16, rows: u16) {
        let cols = cols.clamp(MIN_DIM, MAX_DIM);
        let rows = rows.clamp(MIN_DIM, MAX_DIM);
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        let window_size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(master.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &window_size);
        }
        // Shrinking pushes screen lines into history; count them like any
        // other scroll so client accounting stays exact.
        let before = self.term.history_size();
        self.term
            .resize(TermSize::new(cols as usize, rows as usize));
        let after = self.term.history_size();
        self.total_scrolled += after.saturating_sub(before) as u64;
        self.cols = cols;
        self.rows = rows;
        for client in &mut self.clients {
            client.needs_snapshot = true;
        }
        self.dirty = true;
    }

    fn advance(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(ADVANCE_CHUNK) {
            if self.term.history_size() + chunk.len() > HISTORY_CAP {
                // Restore headroom by dropping the oldest retained lines;
                // the chunk below can then never saturate the cap, keeping
                // the delta an exact scroll count.
                self.term.grid_mut().update_history(HISTORY_TRIM);
                self.term.grid_mut().update_history(HISTORY_CAP);
            }
            let before = self.term.history_size();
            self.parser.advance(&mut self.term, chunk);
            let after = self.term.history_size();
            self.total_scrolled += after.saturating_sub(before) as u64;
            self.drain_events();
        }
        self.dirty = true;
    }

    fn drain_events(&mut self) {
        let events: Vec<TermEvent> = std::mem::take(&mut *self.events.0.lock().unwrap());
        for event in events {
            match event {
                TermEvent::PtyWrite(text) => self.outbuf.extend_from_slice(text.as_bytes()),
                TermEvent::Title(title) => {
                    self.title = Some(title);
                    self.dirty = true;
                }
                TermEvent::ResetTitle => {
                    self.title = None;
                    self.dirty = true;
                }
                TermEvent::ColorRequest(index, format) => {
                    let rgb = self.term.colors()[index].unwrap_or_else(|| default_color(index));
                    self.outbuf.extend_from_slice(format(rgb).as_bytes());
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    let text = format(WindowSize {
                        num_lines: self.rows,
                        num_cols: self.cols,
                        cell_width: 8,
                        cell_height: 16,
                    });
                    self.outbuf.extend_from_slice(text.as_bytes());
                }
                TermEvent::MouseCursorDirty
                | TermEvent::ClipboardStore(..)
                | TermEvent::ClipboardLoad(..)
                | TermEvent::CursorBlinkingChange
                | TermEvent::Bell
                | TermEvent::Wakeup
                | TermEvent::Exit
                | TermEvent::ChildExit(_) => {}
            }
        }
    }

    /// Sends every client what changed since its last successful frames.
    fn sync(&mut self) {
        if !self.dirty {
            return;
        }
        let screen = self.extract_screen();
        let cursor = self.extract_cursor();
        let application_scroll = self.term.mode().intersects(TermMode::MOUSE_MODE)
            || self
                .term
                .mode()
                .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL);
        let history_size = self.term.history_size() as u64;
        let grid = self.term.grid();
        // A client that could not take every frame this tick (queue full or
        // history still replaying) keeps the session dirty so the next tick
        // retries even if the PTY goes quiet.
        let mut any_behind = false;
        let mut index = 0;
        while index < self.clients.len() {
            let client = &mut self.clients[index];

            let mut alive = true;
            let mut congested = false;

            // History first, so a client's ring is always at least as new as
            // its screen.
            let delta = self.total_scrolled - client.synced_scrolled;
            if delta > 0 {
                let lost = delta.saturating_sub(history_size);
                let retained_pending = (delta - lost) as usize;
                let send_count = retained_pending.min(HISTORY_LINES_PER_TICK);
                let mut lines = Vec::with_capacity(send_count);
                for back in (0..send_count).rev() {
                    let line = Line(-((retained_pending - send_count + back + 1) as i32));
                    lines.push(extract_row(&grid[line], self.cols as usize));
                }
                match client
                    .frames
                    .try_send(TermServerFrame::History { lines, lost })
                {
                    Ok(()) => client.synced_scrolled += lost + send_count as u64,
                    Err(mpsc::error::TrySendError::Full(_)) => congested = true,
                    Err(mpsc::error::TrySendError::Closed(_)) => alive = false,
                }
            }

            if alive && !congested && client.title != self.title {
                let title = self.title.clone().unwrap_or_default();
                match client.frames.try_send(TermServerFrame::Title(title)) {
                    Ok(()) => client.title = self.title.clone(),
                    Err(mpsc::error::TrySendError::Full(_)) => congested = true,
                    Err(mpsc::error::TrySendError::Closed(_)) => alive = false,
                }
            }

            if alive && !congested {
                let frame = if client.needs_snapshot || client.screen.len() != screen.len() {
                    Some(TermServerFrame::Snapshot(TermScreen {
                        cols: self.cols,
                        rows: screen.clone(),
                        cursor,
                        application_scroll,
                    }))
                } else {
                    let rows: Vec<(u16, TermRow)> = screen
                        .iter()
                        .enumerate()
                        .filter(|&(row, cells)| client.screen[row] != *cells)
                        .map(|(row, cells)| (row as u16, cells.clone()))
                        .collect();
                    (!rows.is_empty()
                        || client.cursor != cursor
                        || client.application_scroll != application_scroll)
                        .then_some(TermServerFrame::Screen {
                            rows,
                            cursor,
                            application_scroll,
                        })
                };
                if let Some(frame) = frame {
                    match client.frames.try_send(frame) {
                        Ok(()) => {
                            client.needs_snapshot = false;
                            client.screen = screen.clone();
                            client.cursor = cursor;
                            client.application_scroll = application_scroll;
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => congested = true,
                        Err(mpsc::error::TrySendError::Closed(_)) => alive = false,
                    }
                }
            }

            if alive {
                any_behind |= congested || client.synced_scrolled < self.total_scrolled;
                index += 1;
            } else {
                self.clients.remove(index);
            }
        }
        self.dirty = any_behind;
    }

    fn extract_screen(&self) -> Vec<TermRow> {
        let grid = self.term.grid();
        (0..grid.screen_lines())
            .map(|line| extract_row(&grid[Line(line as i32)], grid.columns()))
            .collect()
    }

    fn extract_cursor(&self) -> TermCursor {
        let point = self.term.grid().cursor.point;
        let style = self.term.cursor_style();
        let (shape, style_visible) = match style.shape {
            CursorShape::Block | CursorShape::HollowBlock => (TermCursorShape::Block, true),
            CursorShape::Underline => (TermCursorShape::Underline, true),
            CursorShape::Beam => (TermCursorShape::Beam, true),
            CursorShape::Hidden => (TermCursorShape::Block, false),
        };
        TermCursor {
            row: point.line.0.max(0) as u16,
            col: point.column.0 as u16,
            visible: style_visible && self.term.mode().contains(TermMode::SHOW_CURSOR),
            shape,
        }
    }
}

fn extract_row(row: &alacritty_terminal::grid::Row<Cell>, columns: usize) -> TermRow {
    let default = TermCell::default();
    let mut cells: Vec<TermCell> = (0..columns.min(row.len()))
        .map(|column| convert_cell(&row[Column(column)]))
        .collect();
    while cells.last() == Some(&default) {
        cells.pop();
    }
    TermRow { cells }
}

fn convert_cell(cell: &Cell) -> TermCell {
    let mut flags = 0u16;
    let map = [
        (CellFlags::BOLD, TermCellFlags::BOLD),
        (CellFlags::DIM, TermCellFlags::DIM),
        (CellFlags::ITALIC, TermCellFlags::ITALIC),
        (CellFlags::ALL_UNDERLINES, TermCellFlags::UNDERLINE),
        (CellFlags::INVERSE, TermCellFlags::INVERSE),
        (CellFlags::STRIKEOUT, TermCellFlags::STRIKEOUT),
        (CellFlags::HIDDEN, TermCellFlags::HIDDEN),
        (CellFlags::WIDE_CHAR, TermCellFlags::WIDE),
        (CellFlags::WIDE_CHAR_SPACER, TermCellFlags::WIDE_SPACER),
        (
            CellFlags::LEADING_WIDE_CHAR_SPACER,
            TermCellFlags::WIDE_SPACER,
        ),
        (CellFlags::WRAPLINE, TermCellFlags::WRAPLINE),
    ];
    for (cell_flag, wire_flag) in map {
        if cell.flags.intersects(cell_flag) {
            flags |= wire_flag;
        }
    }
    TermCell {
        c: cell.c,
        extra: cell
            .zerowidth()
            .map(|extra| extra.iter().collect::<String>()),
        fg: convert_color(cell.fg),
        bg: convert_color(cell.bg),
        flags,
    }
}

fn convert_color(color: AnsiColor) -> TermColor {
    match color {
        AnsiColor::Spec(rgb) => TermColor::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(index) => TermColor::Indexed(index),
        AnsiColor::Named(named) => match named {
            NamedColor::Foreground
            | NamedColor::BrightForeground
            | NamedColor::DimForeground
            | NamedColor::Cursor => TermColor::Foreground,
            NamedColor::Background => TermColor::Background,
            named => {
                let index = named as usize;
                if index < 16 {
                    TermColor::Indexed(index as u8)
                } else if let Some(dimmed) = dim_to_normal(named) {
                    TermColor::Indexed(dimmed)
                } else {
                    TermColor::Foreground
                }
            }
        },
    }
}

fn dim_to_normal(named: NamedColor) -> Option<u8> {
    Some(match named {
        NamedColor::DimBlack => 0,
        NamedColor::DimRed => 1,
        NamedColor::DimGreen => 2,
        NamedColor::DimYellow => 3,
        NamedColor::DimBlue => 4,
        NamedColor::DimMagenta => 5,
        NamedColor::DimCyan => 6,
        NamedColor::DimWhite => 7,
        _ => return None,
    })
}

/// Fallback answers for OSC color queries when the palette entry was never
/// set: the standard xterm 256 palette plus neutral dark defaults.
fn default_color(index: usize) -> Rgb {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let (r, g, b) = match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let index = index - 16;
            let level = |value: usize| -> u8 {
                if value == 0 {
                    0
                } else {
                    (40 * value + 55) as u8
                }
            };
            (level(index / 36), level((index / 6) % 6), level(index % 6))
        }
        232..=255 => {
            let gray = (8 + 10 * (index - 232)) as u8;
            (gray, gray, gray)
        }
        // Foreground/cursor and everything else: light gray on
        // near-black background.
        _ if index == NamedColor::Background as usize => (24, 24, 24),
        _ => (216, 216, 216),
    };
    Rgb { r, g, b }
}

fn set_nonblocking(fd: &OwnedFd) -> anyhow::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        anyhow::ensure!(flags >= 0, "F_GETFL failed on pty master");
        anyhow::ensure!(
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0,
            "F_SETFL failed on pty master"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rho_ui_proto::term::{ScrollbackItem, WireScreen};

    use super::*;

    /// The real client reconstruction fed straight from a session's frame
    /// queue, so these tests check the whole sync protocol end to end.
    struct FakeClient {
        frames: mpsc::Receiver<TermServerFrame>,
        screen: WireScreen,
    }

    impl FakeClient {
        fn new(frames: mpsc::Receiver<TermServerFrame>) -> Self {
            Self {
                frames,
                screen: WireScreen::new(usize::MAX),
            }
        }

        fn drain(&mut self) {
            while let Ok(frame) = self.frames.try_recv() {
                self.screen.apply(frame);
            }
        }

        fn lost(&self) -> u64 {
            self.screen.lost_lines()
        }

        /// Contiguous scrollback (since the last gap) plus non-empty screen
        /// rows: the terminal text a client can faithfully show.
        fn visible_lines(&mut self) -> Vec<String> {
            self.drain();
            let mut lines = Vec::new();
            for item in &self.screen.scrollback {
                match item {
                    ScrollbackItem::Line(row) => lines.push(row.text()),
                    ScrollbackItem::Gap(_) => lines.clear(),
                }
            }
            lines.extend(
                self.screen
                    .rows
                    .iter()
                    .map(TermRow::text)
                    .filter(|line| !line.is_empty()),
            );
            lines
        }
    }

    fn new_state(cols: u16, rows: u16) -> SyncState {
        let events = EventSink::default();
        let (client_input_tx, _client_input_rx) = mpsc::unbounded_channel();
        // The receiver must outlive the test's sender uses; leak it.
        std::mem::forget(_client_input_rx);
        SyncState {
            term: Term::new(
                TermConfig {
                    scrolling_history: HISTORY_CAP,
                    ..Default::default()
                },
                &TermSize::new(cols as usize, rows as usize),
                events.clone(),
            ),
            events,
            parser: Processor::new(),
            clients: Vec::new(),
            client_input_tx,
            outbuf: Vec::new(),
            cols,
            rows,
            total_scrolled: 0,
            title: None,
            dirty: false,
        }
    }

    /// A PTY nobody reads, purely so attach/resize have a real fd.
    fn dummy_master() -> AsyncFd<OwnedFd> {
        let pty = rustix_openpty::openpty(None, None).unwrap();
        set_nonblocking(&pty.controller).unwrap();
        std::mem::forget(pty.user);
        AsyncFd::new(pty.controller).unwrap()
    }

    fn settle(state: &mut SyncState, client: &mut FakeClient) {
        // A congested or history-replaying client keeps the state dirty;
        // alternate sync and drain until fully caught up.
        loop {
            state.sync();
            client.drain();
            if !state.dirty {
                break;
            }
        }
    }

    #[tokio::test]
    async fn client_reconstruction_matches_emulator_exactly() {
        let mut state = new_state(40, 6);
        let master = dummy_master();
        let TerminalClient { frames, input: _ } = state.attach(&master, 40, 6);
        let mut client = FakeClient::new(frames);

        let total = 3000usize;
        let mut written = Vec::new();
        let mut pending = Vec::new();
        for index in 0..total {
            let line = format!("line-{index:05}");
            pending.extend_from_slice(line.as_bytes());
            pending.extend_from_slice(b"\r\n");
            written.push(line);
            // Uneven batches exercise chunking and per-tick history limits.
            if index % 137 == 0 || index + 1 == total {
                state.advance(&pending);
                pending.clear();
                settle(&mut state, &mut client);
            }
        }

        assert_eq!(client.lost(), 0, "a drained client must never lose lines");
        assert_eq!(client.visible_lines(), written);
    }

    #[tokio::test]
    async fn blast_output_reports_exact_loss_and_correct_tail() {
        let mut state = new_state(40, 6);
        let master = dummy_master();
        let TerminalClient { frames, input: _ } = state.attach(&master, 40, 6);
        let mut client = FakeClient::new(frames);
        settle(&mut state, &mut client);

        // Far more than HISTORY_CAP lines between syncs: retention must trim
        // and the client must see an exact loss count.
        let total = 3 * HISTORY_CAP;
        let mut written = Vec::new();
        let mut pending = Vec::new();
        for index in 0..total {
            let line = format!("blast-{index:05}");
            pending.extend_from_slice(line.as_bytes());
            pending.extend_from_slice(b"\r\n");
            written.push(line);
        }
        state.advance(&pending);
        settle(&mut state, &mut client);

        assert!(
            client.lost() > 0,
            "blasting past retention must report loss"
        );
        let visible = client.visible_lines();
        // Every line that was not reported lost arrived, in order, ending at
        // the very last written line.
        assert_eq!(client.lost() + visible.len() as u64, total as u64);
        assert_eq!(visible, written[total - visible.len()..]);
    }

    #[tokio::test]
    async fn shell_end_to_end_over_registry() {
        if !std::process::Command::new("direnv")
            .arg("version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            eprintln!("skipping: direnv unavailable");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = Arc::new(
            rho_workspaces::Repo::open_plain_with_path_overrides(
                temp.path(),
                rho_workspaces::PathOverrides::default(),
            )
            .unwrap(),
        );
        let workspace = repo.user_checkout().await.unwrap();
        let view = rho_workspaces::View::new(vec![workspace]).unwrap();

        let registry = Arc::new(TerminalRegistry::default());
        let agent_id =
            AgentId::from_counter(1, &rho_agent::db::AgentIdDomain(42)).expect("counter 1 encodes");
        let mut client = registry
            .create(
                agent_id,
                0,
                80,
                24,
                TerminalSpawn {
                    view: Arc::clone(&view),
                    shell: "sh".to_owned(),
                },
            )
            .await
            .unwrap();
        client
            .input
            .send(ClientInput::Bytes(b"echo term-e2e-$((20+3))\r".to_vec()))
            .unwrap();
        wait_for_line(&mut client, "term-e2e-23").await;

        // A second client attaching sees the same screen from its snapshot,
        // and the listing shows the one running terminal.
        let mut second = registry.attach(agent_id, 0, 80, 24).await.unwrap();
        wait_for_line(&mut second, "term-e2e-23").await;
        let listed = registry.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].terminal_id, 0);
        assert_eq!(listed[0].clients, 2);
        assert!(
            registry.attach(agent_id, 7, 80, 24).await.is_err(),
            "attach must refuse ids that are not running"
        );

        client
            .input
            .send(ClientInput::Bytes(b"exit\r".to_vec()))
            .unwrap();
        let exited = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match client.frames.recv().await {
                    Some(TermServerFrame::Exited { .. }) | None => break,
                    Some(_) => {}
                }
            }
        })
        .await;
        assert!(exited.is_ok(), "terminal exit must reach the client");
    }

    /// Receives frames until some screen row or history line contains
    /// `needle` (panics after 20s).
    async fn wait_for_line(client: &mut TerminalClient, needle: &str) {
        let mut screen = WireScreen::new(usize::MAX);
        let found = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let frame = client.frames.recv().await.expect("terminal stream ended");
                if matches!(&frame, TermServerFrame::Exited { .. }) {
                    panic!("terminal exited early");
                }
                screen.apply(frame);
                let history = screen.scrollback.iter().filter_map(|item| match item {
                    ScrollbackItem::Line(row) => Some(row.text()),
                    ScrollbackItem::Gap(_) => None,
                });
                if history
                    .chain(screen.rows.iter().map(TermRow::text))
                    .any(|line| line.contains(needle))
                {
                    break;
                }
            }
        })
        .await;
        assert!(found.is_ok(), "expected {needle:?} on the terminal");
    }
}
