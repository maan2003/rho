//! Isolated Claude Code subscription-usage polling.
//!
//! The crate owns the hardened PTY probe, Claude `/usage` interaction, terminal
//! emulation, parsing, refresh cadence, and retry backoff. Consumers receive
//! parsed snapshots and remain responsible for persistence and presentation.

use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use anyhow::Context as _;
use chrono::{DateTime, Datelike as _, NaiveDateTime, TimeZone as _, Utc};
use tokio::io::unix::AsyncFd;

const COLS: u16 = 100;
const ROWS: u16 = 48;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageWindow {
    pub used_percent: u8,
    pub reset_at_unix: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaudeUsage {
    pub all_models: UsageWindow,
    pub fable: UsageWindow,
}

#[derive(Clone, Default)]
struct EventSink(Arc<Mutex<Vec<Event>>>);

impl EventListener for EventSink {
    fn send_event(&self, event: Event) {
        self.0.lock().expect("terminal event lock").push(event);
    }
}

struct ProbeScreen {
    term: Term<EventSink>,
    parser: Processor<StdSyncHandler>,
    events: EventSink,
    replies: Vec<u8>,
}

impl ProbeScreen {
    fn new() -> Self {
        let events = EventSink::default();
        Self {
            term: Term::new(
                TermConfig::default(),
                &TermSize::new(COLS as usize, ROWS as usize),
                events.clone(),
            ),
            parser: Processor::new(),
            events,
            replies: Vec::new(),
        }
    }

    fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        for event in std::mem::take(&mut *self.events.0.lock().expect("terminal event lock")) {
            match event {
                Event::PtyWrite(text) => self.replies.extend_from_slice(text.as_bytes()),
                Event::ColorRequest(index, format) => {
                    let color = self.term.colors()[index].unwrap_or_default();
                    self.replies.extend_from_slice(format(color).as_bytes());
                }
                Event::TextAreaSizeRequest(format) => self.replies.extend_from_slice(
                    format(WindowSize {
                        num_lines: ROWS,
                        num_cols: COLS,
                        cell_width: 8,
                        cell_height: 16,
                    })
                    .as_bytes(),
                ),
                _ => {}
            }
        }
    }

    fn text(&self) -> String {
        let grid = self.term.grid();
        (0..grid.screen_lines())
            .map(|line| {
                let row = &grid[Line(line as i32)];
                let mut text = String::new();
                for column in 0..grid.columns().min(row.len()) {
                    let cell = &row[Column(column)];
                    if !cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                        text.push(cell.c);
                    }
                }
                text.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

async fn probe_usage(
    mut command: tokio::process::Command,
    probe_dir: &Path,
) -> anyhow::Result<ClaudeUsage> {
    prepare_probe_dir(probe_dir)?;

    let window_size = rustix_openpty::rustix::termios::Winsize {
        ws_row: ROWS,
        ws_col: COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = rustix_openpty::openpty(None, Some(&window_size)).context("open Claude quota PTY")?;
    set_nonblocking(&pty.controller)?;
    let master = AsyncFd::new(pty.controller).context("register Claude quota PTY")?;
    let slave = pty.user;

    command
        .arg("--safe-mode")
        .arg("--permission-mode")
        .arg("dontAsk")
        .arg("--tools")
        .arg("")
        .arg("--allowed-tools")
        .arg("")
        .current_dir(probe_dir)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env("CLAUDE_CODE_SKIP_PROMPT_HISTORY", "1");
    command.stdin(std::process::Stdio::from(slave.try_clone()?));
    command.stdout(std::process::Stdio::from(slave.try_clone()?));
    command.stderr(std::process::Stdio::from(slave));
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
    let mut child = command.spawn().context("spawn Claude quota probe")?;
    let process_group = child.id().map(|id| id as i32);

    let timed_result = tokio::time::timeout(PROBE_TIMEOUT, async {
        let mut screen = ProbeScreen::new();
        let mut output = Vec::new();
        let mut accepted_probe_dir = false;
        let mut usage_sent = false;
        loop {
            tokio::select! {
                ready = master.readable() => {
                    let mut guard = ready.context("wait for Claude quota output")?;
                    let mut buf = [0u8; 16 * 1024];
                    for _ in 0..16 {
                        match guard.try_io(|inner| read_fd(inner.get_ref(), &mut buf)) {
                            Ok(Ok(0)) => anyhow::bail!("Claude quota probe exited before producing usage"),
                            Ok(Ok(n)) => screen.advance(&buf[..n]),
                            Ok(Err(error)) => return Err(error.into()),
                            Err(_) => break,
                        }
                    }
                }
                ready = master.writable(), if !output.is_empty() || !screen.replies.is_empty() => {
                    output.append(&mut screen.replies);
                    let mut guard = ready.context("wait to write Claude quota input")?;
                    write_fd(&mut guard, &mut output)?;
                }
                status = child.wait() => {
                    anyhow::bail!("Claude quota probe exited early: {}", status?);
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }

            let text = screen.text();
            if !accepted_probe_dir && text.contains("Quick safety check") {
                accepted_probe_dir = true;
                output.extend_from_slice(b"\r");
            }
            if !usage_sent
                && text.contains("Safe mode: all customizations are disabled")
                && text.contains('❯')
            {
                usage_sent = true;
                output.extend_from_slice(b"/usage\r");
            }
            if usage_sent {
                let usage = parse_usage_screen(&text, Utc::now());
                if let Some(usage) = usage {
                    break Ok(usage);
                }
            }
        }
    })
    .await;

    terminate_process_group(process_group, &mut child).await;
    timed_result.context("Claude quota probe timed out")?
}

fn prepare_probe_dir(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                "Claude quota probe path is not a real directory: {}",
                path.display()
            );
            anyhow::ensure!(
                std::fs::read_dir(path)
                    .with_context(|| format!(
                        "read Claude quota probe directory {}",
                        path.display()
                    ))?
                    .next()
                    .is_none(),
                "Claude quota probe directory is not empty: {}",
                path.display()
            );
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).with_context(
                || format!("secure Claude quota probe directory {}", path.display()),
            )?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt as _;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .with_context(|| {
                    format!("create Claude quota probe directory {}", path.display())
                })?;
        }
        Err(error) => return Err(error).context("inspect Claude quota probe directory"),
    }
    Ok(())
}

fn parse_usage_screen(text: &str, now: DateTime<Utc>) -> Option<ClaudeUsage> {
    let lines = text
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>();
    let all_models = parse_window(&lines, "Current week (all models)", now)?;
    let fable = parse_window(&lines, "Current week (Fable)", now)?;
    Some(ClaudeUsage { all_models, fable })
}

fn parse_window(lines: &[String], heading: &str, now: DateTime<Utc>) -> Option<UsageWindow> {
    let start = lines.iter().position(|line| line == heading)? + 1;
    let section = lines[start..]
        .iter()
        .take_while(|line| !line.starts_with("Current "))
        .take(8)
        .collect::<Vec<_>>();
    let used_percent = section.iter().find_map(|line| {
        let percent = line.split_whitespace().find(|word| word.ends_with('%'))?;
        percent
            .trim_end_matches('%')
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(0.0, 100.0).round() as u8)
    })?;
    let reset = section
        .iter()
        .find_map(|line| line.strip_prefix("Resets "))?;
    Some(UsageWindow {
        used_percent,
        reset_at_unix: parse_reset(reset, now)?,
    })
}

