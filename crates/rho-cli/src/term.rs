//! `rho term <agent>`: attach the real terminal to a daemon-owned terminal.
//!
//! A passthrough viewer: raw-mode stdin bytes go to the daemon, display
//! frames come back as rows ([`rho_ui_proto::term`]) and are painted with
//! plain ANSI onto the alternate screen. Detaching (Ctrl-\ or closing) leaves
//! the terminal running in the daemon; history frames are ignored — the
//! alternate screen has no scrollback to put them in.

use std::io::Write as _;
use std::os::fd::AsRawFd as _;

use anyhow::{Context as _, Result};
use rho_ui_proto::term::{
    FrameApplied, TermCell, TermCellFlags, TermClientFrame, TermColor, TermCursor, TermCursorShape,
    TermRow, TermServerFrame, WireScreen,
};
use rho_ui_proto::{ClientMessage, ServerMessage};
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;

/// Detach byte: Ctrl-\ (0x1c). Raw mode disables its usual SIGQUIT meaning.
const DETACH: u8 = 0x1c;

#[derive(Clone, clap::Args)]
pub(crate) struct TermArgs {
    /// Agent handle or id prefix ("eng-ht08"); omit to list all terminals.
    agent: Option<String>,
    /// Terminal id to attach to (defaults to the agent's only terminal).
    #[arg(long = "id")]
    terminal_id: Option<u64>,
    /// Start a new terminal for the agent instead of attaching.
    #[arg(long)]
    new: bool,
    /// List running terminals instead of attaching.
    #[arg(long)]
    list: bool,
    #[arg(long = "socket-path")]
    socket_path: Option<std::path::PathBuf>,
}

pub(crate) async fn run(args: TermArgs) -> Result<()> {
    let socket_path = match args.socket_path {
        Some(path) => path,
        None => rho_daemon::default_socket_path()?,
    };
    let Some(agent) = args.agent else {
        anyhow::ensure!(!args.new, "--new needs an agent: rho term <agent> --new");
        return list_terminals(&socket_path, None).await;
    };
    if args.list {
        return list_terminals(&socket_path, Some(&agent)).await;
    }
    let (cols, rows) = terminal_size()?;
    let (terminal_id, open) = if args.new {
        let terminal_id = match args.terminal_id {
            Some(id) => id,
            None => next_terminal_id(&socket_path, &agent).await?,
        };
        let open = ClientMessage::TerminalCreate {
            agent: agent.clone(),
            terminal_id,
            attach: true,
            cols,
            rows,
        };
        (terminal_id, open)
    } else {
        let terminal_id = match args.terminal_id {
            Some(id) => id,
            None => sole_terminal_id(&socket_path, &agent).await?,
        };
        let open = ClientMessage::TerminalAttach {
            agent: agent.clone(),
            terminal_id,
            cols,
            rows,
        };
        (terminal_id, open)
    };
    let stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .context("connect to rho daemon")?;
    let (mut reader, mut writer) = stream.into_split();
    rho_ui_proto::write_frame(&mut writer, &open).await?;
    match rho_ui_proto::read_frame::<_, ServerMessage>(&mut reader).await? {
        ServerMessage::TerminalOpened { .. } => {}
        ServerMessage::TerminalRefused { reason } => anyhow::bail!("{reason}"),
        _ => anyhow::bail!("unexpected daemon reply on terminal stream"),
    }

    eprintln!("[attached to {agent} terminal {terminal_id}; detach: Ctrl-\\]");
    let _guard = RawModeGuard::enter()?;

    // Frames arrive through a task because frame reads are not
    // cancel-safe under select.
    let (frame_tx, mut frame_rx) = mpsc::channel::<TermServerFrame>(64);
    let frame_task = tokio::spawn(async move {
        while let Ok(frame) = rho_ui_proto::read_frame::<_, TermServerFrame>(&mut reader).await {
            if frame_tx.send(frame).await.is_err() {
                break;
            }
        }
    });

    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut stdin = tokio::io::stdin();
    let mut stdin_buf = [0u8; 4096];
    let mut painter = Painter::default();
    let mut exit_status: Option<Option<i32>> = None;

    loop {
        tokio::select! {
            frame = frame_rx.recv() => match frame {
                Some(TermServerFrame::Exited { status }) => {
                    exit_status = Some(status);
                    break;
                }
                Some(frame) => painter.apply(frame)?,
                // Stream closed: daemon gone or terminal dropped.
                None => break,
            },
            read = stdin.read(&mut stdin_buf) => {
                let Ok(n) = read else { break };
                if n == 0 {
                    break;
                }
                let bytes = &stdin_buf[..n];
                if bytes.contains(&DETACH) {
                    break;
                }
                if rho_ui_proto::write_frame(
                    &mut writer,
                    &TermClientFrame::Input(bytes.to_vec()),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            _ = winch.recv() => {
                if let Ok((cols, rows)) = terminal_size() {
                    let _ = rho_ui_proto::write_frame(
                        &mut writer,
                        &TermClientFrame::Resize { cols, rows },
                    )
                    .await;
                }
            }
        }
    }
    frame_task.abort();
    drop(_guard);
    match exit_status {
        Some(status) => {
            eprintln!("[terminal exited: {status:?}]");
        }
        None => eprintln!("[detached]"),
    }
    Ok(())
}

/// One-shot `TerminalList` request on a fresh connection.
async fn fetch_terminals(
    socket_path: &std::path::Path,
    agent: Option<&str>,
) -> Result<Vec<rho_ui_proto::term::TerminalInfo>> {
    let mut stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .context("connect to rho daemon")?;
    rho_ui_proto::write_frame(
        &mut stream,
        &ClientMessage::TerminalList {
            agent: agent.map(str::to_owned),
        },
    )
    .await?;
    match rho_ui_proto::read_frame::<_, ServerMessage>(&mut stream).await? {
        ServerMessage::TerminalList { terminals } => Ok(terminals),
        ServerMessage::TerminalRefused { reason } => anyhow::bail!("{reason}"),
        _ => anyhow::bail!("unexpected daemon reply to terminal list"),
    }
}

async fn list_terminals(socket_path: &std::path::Path, agent: Option<&str>) -> Result<()> {
    let terminals = fetch_terminals(socket_path, agent).await?;
    if terminals.is_empty() {
        println!("no running terminals");
        return Ok(());
    }
    print_terminals(&terminals);
    Ok(())
}

fn print_terminals(terminals: &[rho_ui_proto::term::TerminalInfo]) {
    println!(
        "{:<14} {:>4} {:>9} {:>7}  TITLE",
        "AGENT", "ID", "SIZE", "CLIENTS"
    );
    for terminal in terminals {
        println!(
            "{:<14} {:>4} {:>9} {:>7}  {}",
            terminal.agent,
            terminal.terminal_id,
            format!("{}x{}", terminal.cols, terminal.rows),
            terminal.clients,
            terminal.title,
        );
    }
}

/// Smallest id above the agent's running terminals (0 for the first one).
async fn next_terminal_id(socket_path: &std::path::Path, agent: &str) -> Result<u64> {
    let terminals = fetch_terminals(socket_path, Some(agent)).await?;
    Ok(terminals
        .iter()
        .map(|terminal| terminal.terminal_id.saturating_add(1))
        .max()
        .unwrap_or(0))
}

/// The id to attach when none was given: the agent's only running terminal.
async fn sole_terminal_id(socket_path: &std::path::Path, agent: &str) -> Result<u64> {
    let terminals = fetch_terminals(socket_path, Some(agent)).await?;
    match terminals.as_slice() {
        [] => anyhow::bail!("no terminals running for {agent}; start one with --new"),
        [only] => Ok(only.terminal_id),
        many => {
            print_terminals(many);
            anyhow::bail!("multiple terminals running for {agent}; pick one with --id")
        }
    }
}

/// Puts the controlling terminal into raw mode on the alternate screen and
/// restores it on drop (including unwinds).
struct RawModeGuard {
    original: libc::termios,
}

impl RawModeGuard {
    fn enter() -> Result<Self> {
        let fd = std::io::stdin().as_raw_fd();
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        anyhow::ensure!(
            unsafe { libc::tcgetattr(fd, &mut original) } == 0,
            "stdin is not a terminal"
        );
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        anyhow::ensure!(
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } == 0,
            "failed to enter raw mode"
        );
        // Alternate screen, clear, hide cursor until the first frame.
        print!("\x1b[?1049h\x1b[2J\x1b[?25l");
        let _ = std::io::stdout().flush();
        Ok(Self { original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        print!("\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
        let fd = std::io::stdin().as_raw_fd();
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.original) };
    }
}

fn terminal_size() -> Result<(u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe { libc::ioctl(std::io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    anyhow::ensure!(
        ok == 0 && size.ws_col > 0 && size.ws_row > 0,
        "stdout is not a terminal"
    );
    Ok((size.ws_col, size.ws_row))
}

/// Paints wire frames onto the real terminal with plain ANSI, rendering from
/// the shared [`WireScreen`] reconstruction.
struct Painter {
    screen: WireScreen,
}

impl Default for Painter {
    fn default() -> Self {
        Self {
            // The alternate screen has no scrollback to put history in.
            screen: WireScreen::new(0),
        }
    }
}

impl Painter {
    fn apply(&mut self, frame: TermServerFrame) -> Result<()> {
        let mut out = String::new();
        match self.screen.apply(frame) {
            FrameApplied::Snapshot => {
                out.push_str("\x1b[2J");
                for (row, cells) in self.screen.rows.iter().enumerate() {
                    paint_row(&mut out, row, cells);
                }
            }
            FrameApplied::Rows(rows) => {
                for row in rows {
                    paint_row(&mut out, row as usize, &self.screen.rows[row as usize]);
                }
            }
            FrameApplied::Title => {
                out.push_str("\x1b]0;");
                out.push_str(&self.screen.title);
                out.push('\x07');
            }
            FrameApplied::History => {}
            FrameApplied::Exited { .. } => unreachable!("handled by the attach loop"),
        }
        place_cursor(&mut out, self.screen.cursor);
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(out.as_bytes())?;
        stdout.flush()?;
        Ok(())
    }
}

fn place_cursor(out: &mut String, cursor: TermCursor) {
    out.push_str(&format!(
        "\x1b[{};{}H",
        cursor.row as usize + 1,
        cursor.col as usize + 1
    ));
    out.push_str(match cursor.shape {
        TermCursorShape::Block => "\x1b[2 q",
        TermCursorShape::Underline => "\x1b[4 q",
        TermCursorShape::Beam => "\x1b[6 q",
    });
    out.push_str(if cursor.visible {
        "\x1b[?25h"
    } else {
        "\x1b[?25l"
    });
}

fn paint_row(out: &mut String, row: usize, cells: &TermRow) {
    out.push_str(&format!("\x1b[{};1H", row + 1));
    let mut last_sgr = String::new();
    for cell in &cells.cells {
        if cell.flags & TermCellFlags::WIDE_SPACER != 0 {
            continue;
        }
        let sgr = cell_sgr(cell);
        if sgr != last_sgr {
            out.push_str(&sgr);
            last_sgr = sgr;
        }
        out.push(cell.c);
        if let Some(extra) = &cell.extra {
            out.push_str(extra);
        }
    }
    // Reset before clearing so the row tail is default-background.
    out.push_str("\x1b[0m\x1b[K");
}

fn cell_sgr(cell: &TermCell) -> String {
    let mut sgr = String::from("\x1b[0");
    let flags = [
        (TermCellFlags::BOLD, "1"),
        (TermCellFlags::DIM, "2"),
        (TermCellFlags::ITALIC, "3"),
        (TermCellFlags::UNDERLINE, "4"),
        (TermCellFlags::INVERSE, "7"),
        (TermCellFlags::HIDDEN, "8"),
        (TermCellFlags::STRIKEOUT, "9"),
    ];
    for (flag, code) in flags {
        if cell.flags & flag != 0 {
            sgr.push(';');
            sgr.push_str(code);
        }
    }
    match cell.fg {
        TermColor::Foreground | TermColor::Background => sgr.push_str(";39"),
        TermColor::Indexed(index @ 0..=7) => sgr.push_str(&format!(";{}", 30 + index as u16)),
        TermColor::Indexed(index @ 8..=15) => sgr.push_str(&format!(";{}", 82 + index as u16)),
        TermColor::Indexed(index) => sgr.push_str(&format!(";38;5;{index}")),
        TermColor::Rgb(r, g, b) => sgr.push_str(&format!(";38;2;{r};{g};{b}")),
    }
    match cell.bg {
        TermColor::Foreground | TermColor::Background => sgr.push_str(";49"),
        TermColor::Indexed(index @ 0..=7) => sgr.push_str(&format!(";{}", 40 + index as u16)),
        TermColor::Indexed(index @ 8..=15) => sgr.push_str(&format!(";{}", 92 + index as u16)),
        TermColor::Indexed(index) => sgr.push_str(&format!(";48;5;{index}")),
        TermColor::Rgb(r, g, b) => sgr.push_str(&format!(";48;2;{r};{g};{b}")),
    }
    sgr.push('m');
    sgr
}