fn parse_reset(reset: &str, now: DateTime<Utc>) -> Option<i64> {
    let reset = reset
        .strip_suffix(" (UTC)")
        .unwrap_or(reset)
        .trim()
        .to_ascii_uppercase();
    let (date, time) = reset.rsplit_once(' ')?;
    let meridiem = time.len().checked_sub(2)?;
    let (clock, meridiem) = time.split_at(meridiem);
    let time = match clock.matches(':').count() {
        0 => format!("{clock}:00:00{meridiem}"),
        1 => format!("{clock}:00{meridiem}"),
        2 => time.to_owned(),
        _ => return None,
    };
    [now.year() - 1, now.year(), now.year() + 1]
        .into_iter()
        .filter_map(|year| {
            let value = format!("{year} {date} {time}");
            let naive = NaiveDateTime::parse_from_str(&value, "%Y %b %e, %I:%M:%S%p").ok()?;
            Some(Utc.from_utc_datetime(&naive))
        })
        .min_by_key(|candidate| candidate.timestamp().abs_diff(now.timestamp()))
        .map(|candidate| candidate.timestamp())
}

fn set_nonblocking(fd: &OwnedFd) -> anyhow::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    anyhow::ensure!(flags >= 0, "F_GETFL failed for Claude quota PTY");
    let result = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    anyhow::ensure!(result >= 0, "F_SETFL failed for Claude quota PTY");
    Ok(())
}

fn read_fd(fd: &OwnedFd, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn write_fd(
    guard: &mut tokio::io::unix::AsyncFdReadyGuard<'_, OwnedFd>,
    output: &mut Vec<u8>,
) -> std::io::Result<()> {
    while !output.is_empty() {
        match guard.try_io(|inner| {
            let n = unsafe {
                libc::write(
                    inner.get_ref().as_raw_fd(),
                    output.as_ptr().cast(),
                    output.len(),
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(Ok(n)) => {
                output.drain(..n);
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => break,
        }
    }
    Ok(())
}

async fn terminate_process_group(group: Option<i32>, child: &mut tokio::process::Child) {
    if let Some(group) = group {
        unsafe {
            libc::kill(-group, libc::SIGTERM);
        }
    } else {
        let _ = child.start_kill();
    }
    let leader_running = tokio::time::timeout(Duration::from_secs(1), child.wait())
        .await
        .is_err();
    // Kill the process group even if its leader exited promptly: a descendant
    // may have ignored SIGTERM while retaining the probe's process group.
    if let Some(group) = group {
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    } else if leader_running {
        let _ = child.start_kill();
    }
    if leader_running {
        let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    }
}

/// Start one centralized Claude usage poller.
///
/// The first probe runs immediately. Successful probes repeat every five
/// minutes; failures retry with exponential backoff from one to fifteen
/// minutes. Dropping the receiver stops the task after its current probe.
pub fn spawn_poller<F>(
    mut command: F,
    probe_dir: PathBuf,
) -> tokio::sync::mpsc::Receiver<anyhow::Result<ClaudeUsage>>
where
    F: FnMut() -> anyhow::Result<tokio::process::Command> + Send + 'static,
{
    const REFRESH: Duration = Duration::from_secs(5 * 60);
    const INITIAL_RETRY: Duration = Duration::from_secs(60);
    const MAX_RETRY: Duration = Duration::from_secs(15 * 60);

    let (updates, receiver) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let mut retry = INITIAL_RETRY;
        loop {
            let result = match command() {
                Ok(command) => probe_usage(command, &probe_dir).await,
                Err(error) => Err(error),
            };
            let (update, delay) = match result {
                Ok(usage) => {
                    retry = INITIAL_RETRY;
                    (Ok(usage), REFRESH)
                }
                Err(error) => {
                    let delay = retry;
                    retry = retry.saturating_mul(2).min(MAX_RETRY);
                    (Err(error), delay)
                }
            };
            if updates.send(update).await.is_err() {
                break;
            }
            tokio::time::sleep(delay).await;
        }
    });
    receiver
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    #[test]
    fn probe_directory_must_be_empty_and_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let probe = root.path().join("probe");
        prepare_probe_dir(&probe).unwrap();
        assert_eq!(
            std::fs::metadata(&probe).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::write(probe.join("unexpected"), b"data").unwrap();
        assert!(prepare_probe_dir(&probe).is_err());
    }

    #[test]
    fn terminal_screen_handles_fragmented_redraws() {
        let mut screen = ProbeScreen::new();
        let redraw = b"\x1b[2J\x1b[HCurrent week (all models)\r\n7% used\r\nResets Aug 4, 5pm (UTC)\r\n\r\nCurrent week (Fable)\r\n12% used\r\nResets Aug 4, 4:59pm (UTC)";
        for chunk in redraw.chunks(3) {
            screen.advance(chunk);
        }
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).unwrap();
        let usage = parse_usage_screen(&screen.text(), now).unwrap();
        assert_eq!(usage.all_models.used_percent, 7);
        assert_eq!(usage.fable.used_percent, 12);
    }

    #[test]
    fn reset_uses_nearest_calendar_occurrence() {
        let same_minute = Utc.with_ymd_and_hms(2026, 8, 4, 17, 0, 30).unwrap();
        assert_eq!(
            parse_reset("Aug 4, 5pm (UTC)", same_minute),
            Some(
                Utc.with_ymd_and_hms(2026, 8, 4, 17, 0, 0)
                    .unwrap()
                    .timestamp()
            )
        );

        let december = Utc.with_ymd_and_hms(2026, 12, 30, 12, 0, 0).unwrap();
        assert_eq!(
            parse_reset("Jan 2, 5pm (UTC)", december),
            Some(
                Utc.with_ymd_and_hms(2027, 1, 2, 17, 0, 0)
                    .unwrap()
                    .timestamp()
            )
        );
    }

    #[test]
    fn parses_claude_usage_panel() {
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).unwrap();
        let usage = parse_usage_screen(
            "Current week (all models)\n███ 7% used\nResets Aug 4, 5pm (UTC)\n\nCurrent week (Fable)\n████ 12% used\nResets Aug 4, 5pm (UTC)",
            now,
        )
        .unwrap();
        assert_eq!(usage.all_models.used_percent, 7);
        assert_eq!(usage.fable.used_percent, 12);
        assert_eq!(
            usage.all_models.reset_at_unix,
            Utc.with_ymd_and_hms(2026, 8, 4, 17, 0, 0)
                .unwrap()
                .timestamp()
        );
    }
}
