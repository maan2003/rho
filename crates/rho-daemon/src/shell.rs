//! Daemon-owned Comint-style shell sessions.
//!
//! Each agent has at most one `rho-shell` process. A sideband protocol carries
//! complete commands and authoritative execution/output boundaries. Each
//! execution uses a fresh PTY; programs requiring a persistent controlling
//! terminal belong in the raw terminal. The daemon owns the bounded canonical
//! structured state. Attached clients keep editable pending input locally.
//! Closing a client only detaches it; the shell remains alive until explicitly
//! closed, it exits, or the daemon stops.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::ops::Range;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alacritty_terminal::vte::{self, Params, Perform};
use anyhow::Context as _;
use rho_shell_proto::{
    MAX_ACTIVE_PAGERS, MAX_PAGER_BYTES, MAX_PAGER_LINES, MAX_PROMPT_BYTES, PROTOCOL_VERSION,
    PagerAction, Request, Response,
};
use rho_ui_proto::AgentId;
use rho_ui_proto::shell::{
    MAX_STYLE_SPANS, ShellColor, ShellServerFrame, ShellStyleSpan, ShellTextStyle,
};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot, watch};

const CLIENT_QUEUE: usize = 32;
pub(crate) const SUBMIT_QUEUE: usize = 8;
const CONTROL_QUEUE: usize = 8;
const SIDECAR_QUEUE: usize = 64;
const TICK: std::time::Duration = std::time::Duration::from_millis(16);
const SHELL_STATE_CAP: usize = 4 * 1024 * 1024;
const OUTPUT_TRIM_TO: usize = 2 * 1024 * 1024;
const STYLE_SPAN_COST: usize = 64;
const OMITTED: &str = "[... older shell output omitted ...]\n";
const SHELL_COLS: u16 = 80;
const SHELL_ROWS: u16 = 24;

pub fn ensure_supported_workdirs(workdirs: &[rho_workspaces::WorkspaceInfo]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !workdirs
            .iter()
            .any(|workdir| matches!(workdir, rho_workspaces::WorkspaceInfo::Sandbox { .. })),
        "sandboxed agents have no editor shells yet"
    );
    Ok(())
}

pub struct ShellSpawn {
    pub view: Arc<rho_workspaces::View>,
    /// Shell sidecar launched through the agent View.
    pub program: OsString,
    pub args: Vec<OsString>,
    pub pager_program: OsString,
}

pub struct ShellClient {
    pub frames: mpsc::Receiver<ShellServerFrame>,
    pub exit: watch::Receiver<Option<Arc<ShellExit>>>,
    pub submit: ShellSubmitter,
    pub control: ShellControls,
}

#[derive(Clone)]
pub struct ShellSubmitter {
    tx: mpsc::Sender<QueuedSubmission>,
    next_execution: Arc<std::sync::Mutex<u64>>,
}

pub enum ShellSubmitError {
    Full,
    Closed,
    Exhausted,
    TooLarge,
}

impl ShellSubmitter {
    pub fn try_send(&self, command: String) -> Result<u64, ShellSubmitError> {
        if !rho_ui_proto::shell::command_fits(&command) {
            return Err(ShellSubmitError::TooLarge);
        }
        let mut next = self.next_execution.lock().unwrap();
        let permit = self.tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => ShellSubmitError::Full,
            mpsc::error::TrySendError::Closed(()) => ShellSubmitError::Closed,
        })?;
        let execution = *next;
        *next = next.checked_add(1).ok_or(ShellSubmitError::Exhausted)?;
        permit.send(QueuedSubmission { execution, command });
        Ok(execution)
    }

    #[cfg(test)]
    async fn send(&self, command: String) -> Result<u64, ()> {
        if !rho_ui_proto::shell::command_fits(&command) {
            return Err(());
        }
        let permit = self.tx.reserve().await.map_err(|_| ())?;
        let mut next = self.next_execution.lock().unwrap();
        let execution = *next;
        *next = next.checked_add(1).ok_or(())?;
        permit.send(QueuedSubmission { execution, command });
        Ok(execution)
    }
}

struct QueuedSubmission {
    execution: u64,
    command: String,
}

#[derive(Clone)]
pub struct ShellExit {
    pub state: rho_ui_proto::shell::ShellState,
    pub status: Option<i32>,
}

#[derive(Clone, Copy)]
pub enum ShellControl {
    Interrupt,
    Eof,
    Pager {
        pager: u64,
        page: u64,
        action: PagerAction,
    },
}

#[derive(Clone)]
pub struct ShellControls {
    tx: mpsc::Sender<ScopedShellControl>,
    active_execution: Arc<AtomicU64>,
}

impl ShellControls {
    pub async fn send(&self, control: ShellControl) -> Result<(), ()> {
        let execution = self.active_execution.load(Ordering::Acquire);
        if execution == 0 {
            return Ok(());
        }
        self.tx
            .send(ScopedShellControl { execution, control })
            .await
            .map_err(|_| ())
    }

    pub async fn pager_action(
        &self,
        execution: u64,
        pager: u64,
        page: u64,
        action: PagerAction,
    ) -> Result<(), ()> {
        self.tx
            .send(ScopedShellControl {
                execution,
                control: ShellControl::Pager {
                    pager,
                    page,
                    action,
                },
            })
            .await
            .map_err(|_| ())
    }
}

struct ScopedShellControl {
    execution: u64,
    control: ShellControl,
}

#[derive(Default)]
pub struct ShellRegistry {
    sessions: Mutex<HashMap<AgentId, SessionHandle>>,
}

#[derive(Clone)]
struct SessionHandle {
    cmds: mpsc::UnboundedSender<SessionCmd>,
}

enum SessionCmd {
    Attach { reply: oneshot::Sender<ShellClient> },
    Describe { reply: oneshot::Sender<ShellEntry> },
    Close { reply: oneshot::Sender<()> },
}

/// A running shell's identity and current attachment count.
pub struct ShellEntry {
    pub agent_id: AgentId,
    pub clients: usize,
}

impl ShellRegistry {
    /// Starts an agent shell detached; refuses if one is running.
    pub async fn start(
        self: &Arc<Self>,
        agent_id: AgentId,
        spawn: ShellSpawn,
    ) -> anyhow::Result<()> {
        {
            let mut sessions = self.sessions.lock().await;
            anyhow::ensure!(
                !sessions.contains_key(&agent_id),
                "shell is already running (attach instead)"
            );
            let handle = Session::spawn(Arc::clone(self), agent_id, spawn).await?;
            sessions.insert(agent_id, handle);
        }
        Ok(())
    }

    /// Attaches to an already-running agent shell.
    pub async fn attach(&self, agent_id: AgentId) -> anyhow::Result<ShellClient> {
        loop {
            let handle = self
                .sessions
                .lock()
                .await
                .get(&agent_id)
                .cloned()
                .context("shell is not running")?;
            if let Some(client) = attach_handle(&handle).await {
                return Ok(client);
            }
            self.forget(agent_id, &handle.cmds).await;
        }
    }

    /// Returns every running shell in stable agent-id order.
    pub async fn list(&self) -> Vec<ShellEntry> {
        let handles = {
            let sessions = self.sessions.lock().await;
            let mut keyed = sessions.iter().collect::<Vec<_>>();
            keyed.sort_by_key(|(agent_id, _)| **agent_id);
            keyed
                .into_iter()
                .map(|(_, handle)| handle.clone())
                .collect::<Vec<_>>()
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

    /// Gracefully stops an agent shell and waits for session cleanup.
    pub async fn close(&self, agent_id: AgentId) -> anyhow::Result<()> {
        let handle = self
            .sessions
            .lock()
            .await
            .get(&agent_id)
            .cloned()
            .context("shell is not running")?;
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .cmds
            .send(SessionCmd::Close { reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("shell exited while closing"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("shell exited while closing"))
    }

    async fn forget(&self, agent_id: AgentId, cmds: &mpsc::UnboundedSender<SessionCmd>) {
        let mut sessions = self.sessions.lock().await;
        if let Some(current) = sessions.get(&agent_id)
            && current.cmds.same_channel(cmds)
        {
            sessions.remove(&agent_id);
        }
    }
}

async fn attach_handle(handle: &SessionHandle) -> Option<ShellClient> {
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .cmds
        .send(SessionCmd::Attach { reply: reply_tx })
        .ok()?;
    reply_rx.await.ok()
}

struct ClientState {
    frames: mpsc::Sender<ShellServerFrame>,
    /// A full structured snapshot must precede any more deltas.
    needs_snapshot: bool,
}

struct State {
    shell: rho_ui_proto::shell::ShellState,
    execution_outputs: HashMap<u64, PlainOutput>,
    terminal_output: PlainOutput,
    clients: Vec<ClientState>,
    submit: ShellSubmitter,
    control: ShellControls,
    exit_tx: watch::Sender<Option<Arc<ShellExit>>>,
}

impl State {
    fn attach(&mut self) -> ShellClient {
        let (frames_tx, frames_rx) = mpsc::channel(CLIENT_QUEUE);
        self.clients.push(ClientState {
            frames: frames_tx,
            needs_snapshot: true,
        });
        ShellClient {
            frames: frames_rx,
            exit: self.exit_tx.subscribe(),
            submit: self.submit.clone(),
            control: self.control.clone(),
        }
    }

    fn client_count(&mut self) -> usize {
        self.clients.retain(|client| !client.frames.is_closed());
        self.clients.len()
    }

    fn send_state_frame(&mut self, frame: ShellServerFrame) {
        self.clients.retain(|client| !client.frames.is_closed());
        for client in &mut self.clients {
            if client.needs_snapshot {
                continue;
            }
            match client.frames.try_send(frame.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => client.needs_snapshot = true,
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    fn set_prompt(&mut self, prompt: &[u8]) {
        let mut prompt = plain_text(prompt);
        if prompt.len() > MAX_PROMPT_BYTES {
            let mut start = prompt.len() - MAX_PROMPT_BYTES;
            while !prompt.is_char_boundary(start) {
                start += 1;
            }
            prompt.drain(..start);
        }
        if prompt.is_empty() {
            prompt.push_str("> ");
        }
        if self.shell.prompt != prompt {
            self.shell.prompt = prompt.clone();
            self.send_state_frame(ShellServerFrame::Prompt {
                prompt,
                cwd: self.shell.cwd.clone(),
            });
        }
    }

    fn set_cwd(&mut self, cwd: String) {
        let cwd = plain_text(cwd.as_bytes());
        if self.shell.cwd != cwd {
            self.shell.cwd = cwd.clone();
            self.send_state_frame(ShellServerFrame::Prompt {
                prompt: self.shell.prompt.clone(),
                cwd,
            });
        }
    }

    fn queued(&mut self, execution: u64, command: String) {
        self.execution_outputs
            .insert(execution, PlainOutput::default());
        let block = rho_ui_proto::shell::ShellExecution {
            execution,
            command,
            prompt: String::new(),
            cwd: String::new(),
            state: rho_ui_proto::shell::ShellExecutionState::Queued,
            output: String::new(),
            styles: Vec::new(),
        };
        self.shell.executions.push(block.clone());
        self.send_state_frame(ShellServerFrame::ExecutionQueued { execution: block });
        self.trim_shell_state();
    }

    fn started(&mut self, execution: u64) {
        let Some(block) = self
            .shell
            .executions
            .iter_mut()
            .find(|block| block.execution == execution)
        else {
            return;
        };
        block.state = rho_ui_proto::shell::ShellExecutionState::Running;
        let prompt = self.shell.prompt.clone();
        let cwd = self.shell.cwd.clone();
        block.prompt.clone_from(&prompt);
        block.cwd.clone_from(&cwd);
        self.send_state_frame(ShellServerFrame::ExecutionStarted {
            execution,
            prompt,
            cwd,
        });
    }

    fn execution_output(&mut self, execution: u64, bytes: &[u8]) {
        let Some((start, end, text, styles, all_styles)) =
            self.execution_outputs.get_mut(&execution).map(|output| {
                let end = output.text.len();
                let start = output.line_start;
                output.advance(bytes);
                let start = if output.trim() { 0 } else { start };
                (
                    start,
                    end,
                    output.text[start..].to_owned(),
                    output.styles_from(start),
                    output.styles.clone(),
                )
            })
        else {
            self.terminal_output(bytes);
            return;
        };
        if let Some(block) = self
            .shell
            .executions
            .iter_mut()
            .find(|block| block.execution == execution)
        {
            block.output.replace_range(start..end, &text);
            block.styles = all_styles;
        }
        self.send_state_frame(ShellServerFrame::ExecutionOutput {
            execution,
            start: start as u64,
            end: end as u64,
            text,
            styles,
        });
        self.trim_shell_state();
    }

    fn pager_paused(&mut self, execution: u64, pager: u64, page: u64, lines: u32, bytes: u64) {
        let state = rho_ui_proto::shell::ShellPager {
            execution,
            pager,
            page,
            lines,
            bytes,
        };
        if let Some(existing) = self
            .shell
            .pagers
            .iter_mut()
            .find(|item| item.execution == execution && item.pager == pager)
        {
            *existing = state.clone();
        } else {
            self.shell.pagers.push(state.clone());
        }
        self.send_state_frame(ShellServerFrame::PagerPaused {
            execution,
            pager: state,
        });
    }

    fn pager_resumed(&mut self, execution: u64, pager: u64) {
        self.remove_pager(execution, pager);
        self.send_state_frame(ShellServerFrame::PagerResumed { execution, pager });
    }

    fn pager_finished(&mut self, execution: u64, pager: u64) {
        self.remove_pager(execution, pager);
        self.send_state_frame(ShellServerFrame::PagerFinished { execution, pager });
        self.trim_shell_state();
    }

    fn remove_pager(&mut self, execution: u64, pager: u64) {
        self.shell
            .pagers
            .retain(|item| item.execution != execution || item.pager != pager);
    }

    fn pager_is_paused(&self, execution: u64, pager: u64, page: u64) -> bool {
        self.shell
            .pagers
            .iter()
            .any(|item| item.execution == execution && item.pager == pager && item.page == page)
    }

    fn execution_finished(&mut self, execution: u64, status: i32) {
        if let Some(block) = self
            .shell
            .executions
            .iter_mut()
            .find(|block| block.execution == execution)
        {
            block.state = rho_ui_proto::shell::ShellExecutionState::Finished { status };
        }
        self.send_state_frame(ShellServerFrame::ExecutionFinished { execution, status });
        self.trim_shell_state();
    }

    fn execution_failed(&mut self, execution: Option<u64>) {
        if let Some(block) = execution.and_then(|execution| {
            self.shell
                .executions
                .iter_mut()
                .find(|block| block.execution == execution)
        }) {
            block.state = rho_ui_proto::shell::ShellExecutionState::Failed;
        }
        self.send_state_frame(ShellServerFrame::ExecutionFailed { execution });
        self.trim_shell_state();
    }

    fn execution_cancelled(&mut self, execution: u64) {
        if let Some(block) = self
            .shell
            .executions
            .iter_mut()
            .find(|block| block.execution == execution)
        {
            block.state = rho_ui_proto::shell::ShellExecutionState::Cancelled;
        }
        for client in &mut self.clients {
            client.needs_snapshot = true;
        }
    }

    fn terminal_output(&mut self, bytes: &[u8]) {
        let end = self.terminal_output.text.len();
        let start = self.terminal_output.line_start;
        self.terminal_output.advance(bytes);
        let start = if self.terminal_output.trim() {
            0
        } else {
            start
        };
        let text = self.terminal_output.text[start..].to_owned();
        let styles = self.terminal_output.styles_from(start);
        self.shell.terminal_output.replace_range(start..end, &text);
        self.shell
            .terminal_styles
            .clone_from(&self.terminal_output.styles);
        self.send_state_frame(ShellServerFrame::TerminalOutput {
            start: start as u64,
            end: end as u64,
            text,
            styles,
        });
    }

    fn trim_shell_state(&mut self) {
        let retained_bytes = |block: &rho_ui_proto::shell::ShellExecution| {
            block.command.len()
                + block.prompt.len()
                + block.cwd.len()
                + block.output.len()
                + block.styles.len() * STYLE_SPAN_COST
        };
        while self
            .shell
            .executions
            .iter()
            .map(retained_bytes)
            .sum::<usize>()
            > SHELL_STATE_CAP
        {
            let Some(index) = self.shell.executions.iter().position(|block| {
                matches!(
                    block.state,
                    rho_ui_proto::shell::ShellExecutionState::Finished { .. }
                        | rho_ui_proto::shell::ShellExecutionState::Failed
                        | rho_ui_proto::shell::ShellExecutionState::Cancelled
                )
            }) else {
                break;
            };
            let removed = self.shell.executions.remove(index);
            self.execution_outputs.remove(&removed.execution);
            for client in &mut self.clients {
                client.needs_snapshot = true;
            }
        }
    }

    fn needs_sync(&self) -> bool {
        self.clients.iter().any(|client| client.needs_snapshot)
    }

    fn sync(&mut self) {
        self.clients.retain(|client| !client.frames.is_closed());
        for client in &mut self.clients {
            if !client.needs_snapshot {
                continue;
            }
            match client.frames.try_send(ShellServerFrame::Snapshot {
                state: self.shell.clone(),
            }) {
                Ok(()) => client.needs_snapshot = false,
                Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    /// Publishes one canonical final state. Stream writers prioritize this
    /// watch value over queued incremental frames, so output congestion can
    /// coalesce but cannot hide process exit.
    fn finish(&mut self, status: Option<i32>) {
        self.shell.pagers.clear();
        self.exit_tx.send_replace(Some(Arc::new(ShellExit {
            state: self.shell.clone(),
            status,
        })));
    }
}

/// Safe output accumulator. SGR is retained as bounded structured spans; all
/// other escape sequences are discarded, while carriage return, backspace,
/// and erase-line rewrite only the active line.
struct PlainOutput {
    text: String,
    styles: Vec<ShellStyleSpan>,
    current_style: ShellTextStyle,
    cursor: usize,
    line_start: usize,
    parser: vte::Parser,
}

impl Default for PlainOutput {
    fn default() -> Self {
        Self {
            text: String::new(),
            styles: Vec::new(),
            current_style: ShellTextStyle::default(),
            cursor: 0,
            line_start: 0,
            parser: vte::Parser::new(),
        }
    }
}

impl PlainOutput {
    fn advance(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::replace(&mut self.parser, vte::Parser::new());
        parser.advance(self, bytes);
        self.parser = parser;
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |offset| self.cursor + offset)
    }

    fn styles_from(&self, start: usize) -> Vec<ShellStyleSpan> {
        let first = self.styles.partition_point(|span| span.end <= start as u64);
        self.styles
            .iter()
            .skip(first)
            .filter_map(|span| {
                let span_start = usize::try_from(span.start).ok()?;
                let span_end = usize::try_from(span.end).ok()?;
                (span_end > start).then(|| ShellStyleSpan {
                    start: span_start.max(start) as u64,
                    end: span_end as u64,
                    style: span.style,
                })
            })
            .collect()
    }

    fn push_style(&mut self, range: Range<usize>, style: ShellTextStyle) {
        if range.is_empty() || style == ShellTextStyle::default() {
            return;
        }
        if let Some(last) = self.styles.last_mut()
            && last.end == range.start as u64
            && last.style == style
        {
            last.end = range.end as u64;
            return;
        }
        self.styles.push(ShellStyleSpan {
            start: range.start as u64,
            end: range.end as u64,
            style,
        });
        if self.styles.len() > MAX_STYLE_SPANS {
            self.styles.truncate(MAX_STYLE_SPANS);
        }
    }

    fn replace_styled(&mut self, range: Range<usize>, replacement: &str, style: ShellTextStyle) {
        let replacement_end = range.start + replacement.len();
        let first = self
            .styles
            .partition_point(|span| span.end <= range.start as u64);
        let affected = self.styles.split_off(first);
        let mut styles = Vec::with_capacity(affected.len() + 2);
        for span in affected {
            let start = span.start as usize;
            let end = span.end as usize;
            if end <= range.start {
                styles.push(span);
            } else if start >= range.end {
                let shifted_start = replacement_end + start.saturating_sub(range.end);
                let shifted_end = replacement_end + end.saturating_sub(range.end);
                styles.push(ShellStyleSpan {
                    start: shifted_start as u64,
                    end: shifted_end as u64,
                    style: span.style,
                });
            } else {
                if start < range.start {
                    styles.push(ShellStyleSpan {
                        start: start as u64,
                        end: range.start as u64,
                        style: span.style,
                    });
                }
                if end > range.end {
                    styles.push(ShellStyleSpan {
                        start: replacement_end as u64,
                        end: (replacement_end + end - range.end) as u64,
                        style: span.style,
                    });
                }
            }
        }
        if !replacement.is_empty() && style != ShellTextStyle::default() {
            styles.push(ShellStyleSpan {
                start: range.start as u64,
                end: replacement_end as u64,
                style,
            });
        }
        styles.sort_unstable_by_key(|span| (span.start, span.end));
        for span in styles {
            if let Some(last) = self.styles.last_mut()
                && last.end == span.start
                && last.style == span.style
            {
                last.end = span.end;
            } else {
                self.styles.push(span);
            }
        }
        if self.styles.len() > MAX_STYLE_SPANS {
            self.styles.truncate(MAX_STYLE_SPANS);
        }
        self.text.replace_range(range, replacement);
    }

    fn trim(&mut self) -> bool {
        if self.text.len() <= SHELL_STATE_CAP {
            return false;
        }
        let mut target = self.text.len().saturating_sub(OUTPUT_TRIM_TO);
        while !self.text.is_char_boundary(target) {
            target += 1;
        }
        let cut = self.text[target..]
            .find('\n')
            .map_or(target, |offset| target + offset + 1);
        self.replace_styled(0..cut, OMITTED, ShellTextStyle::default());
        let delta = cut.saturating_sub(OMITTED.len());
        self.cursor = self.cursor.saturating_sub(delta).min(self.text.len());
        self.line_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        true
    }
}

impl Perform for PlainOutput {
    fn print(&mut self, c: char) {
        let mut bytes = [0; 4];
        let text = c.encode_utf8(&mut bytes);
        if self.cursor < self.text.len() {
            let next = self.cursor
                + self.text[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
            self.replace_styled(self.cursor..next, text, self.current_style);
        } else {
            let start = self.text.len();
            self.text.push(c);
            self.push_style(start..self.text.len(), self.current_style);
        }
        self.cursor += c.len_utf8();
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                let end = self.line_end();
                if self.text.as_bytes().get(end) == Some(&b'\n') {
                    self.cursor = end + 1;
                } else {
                    if end == self.text.len() {
                        self.text.push('\n');
                    } else {
                        self.replace_styled(end..end, "\n", ShellTextStyle::default());
                    }
                    self.cursor = end + 1;
                }
                self.line_start = self.cursor;
            }
            b'\r' => self.cursor = self.line_start,
            0x08 => {
                if self.cursor > self.line_start {
                    self.cursor = self.text[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(self.line_start, |(offset, _)| offset);
                }
            }
            b'\t' => self.print('\t'),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        if action == 'm' {
            let mut params = params.iter();
            while let Some(param) = params.next() {
                let code = param.first().copied().unwrap_or(0);
                match code {
                    0 => self.current_style = ShellTextStyle::default(),
                    1 => self.current_style.bold = true,
                    2 => self.current_style.dim = true,
                    3 => self.current_style.italic = true,
                    4 => self.current_style.underline = param.get(1).copied().unwrap_or(1) != 0,
                    7 => self.current_style.inverse = true,
                    9 => self.current_style.strikethrough = true,
                    21 => self.current_style.bold = false,
                    22 => {
                        self.current_style.bold = false;
                        self.current_style.dim = false;
                    }
                    23 => self.current_style.italic = false,
                    24 => self.current_style.underline = false,
                    27 => self.current_style.inverse = false,
                    29 => self.current_style.strikethrough = false,
                    30..=37 => {
                        self.current_style.foreground =
                            Some(ShellColor::Indexed((code - 30) as u8));
                    }
                    38 => {
                        let color = if param.len() > 1 {
                            colon_sgr_color(&param[1..])
                        } else {
                            semicolon_sgr_color(&mut params)
                        };
                        if let Some(color) = color {
                            self.current_style.foreground = Some(color);
                        }
                    }
                    39 => self.current_style.foreground = None,
                    40..=47 => {
                        self.current_style.background =
                            Some(ShellColor::Indexed((code - 40) as u8));
                    }
                    48 => {
                        let color = if param.len() > 1 {
                            colon_sgr_color(&param[1..])
                        } else {
                            semicolon_sgr_color(&mut params)
                        };
                        if let Some(color) = color {
                            self.current_style.background = Some(color);
                        }
                    }
                    49 => self.current_style.background = None,
                    90..=97 => {
                        self.current_style.foreground =
                            Some(ShellColor::Indexed((code - 90 + 8) as u8));
                    }
                    100..=107 => {
                        self.current_style.background =
                            Some(ShellColor::Indexed((code - 100 + 8) as u8));
                    }
                    _ => {}
                }
            }
            return;
        }
        if action != 'K' {
            return;
        }
        let mode = params
            .iter()
            .next()
            .and_then(|param| param.first())
            .copied()
            .unwrap_or(0);
        let end = self.line_end();
        match mode {
            0 => self.replace_styled(self.cursor..end, "", ShellTextStyle::default()),
            2 => {
                self.replace_styled(self.line_start..end, "", ShellTextStyle::default());
                self.cursor = self.line_start;
            }
            _ => {}
        }
    }
}

fn semicolon_sgr_color<'a>(params: &mut impl Iterator<Item = &'a [u16]>) -> Option<ShellColor> {
    match params.next()?.first().copied()? {
        2 => Some(ShellColor::Rgb {
            red: u8::try_from(params.next()?.first().copied()?).ok()?,
            green: u8::try_from(params.next()?.first().copied()?).ok()?,
            blue: u8::try_from(params.next()?.first().copied()?).ok()?,
        }),
        5 => Some(ShellColor::Indexed(
            u8::try_from(params.next()?.first().copied()?).ok()?,
        )),
        _ => None,
    }
}

fn colon_sgr_color(params: &[u16]) -> Option<ShellColor> {
    match params.first().copied()? {
        2 => {
            let rgb = if params.len() > 4 {
                params.get(2..5)?
            } else {
                params.get(1..4)?
            };
            Some(ShellColor::Rgb {
                red: u8::try_from(rgb[0]).ok()?,
                green: u8::try_from(rgb[1]).ok()?,
                blue: u8::try_from(rgb[2]).ok()?,
            })
        }
        5 => Some(ShellColor::Indexed(u8::try_from(*params.get(1)?).ok()?)),
        _ => None,
    }
}

fn plain_text(bytes: &[u8]) -> String {
    let mut output = PlainOutput::default();
    output.advance(bytes);
    output.text
}

/// Kills the entire shell session on task cancellation. Interactive job
/// control gives foreground/background jobs their own process groups, so
/// `Child::kill_on_drop` and a single `killpg` are insufficient.
struct ProcessSessionGuard {
    id: Option<i32>,
}

impl ProcessSessionGuard {
    fn new(id: Option<i32>) -> Self {
        Self { id }
    }

    fn members(&self) -> Vec<i32> {
        let Some(id) = self.id else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| unsafe { libc::getsid(*pid) } == id)
            .collect()
    }

    fn signal_session(&self, signal: i32) {
        let Some(id) = self.id else {
            return;
        };
        // Fast path/fallback for the shell's own group, then cover every job
        // control group still belonging to the session.
        unsafe {
            libc::kill(-id, signal);
        }
        for pid in self.members() {
            unsafe {
                libc::kill(pid, signal);
            }
        }
    }

    fn exists(&self) -> bool {
        !self.members().is_empty()
    }

    async fn terminate(&mut self) {
        if self.id.is_none() {
            return;
        }
        self.signal_session(libc::SIGTERM);
        for _ in 0..10 {
            if !self.exists() {
                self.id = None;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        self.signal_session(libc::SIGKILL);
        // The signal was sent while this group identity was still observed;
        // disarm now rather than risking a later PID/group-id reuse.
        self.id = None;
    }
}

impl Drop for ProcessSessionGuard {
    fn drop(&mut self) {
        self.signal_session(libc::SIGKILL);
    }
}

struct Session;

impl Session {
    async fn spawn(
        registry: Arc<ShellRegistry>,
        agent_id: AgentId,
        spawn: ShellSpawn,
    ) -> anyhow::Result<SessionHandle> {
        let (parent_control, child_control) =
            StdUnixStream::pair().context("open rho-shell control socket")?;
        parent_control
            .set_nonblocking(true)
            .context("make rho-shell control socket nonblocking")?;
        let control =
            UnixStream::from_std(parent_control).context("register rho-shell control socket")?;

        let mut command = tokio::process::Command::new(&spawn.program);
        command.args(&spawn.args);
        command
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .env("PAGER", &spawn.pager_program)
            .env("GIT_PAGER", &spawn.pager_program)
            .env("COLUMNS", SHELL_COLS.to_string())
            .env("LINES", SHELL_ROWS.to_string());
        spawn
            .view
            .prepare_command(&mut command, None, Vec::new())
            .await?;
        if cfg!(test) {
            command.env("RHO_SHELL_TEST_CHILD", "1");
        }
        command.stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            child_control,
        )));
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());
        // Enter the workspace namespace first, then make the child a session
        // leader. rho-shell remains isolated as a session leader; the daemon retains
        // the session id for generic process cleanup.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.kill_on_drop(true);
        let child = command.spawn().context("spawn rho-shell")?;
        let process_session = ProcessSessionGuard::new(child.id().map(|pid| pid as i32));
        let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
        // The session drains this bounded ingress into its own bounded queue
        // so accepted commands enter canonical ShellState immediately.
        let (submit_tx, submit_rx) = mpsc::channel(SUBMIT_QUEUE);
        let submit = ShellSubmitter {
            tx: submit_tx,
            next_execution: Arc::new(std::sync::Mutex::new(1)),
        };
        let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE);
        let active_execution = Arc::new(AtomicU64::new(0));
        let (exit_tx, _exit_rx) = watch::channel(None);
        let state = State {
            shell: rho_ui_proto::shell::ShellState {
                prompt: "> ".into(),
                cwd: String::new(),
                executions: Vec::new(),
                pagers: Vec::new(),
                terminal_output: String::new(),
                terminal_styles: Vec::new(),
            },
            execution_outputs: HashMap::new(),
            terminal_output: PlainOutput::default(),
            clients: Vec::new(),
            submit,
            control: ShellControls {
                tx: control_tx,
                active_execution: Arc::clone(&active_execution),
            },
            exit_tx,
        };
        tokio::spawn(run_session(
            registry,
            agent_id,
            cmds_tx.clone(),
            cmds_rx,
            submit_rx,
            control_rx,
            active_execution,
            control,
            child,
            process_session,
            state,
        ));
        Ok(SessionHandle { cmds: cmds_tx })
    }
}

struct SidecarProtocol {
    ready: bool,
    exited: bool,
    next_execution: u64,
    pending: VecDeque<u64>,
    current: Option<u64>,
    announced_status: Option<i32>,
    pagers: HashSet<(u64, u64)>,
    paused_pagers: HashSet<(u64, u64)>,
    pager_pages: HashMap<(u64, u64), u64>,
}

enum SidecarEvent {
    Ready {
        prompt: String,
        cwd: String,
    },
    Started {
        execution: u64,
    },
    Output {
        execution: u64,
        data: Vec<u8>,
    },
    PagerStarted,
    PagerPaused {
        execution: u64,
        pager: u64,
        page: u64,
        lines: u32,
        bytes: u64,
    },
    PagerResumed {
        execution: u64,
        pager: u64,
    },
    PagerFinished {
        execution: u64,
        pager: u64,
    },
    Finished {
        execution: u64,
        status: i32,
        prompt: String,
        cwd: String,
    },
    Error {
        execution: Option<u64>,
        message: String,
    },
    Exited,
}

impl SidecarProtocol {
    fn new() -> Self {
        Self {
            ready: false,
            exited: false,
            next_execution: 1,
            pending: VecDeque::new(),
            current: None,
            announced_status: None,
            pagers: HashSet::new(),
            paused_pagers: HashSet::new(),
            pager_pages: HashMap::new(),
        }
    }

    fn can_submit(&self) -> bool {
        self.ready && !self.exited && self.pending.is_empty() && self.current.is_none()
    }

    fn request(&mut self, submission: QueuedSubmission) -> Option<Request> {
        let QueuedSubmission { execution, command } = submission;
        if execution != self.next_execution {
            return None;
        }
        self.next_execution = self.next_execution.checked_add(1)?;
        self.pending.push_back(execution);
        Some(Request::Execute { execution, command })
    }

    /// Validates every sidecar transition. The sidecar is never authoritative
    /// over command identity or execution ordering.
    fn receive(&mut self, response: Response) -> Result<SidecarEvent, ()> {
        if self.exited {
            return Err(());
        }
        match response {
            Response::Ready {
                protocol,
                prompt,
                cwd,
            } if !self.ready
                && protocol == PROTOCOL_VERSION
                && prompt.len() <= MAX_PROMPT_BYTES
                && cwd.len() <= MAX_PROMPT_BYTES =>
            {
                self.ready = true;
                Ok(SidecarEvent::Ready { prompt, cwd })
            }
            Response::Ready { .. } => Err(()),
            _ if !self.ready => Err(()),
            Response::Started { execution } => {
                if self.current.is_some() {
                    return Err(());
                }
                let Some(expected) = self.pending.pop_front() else {
                    return Err(());
                };
                if execution != expected {
                    return Err(());
                }
                self.current = Some(execution);
                Ok(SidecarEvent::Started { execution })
            }
            Response::Output {
                execution, data, ..
            } => {
                let issued = execution > 0 && execution < self.next_execution;
                let not_waiting_to_start =
                    !self.pending.iter().any(|pending| *pending == execution);
                if !issued || !not_waiting_to_start {
                    return Err(());
                }
                Ok(SidecarEvent::Output { execution, data })
            }
            Response::PagerStarted { execution, pager } => {
                let key = (execution, pager);
                let issued = execution > 0 && execution < self.next_execution;
                let not_waiting_to_start =
                    !self.pending.iter().any(|pending| *pending == execution);
                if !issued
                    || !not_waiting_to_start
                    || pager == 0
                    || self.pagers.contains(&key)
                    || self.pagers.len() >= MAX_ACTIVE_PAGERS
                {
                    return Err(());
                }
                self.pagers.insert(key);
                Ok(SidecarEvent::PagerStarted)
            }
            Response::PagerPaused {
                execution,
                pager,
                page,
                lines,
                bytes,
            } => {
                let key = (execution, pager);
                let issued = execution > 0 && execution < self.next_execution;
                let not_waiting_to_start =
                    !self.pending.iter().any(|pending| *pending == execution);
                let last_page = self.pager_pages.get(&key).copied().unwrap_or(0);
                if !issued
                    || !not_waiting_to_start
                    || pager == 0
                    || !self.pagers.contains(&key)
                    || page <= last_page
                    || bytes == 0
                    || bytes > MAX_PAGER_BYTES
                    || lines > MAX_PAGER_LINES
                    || self.paused_pagers.contains(&key)
                {
                    return Err(());
                }
                self.paused_pagers.insert(key);
                self.pager_pages.insert(key, page);
                Ok(SidecarEvent::PagerPaused {
                    execution,
                    pager,
                    page,
                    lines,
                    bytes,
                })
            }
            Response::PagerResumed { execution, pager } => {
                let key = (execution, pager);
                if !self.pagers.contains(&key) || !self.paused_pagers.remove(&key) {
                    return Err(());
                }
                Ok(SidecarEvent::PagerResumed { execution, pager })
            }
            Response::PagerFinished { execution, pager } => {
                let key = (execution, pager);
                if !self.pagers.remove(&key) {
                    return Err(());
                }
                self.paused_pagers.remove(&key);
                self.pager_pages.remove(&key);
                Ok(SidecarEvent::PagerFinished { execution, pager })
            }
            Response::Finished {
                execution,
                status,
                prompt,
                cwd,
            } => {
                if self.current != Some(execution)
                    || prompt.len() > MAX_PROMPT_BYTES
                    || cwd.len() > MAX_PROMPT_BYTES
                {
                    return Err(());
                }
                self.current = None;
                Ok(SidecarEvent::Finished {
                    execution,
                    status,
                    prompt,
                    cwd,
                })
            }
            Response::Error { execution, message } => {
                if let Some(execution) = execution {
                    if self.current == Some(execution) {
                        self.current = None;
                    } else if self.pending.front().copied() == Some(execution) {
                        self.pending.pop_front();
                    } else {
                        return Err(());
                    }
                }
                Ok(SidecarEvent::Error { execution, message })
            }
            Response::Exited { status } if self.current.is_none() && self.pending.is_empty() => {
                self.exited = true;
                self.announced_status = Some(status);
                self.pagers.clear();
                self.paused_pagers.clear();
                self.pager_pages.clear();
                Ok(SidecarEvent::Exited)
            }
            Response::Exited { .. } => Err(()),
        }
    }
}

fn apply_sidecar_event(state: &mut State, active_execution: &AtomicU64, event: SidecarEvent) {
    match event {
        SidecarEvent::Ready { prompt, cwd } => {
            state.set_cwd(cwd);
            state.set_prompt(prompt.as_bytes());
        }
        SidecarEvent::Started { execution } => {
            active_execution.store(execution, Ordering::Release);
            state.started(execution);
        }
        SidecarEvent::Output { execution, data } => state.execution_output(execution, &data),
        SidecarEvent::PagerStarted => {}
        SidecarEvent::PagerPaused {
            execution,
            pager,
            page,
            lines,
            bytes,
        } => state.pager_paused(execution, pager, page, lines, bytes),
        SidecarEvent::PagerResumed { execution, pager } => {
            state.pager_resumed(execution, pager);
        }
        SidecarEvent::PagerFinished { execution, pager } => {
            state.pager_finished(execution, pager);
        }
        SidecarEvent::Finished {
            execution,
            status,
            prompt,
            cwd,
        } => {
            if active_execution.load(Ordering::Acquire) == execution {
                active_execution.store(0, Ordering::Release);
            }
            state.execution_finished(execution, status);
            state.set_cwd(cwd);
            state.set_prompt(prompt.as_bytes());
        }
        SidecarEvent::Error { execution, message } => {
            if execution
                .is_some_and(|execution| active_execution.load(Ordering::Acquire) == execution)
            {
                active_execution.store(0, Ordering::Release);
            }
            state.execution_failed(execution);
            let message = format!("[rho-shell: {message}]\n");
            if let Some(execution) = execution {
                state.execution_output(execution, message.as_bytes());
            } else {
                state.terminal_output(message.as_bytes());
            }
        }
        SidecarEvent::Exited => {
            state.shell.pagers.clear();
        }
    }
}

fn receive_sidecar(
    state: &mut State,
    protocol: &mut SidecarProtocol,
    active_execution: &AtomicU64,
    response: Response,
) -> bool {
    match protocol.receive(response) {
        Ok(event) => {
            apply_sidecar_event(state, active_execution, event);
            true
        }
        Err(()) => {
            state.terminal_output(b"rho-shell protocol violation\n");
            false
        }
    }
}

#[expect(clippy::too_many_arguments)]
async fn run_session(
    registry: Arc<ShellRegistry>,
    agent_id: AgentId,
    _cmds_keepalive: mpsc::UnboundedSender<SessionCmd>,
    mut cmds: mpsc::UnboundedReceiver<SessionCmd>,
    mut submits: mpsc::Receiver<QueuedSubmission>,
    mut controls: mpsc::Receiver<ScopedShellControl>,
    active_execution: Arc<AtomicU64>,
    control: UnixStream,
    mut child: tokio::process::Child,
    mut process_session: ProcessSessionGuard,
    mut state: State,
) {
    let (control_reader, control_writer) = control.into_split();
    let (responses_tx, mut responses) = mpsc::channel(SIDECAR_QUEUE);
    let (requests_tx, requests_rx) = mpsc::channel(SUBMIT_QUEUE);
    let mut response_task = tokio::spawn(read_responses(control_reader, responses_tx));
    let request_task = tokio::spawn(write_requests(control_writer, requests_rx));

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut protocol = SidecarProtocol::new();
    let mut queued_submissions = VecDeque::new();
    let mut closing = false;
    let mut close_replies = Vec::new();
    let startup_timeout = tokio::time::sleep(std::time::Duration::from_secs(10));
    tokio::pin!(startup_timeout);
    let close_timeout = tokio::time::sleep(std::time::Duration::from_secs(365 * 24 * 60 * 60));
    tokio::pin!(close_timeout);

    let status = loop {
        if protocol.can_submit()
            && let Some(submission) = queued_submissions.pop_front()
        {
            let Some(request) = protocol.request(submission) else {
                state.terminal_output(b"rho-shell execution sequence invalid\n");
                break None;
            };
            if requests_tx.try_send(request).is_err() {
                state.terminal_output(b"rho-shell command channel closed\n");
                break None;
            }
            continue;
        }
        tokio::select! {
            Some(cmd) = cmds.recv() => match cmd {
                SessionCmd::Attach { reply } => {
                    let _ = reply.send(state.attach());
                }
                SessionCmd::Describe { reply } => {
                    let _ = reply.send(ShellEntry {
                        agent_id,
                        clients: state.client_count(),
                    });
                }
                SessionCmd::Close { reply } => {
                    close_replies.push(reply);
                    if !closing {
                        closing = true;
                        for submission in queued_submissions.drain(..) {
                            state.execution_cancelled(submission.execution);
                        }
                        while let Ok(submission) = submits.try_recv() {
                            state.queued(submission.execution, submission.command);
                            state.execution_cancelled(submission.execution);
                        }
                        let execution = active_execution.load(Ordering::Acquire);
                        if execution != 0
                            && requests_tx
                                .send(Request::Interrupt { execution })
                                .await
                                .is_err()
                        {
                            break None;
                        }
                        if requests_tx.send(Request::Shutdown).await.is_err() {
                            break None;
                        }
                        close_timeout.as_mut().reset(
                            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                        );
                    }
                }
            },
            Some(request) = controls.recv() => {
                let request = match request.control {
                    ShellControl::Interrupt
                        if active_execution.load(Ordering::Acquire) == request.execution =>
                    {
                        Some(Request::Interrupt {
                            execution: request.execution,
                        })
                    }
                    ShellControl::Eof
                        if active_execution.load(Ordering::Acquire) == request.execution =>
                    {
                        Some(Request::Eof {
                            execution: request.execution,
                        })
                    }
                    ShellControl::Pager {
                        pager,
                        page,
                        action,
                    } if state.pager_is_paused(request.execution, pager, page) =>
                    {
                        Some(Request::PagerAction {
                            execution: request.execution,
                            pager,
                            page,
                            action,
                        })
                    }
                    _ => None,
                };
                if let Some(request) = request
                    && requests_tx.send(request).await.is_err()
                {
                    break None;
                }
            },
            Some(submission) = submits.recv(), if !closing && queued_submissions.len() < SUBMIT_QUEUE => {
                state.queued(submission.execution, submission.command.clone());
                queued_submissions.push_back(submission);
            },
            response = responses.recv() => match response {
                Some(response) => {
                    if !receive_sidecar(
                        &mut state,
                        &mut protocol,
                        &active_execution,
                        response,
                    ) {
                        break None;
                    }
                }
                None => {
                    if protocol.announced_status.is_none() {
                        state.terminal_output(b"rho-shell protocol stream closed unexpectedly\n");
                    }
                    break None;
                }
            },
            _ = tick.tick(), if state.needs_sync() => state.sync(),
            _ = &mut startup_timeout, if !protocol.ready => {
                state.terminal_output(b"rho-shell did not complete its protocol handshake\n");
                break None;
            }
            _ = &mut close_timeout, if closing => break None,
            status = child.wait() => break status.ok().and_then(|status| status.code()),
        }
    };

    drop(requests_tx);
    // The kernel can exit while background descendants retain workspace authority.
    // Terminate the whole owned session before draining.
    process_session.terminate().await;
    let status = match status {
        Some(status) => Some(status),
        None => tokio::time::timeout(std::time::Duration::from_secs(1), child.wait())
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|status| status.code()),
    };

    // The process can exit after writing its final frames but before the
    // socket reader has forwarded them into the bounded channel. Drain until
    // socket EOF so final structured output does not race child reaping.
    let response_drain = tokio::time::sleep(std::time::Duration::from_secs(1));
    tokio::pin!(response_drain);
    loop {
        tokio::select! {
            biased;
            response = responses.recv() => match response {
                Some(response) => if !receive_sidecar(
                    &mut state,
                    &mut protocol,
                    &active_execution,
                    response,
                ) {
                    break;
                },
                None => break,
            },
            _ = &mut response_task => {
                while let Ok(response) = responses.try_recv() {
                    if !receive_sidecar(
                        &mut state,
                        &mut protocol,
                        &active_execution,
                        response,
                    ) {
                        break;
                    }
                }
                break;
            }
            _ = &mut response_drain => break,
        }
    }

    if !response_task.is_finished() {
        response_task.abort();
    }
    request_task.abort();
    active_execution.store(0, Ordering::Release);
    state.finish(status.or(protocol.announced_status));
    registry.forget(agent_id, &_cmds_keepalive).await;
    for reply in close_replies {
        let _ = reply.send(());
    }
}

async fn read_responses(
    mut reader: tokio::net::unix::OwnedReadHalf,
    responses: mpsc::Sender<Response>,
) {
    loop {
        let Ok(response) = rho_shell_proto::read_frame_async(&mut reader).await else {
            break;
        };
        if responses.send(response).await.is_err() {
            break;
        }
    }
}

async fn write_requests(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut requests: mpsc::Receiver<Request>,
) {
    while let Some(request) = requests.recv().await {
        if rho_shell_proto::write_frame_async(&mut writer, &request)
            .await
            .is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandboxed_workdirs_are_refused() {
        let id =
            rho_workspaces::WorkspaceId::from_counter(1, &rho_workspaces::WorkspaceIdDomain(42))
                .unwrap();
        let sandbox = rho_workspaces::WorkspaceInfo::Sandbox {
            repo: camino::Utf8PathBuf::from("/repo"),
            id,
        };
        assert!(ensure_supported_workdirs(&[sandbox]).is_err());
        let checkout = rho_workspaces::WorkspaceInfo::UserCheckout {
            repo: camino::Utf8PathBuf::from("/repo"),
        };
        assert!(ensure_supported_workdirs(&[checkout]).is_ok());
    }

    #[test]
    fn abnormal_exit_clears_paused_pagers_from_final_state() {
        let (submit_tx, _submit_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (exit_tx, exit_rx) = watch::channel(None);
        let mut state = State {
            shell: rho_ui_proto::shell::ShellState {
                pagers: vec![rho_ui_proto::shell::ShellPager {
                    execution: 1,
                    pager: 1,
                    page: 1,
                    lines: 24,
                    bytes: 100,
                }],
                ..Default::default()
            },
            execution_outputs: HashMap::new(),
            terminal_output: PlainOutput::default(),
            clients: Vec::new(),
            submit: ShellSubmitter {
                tx: submit_tx,
                next_execution: Arc::new(std::sync::Mutex::new(1)),
            },
            control: ShellControls {
                tx: control_tx,
                active_execution: Arc::new(AtomicU64::new(0)),
            },
            exit_tx,
        };

        state.finish(None);

        assert!(state.shell.pagers.is_empty());
        assert!(exit_rx.borrow().as_ref().unwrap().state.pagers.is_empty());
    }

    #[test]
    fn plain_output_handles_ansi_and_active_line_rewrites() {
        let mut output = PlainOutput::default();
        output.advance(b"count 1\rcount 2\x1b[K\n\x1b[31mdone\x1b[0m\n");
        assert_eq!(output.text, "count 2\ndone\n");
        assert_eq!(
            output.styles,
            vec![ShellStyleSpan {
                start: 8,
                end: 12,
                style: ShellTextStyle {
                    foreground: Some(ShellColor::Indexed(1)),
                    ..Default::default()
                },
            }]
        );
    }

    #[test]
    fn plain_output_tracks_extended_colors_through_rewrites() {
        let mut output = PlainOutput::default();
        output.advance(b"\x1b[38;5;196mred\r\x1b[38:2::1:2:3mG\x1b[48;2;4;5;6mB\x1b[0m");
        assert_eq!(output.text, "GBd");
        assert_eq!(
            output.styles,
            vec![
                ShellStyleSpan {
                    start: 0,
                    end: 1,
                    style: ShellTextStyle {
                        foreground: Some(ShellColor::Rgb {
                            red: 1,
                            green: 2,
                            blue: 3,
                        }),
                        ..Default::default()
                    },
                },
                ShellStyleSpan {
                    start: 1,
                    end: 2,
                    style: ShellTextStyle {
                        foreground: Some(ShellColor::Rgb {
                            red: 1,
                            green: 2,
                            blue: 3,
                        }),
                        background: Some(ShellColor::Rgb {
                            red: 4,
                            green: 5,
                            blue: 6,
                        }),
                        ..Default::default()
                    },
                },
                ShellStyleSpan {
                    start: 2,
                    end: 3,
                    style: ShellTextStyle {
                        foreground: Some(ShellColor::Indexed(196)),
                        ..Default::default()
                    },
                },
            ]
        );
    }

    #[test]
    fn plain_output_trim_stays_on_utf8_boundaries() {
        let text = "λ".repeat(SHELL_STATE_CAP / 2 + 1);
        let end = text.len();
        let mut output = PlainOutput {
            text,
            styles: vec![ShellStyleSpan {
                start: 0,
                end: end as u64,
                style: ShellTextStyle {
                    foreground: Some(ShellColor::Indexed(2)),
                    ..Default::default()
                },
            }],
            cursor: end,
            line_start: end,
            ..Default::default()
        };
        assert!(output.trim());
        assert!(output.text.starts_with(OMITTED));
        assert!(output.text.len() <= OUTPUT_TRIM_TO + OMITTED.len() + 2);
        assert_eq!(output.styles[0].start, OMITTED.len() as u64);
        assert_eq!(output.styles[0].end, output.text.len() as u64);
    }

    #[test]
    fn plain_output_style_overflow_preserves_incremental_prefix() {
        let mut bytes = Vec::new();
        for index in 0..MAX_STYLE_SPANS - 2 {
            let color = if index % 2 == 0 { 31 } else { 32 };
            bytes.extend_from_slice(format!("\x1b[{color}mx\x1b[0m\n").as_bytes());
        }
        let mut output = PlainOutput::default();
        output.advance(&bytes);
        let current_line = output.line_start;
        output.advance(b"\x1b[33ma\x1b[0m \x1b[34mb\x1b[0m \x1b[35mc\x1b[0m");

        assert_eq!(output.styles.len(), MAX_STYLE_SPANS);
        assert!(
            output
                .styles
                .windows(2)
                .all(|spans| spans[0].end <= spans[1].start)
        );
        assert_eq!(output.styles[0].start, 0);

        let prefix_end = output
            .styles
            .partition_point(|span| span.end <= current_line as u64);
        let mut reconstructed = output.styles[..prefix_end].to_vec();
        let current_styles = output.styles_from(current_line);
        assert_eq!(current_styles.len(), 2);
        reconstructed.extend(current_styles);
        assert_eq!(reconstructed, output.styles);
    }

    #[test]
    fn sidecar_cannot_choose_commands_or_execution_order() {
        let mut protocol = SidecarProtocol::new();
        assert!(matches!(
            protocol.receive(Response::Ready {
                protocol: PROTOCOL_VERSION,
                prompt: "> ".into(),
                cwd: "/tmp".into(),
            }),
            Ok(SidecarEvent::Ready { .. })
        ));
        assert_eq!(
            protocol.request(QueuedSubmission {
                execution: 1,
                command: "echo daemon-owned".into(),
            }),
            Some(Request::Execute {
                execution: 1,
                command: "echo daemon-owned".into(),
            })
        );
        assert!(
            protocol
                .receive(Response::Output {
                    execution: 1,
                    data: b"too early".to_vec(),
                })
                .is_err()
        );
        match protocol
            .receive(Response::Started { execution: 1 })
            .unwrap()
        {
            SidecarEvent::Started { execution } => assert_eq!(execution, 1),
            _ => panic!("expected started event"),
        }
        assert!(matches!(
            protocol.receive(Response::PagerStarted {
                execution: 1,
                pager: 7,
            }),
            Ok(SidecarEvent::PagerStarted)
        ));
        assert!(matches!(
            protocol.receive(Response::PagerPaused {
                execution: 1,
                pager: 7,
                page: 1,
                lines: 24,
                bytes: 100,
            }),
            Ok(SidecarEvent::PagerPaused {
                execution: 1,
                pager: 7,
                ..
            })
        ));
        assert!(
            protocol
                .receive(Response::PagerPaused {
                    execution: 1,
                    pager: 7,
                    page: 1,
                    lines: 24,
                    bytes: 100,
                })
                .is_err()
        );
        assert!(matches!(
            protocol.receive(Response::PagerResumed {
                execution: 1,
                pager: 7,
            }),
            Ok(SidecarEvent::PagerResumed { .. })
        ));
        for (lines, bytes) in [(MAX_PAGER_LINES + 1, 100), (1, MAX_PAGER_BYTES + 1), (0, 0)] {
            assert!(
                protocol
                    .receive(Response::PagerPaused {
                        execution: 1,
                        pager: 7,
                        page: 2,
                        lines,
                        bytes,
                    })
                    .is_err()
            );
        }
        assert!(matches!(
            protocol.receive(Response::PagerFinished {
                execution: 1,
                pager: 7,
            }),
            Ok(SidecarEvent::PagerFinished { .. })
        ));
        assert!(
            protocol
                .receive(Response::Finished {
                    execution: 2,
                    status: 0,
                    prompt: "> ".into(),
                    cwd: "/tmp".into(),
                })
                .is_err()
        );
    }

    #[tokio::test]
    async fn shell_end_to_end_over_registry() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".bashrc"),
            "PS1='rho-test> '\n\
             PROMPT_COMMAND='export RHO_TEST_CONFIG_HOOK=fired'\n\
             trap 'printf fired >\"$HOME/brush-exit-hook\"' EXIT\n",
        )
        .unwrap();
        let environment = rho_workspaces::UserEnvironment::new(vec![
            ("PATH".into(), std::env::var_os("PATH").unwrap()),
            ("HOME".into(), home.clone().into_os_string()),
            ("USER".into(), "rho-test".into()),
            ("LOGNAME".into(), "rho-test".into()),
            ("LANG".into(), "C.UTF-8".into()),
        ]);
        let repo = Arc::new(
            rho_workspaces::Repo::open_plain_with_environment(
                temp.path(),
                rho_workspaces::PathOverrides::default(),
                environment,
            )
            .unwrap(),
        );
        let workspace = repo.user_checkout().await.unwrap();
        let view = rho_workspaces::View::new(vec![workspace]).unwrap();
        let registry = Arc::new(ShellRegistry::default());
        let agent_id =
            AgentId::from_counter(1, &rho_agent::db::AgentIdDomain(42)).expect("counter encodes");

        let spawn = || ShellSpawn {
            view: Arc::clone(&view),
            program: std::env::current_exe().unwrap().into_os_string(),
            args: vec![
                "--exact".into(),
                "shell::tests::rho_shell_test_child".into(),
                "--nocapture".into(),
            ],
            pager_program: "cat".into(),
        };
        registry.start(agent_id, spawn()).await.unwrap();
        let mut first = registry.attach(agent_id).await.unwrap();
        let entries = registry.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].agent_id, agent_id);
        assert_eq!(entries[0].clients, 1);
        let mut first_state = rho_ui_proto::shell::ShellState::default();

        let initial_token = "shell-e2e-23";
        let initial_command = shell_token(initial_token);
        first.submit.send(initial_command.clone()).await.unwrap();
        wait_for_text(&mut first, &mut first_state, initial_token).await;
        assert_eq!(
            render_state(&first_state).matches(&initial_command).count(),
            1,
            "the sideband protocol is the only source of accepted input"
        );

        // Brush owns one persistent evaluator, including variables, functions,
        // working directory, startup configuration, and prompt hooks.
        let state_prefix = shell_token("kernel-state-");
        first
            .submit
            .send(format!(
                "rho_kernel_value=persistent; rho_kernel_fn() {{ {state_prefix} \"$rho_kernel_value\"; }}"
            ))
            .await
            .unwrap();
        first.submit.send("rho_kernel_fn".to_owned()).await.unwrap();
        wait_for_text(&mut first, &mut first_state, "kernel-state-persistent").await;

        first
            .submit
            .send("mkdir -p nested; cd nested".to_owned())
            .await
            .unwrap();
        let cwd_token = "cwd-persisted";
        first
            .submit
            .send(format!(
                "test \"$(basename \"$PWD\")\" = nested && {}",
                shell_token(cwd_token)
            ))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, cwd_token).await;
        first.submit.send("cd ..".to_owned()).await.unwrap();

        let prompt_token = "prompt-hook-fired";
        first
            .submit
            .send(format!(
                "{} \"$RHO_TEST_CONFIG_HOOK\"",
                shell_token("prompt-hook-")
            ))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, prompt_token).await;

        let color_token = "colored-shell-output";
        let color_execution = first
            .submit
            .send(format!("printf '\\033[31m{color_token}\\033[0m\\n'"))
            .await
            .unwrap();
        wait_for_finished(&mut first, &mut first_state, color_execution).await;
        let color_block = first_state
            .executions
            .iter()
            .find(|block| block.execution == color_execution)
            .unwrap();
        let color_start = color_block.output.find(color_token).unwrap() as u64;
        assert!(color_block.styles.iter().any(|span| {
            span.start <= color_start
                && span.end >= color_start + color_token.len() as u64
                && span.style.foreground == Some(ShellColor::Indexed(1))
        }));
        let color_env_token = "color-environment-ok";
        first
            .submit
            .send(format!(
                "test \"$TERM\" = xterm-256color && test -z \"${{NO_COLOR+x}}\" && {}",
                shell_token(color_env_token)
            ))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, color_env_token).await;

        // Prompt construction is bounded independently of command execution.
        first
            .submit
            .send("PS1=$(printf '%20000s' '')".to_owned())
            .await
            .unwrap();
        let bounded_prompt_token = "bounded-prompt-survived";
        first
            .submit
            .send(format!(
                "PS1='rho-test> '; {}",
                shell_token(bounded_prompt_token)
            ))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, bounded_prompt_token).await;

        // The protocol descriptor is close-on-exec and is not present in the
        // virtual descriptor table used to execute commands.
        let control_fd_token = "control-fd-closed";
        first
            .submit
            .send(format!(
                "command sh -c 'test ! -e /proc/self/fd/3' && {}",
                shell_token(control_fd_token)
            ))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, control_fd_token).await;

        // A background writer retains the execution that created its PTY,
        // even when its bytes arrive after the foreground evaluator returned.
        let late_token = "tagged-late-output";
        first
            .submit
            .send(format!("{{ sleep 0.1; {}; }} &", shell_token(late_token)))
            .await
            .unwrap();
        let foreground_token = "tagged-foreground-output";
        first
            .submit
            .send(shell_token(foreground_token))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, foreground_token).await;
        wait_for_text(&mut first, &mut first_state, late_token).await;

        // Interrupt targets descendants attached to the active execution PTY
        // without giving the persistent evaluator a controlling terminal.
        let started_token = "foreground-started";
        first
            .submit
            .send(format!(
                "{}; sleep 60 && touch interrupt-failed",
                shell_token(started_token)
            ))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, started_token).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        first.control.send(ShellControl::Interrupt).await.unwrap();
        let interrupt_token = "interrupt-ok";
        first
            .submit
            .send(shell_token(interrupt_token))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, interrupt_token).await;
        assert!(!temp.path().join("interrupt-failed").exists());

        let line_token = "line-limit-ok";
        let long_line = format!("{} #{}", shell_token(line_token), "x".repeat(8192));
        assert!(rho_ui_proto::shell::command_fits(&long_line));
        first.submit.send(long_line).await.unwrap();
        wait_for_text(&mut first, &mut first_state, line_token).await;

        let too_long = format!(
            "touch oversized-command-ran #{}",
            "x".repeat(rho_ui_proto::shell::MAX_COMMAND_BYTES)
        );
        assert!(!rho_ui_proto::shell::command_fits(&too_long));
        assert!(first.submit.send(too_long).await.is_err());
        let after_oversized_token = "after-oversized-ok";
        first
            .submit
            .send(shell_token(after_oversized_token))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, after_oversized_token).await;
        assert!(!temp.path().join("oversized-command-ran").exists());
        wait_for_idle(&first).await;

        // Controls sent while idle are scoped to no execution.
        first.control.send(ShellControl::Interrupt).await.unwrap();
        first.control.send(ShellControl::Eof).await.unwrap();
        let idle_control_token = "idle-controls-discarded";
        first
            .submit
            .send(format!("sleep 0.1; {}", shell_token(idle_control_token)))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, idle_control_token).await;

        // EOF writes VEOF only to this execution's PTY, not the shell session.
        let cat_execution = first.submit.send("cat".to_owned()).await.unwrap();
        wait_for_running(&mut first, &mut first_state, cat_execution).await;
        first.control.send(ShellControl::Eof).await.unwrap();
        let after_eof_token = "after-eof";
        let after_eof_execution = first
            .submit
            .send(shell_token(after_eof_token))
            .await
            .unwrap();
        wait_for_text(&mut first, &mut first_state, after_eof_token).await;
        wait_for_finished(&mut first, &mut first_state, after_eof_execution).await;

        // A later attachment receives the canonical structured snapshot.
        let mut second = registry.attach(agent_id).await.unwrap();
        assert_eq!(registry.list().await[0].clients, 2);
        let mut second_state = rho_ui_proto::shell::ShellState::default();
        wait_for_text(&mut second, &mut second_state, initial_token).await;
        assert_eq!(second_state, first_state);

        let final_token = "final-output-before-exit";
        first
            .submit
            .send(format!(
                "sh -c 'echo $$ > bg.pid; exec sleep 60' & sleep 0.1; {}; exit 7",
                shell_token(final_token)
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), first.exit.changed())
            .await
            .expect("shell must exit")
            .expect("shell exit watch remains open");
        assert_eq!(
            first.exit.borrow().as_ref().and_then(|exit| exit.status),
            Some(7)
        );
        let exit = first.exit.borrow();
        let state = &exit.as_ref().unwrap().state;
        assert!(
            state
                .executions
                .windows(2)
                .all(|pair| pair[0].execution < pair[1].execution)
        );
        let final_execution = state.executions.last().unwrap();
        assert!(final_execution.output.contains(final_token));
        assert_eq!(
            final_execution.state,
            rho_ui_proto::shell::ShellExecutionState::Finished { status: 7 }
        );
        drop(exit);
        assert_eq!(
            std::fs::read_to_string(home.join("brush-exit-hook"))
                .unwrap()
                .trim(),
            "fired"
        );

        let background_pid: i32 = std::fs::read_to_string(temp.path().join("bg.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if unsafe { libc::kill(background_pid, 0) } < 0
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            gone.is_ok(),
            "background shell session member survived exit"
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !registry.list().await.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("exited shell remained registered");

        // Starting, detaching, discovering, and explicitly closing are
        // separate daemon lifecycle operations.
        registry.start(agent_id, spawn()).await.unwrap();
        let detached = registry.attach(agent_id).await.unwrap();
        assert_eq!(registry.list().await[0].clients, 1);
        drop(detached);
        assert_eq!(registry.list().await[0].clients, 0);
        let closing = registry.attach(agent_id).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), registry.close(agent_id))
            .await
            .expect("explicit shell close timed out")
            .unwrap();
        assert!(closing.exit.borrow().is_some());
        assert!(registry.list().await.is_empty());
    }

    fn shell_token(token: &str) -> String {
        let arguments = token
            .chars()
            .map(|character| format!("'{character}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let command = format!("printf '%s' {arguments}");
        assert!(!command.contains(token));
        command
    }

    async fn wait_for_idle(client: &ShellClient) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while client.control.active_execution.load(Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shell did not become idle");
    }

    async fn wait_for_text(
        client: &mut ShellClient,
        state: &mut rho_ui_proto::shell::ShellState,
        needle: &str,
    ) {
        if render_state(state).contains(needle) {
            return;
        }
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                apply_test_frame(
                    state,
                    client.frames.recv().await.expect("shell stream ended"),
                );
                if render_state(state).contains(needle) {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {needle:?} in {:?}", render_state(state)));
    }

    async fn wait_for_running(
        client: &mut ShellClient,
        state: &mut rho_ui_proto::shell::ShellState,
        execution: u64,
    ) {
        let running = |state: &rho_ui_proto::shell::ShellState| {
            state.executions.iter().any(|block| {
                block.execution == execution
                    && matches!(
                        block.state,
                        rho_ui_proto::shell::ShellExecutionState::Running
                    )
            })
        };
        if running(state) {
            return;
        }
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !running(state) {
                apply_test_frame(
                    state,
                    client.frames.recv().await.expect("shell stream ended"),
                );
            }
        })
        .await
        .expect("execution did not start");
    }

    async fn wait_for_finished(
        client: &mut ShellClient,
        state: &mut rho_ui_proto::shell::ShellState,
        execution: u64,
    ) {
        let finished = |state: &rho_ui_proto::shell::ShellState| {
            state.executions.iter().any(|block| {
                block.execution == execution
                    && matches!(
                        block.state,
                        rho_ui_proto::shell::ShellExecutionState::Finished { .. }
                    )
            })
        };
        if finished(state) {
            return;
        }
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !finished(state) {
                apply_test_frame(
                    state,
                    client.frames.recv().await.expect("shell stream ended"),
                );
            }
        })
        .await
        .expect("execution did not finish");
    }

    fn apply_test_frame(state: &mut rho_ui_proto::shell::ShellState, frame: ShellServerFrame) {
        use rho_ui_proto::shell::ShellExecutionState;
        match frame {
            ShellServerFrame::Snapshot { state: snapshot } => *state = snapshot,
            ShellServerFrame::ExecutionQueued { execution } => {
                state.executions.push(execution);
            }
            ShellServerFrame::ExecutionStarted {
                execution,
                prompt,
                cwd,
            } => {
                let block = state
                    .executions
                    .iter_mut()
                    .find(|block| block.execution == execution)
                    .expect("started execution was queued");
                block.state = ShellExecutionState::Running;
                block.prompt = prompt;
                block.cwd = cwd;
            }
            ShellServerFrame::ExecutionOutput {
                execution,
                start,
                end,
                text,
                styles,
            } => {
                let block = state
                    .executions
                    .iter_mut()
                    .find(|block| block.execution == execution)
                    .expect("output execution was queued");
                block
                    .output
                    .replace_range(start as usize..end as usize, &text);
                block.styles.retain(|span| span.end <= start);
                block.styles.extend(styles);
            }
            ShellServerFrame::PagerPaused { execution, pager } => {
                assert_eq!(pager.execution, execution);
                state
                    .pagers
                    .retain(|item| item.execution != execution || item.pager != pager.pager);
                state.pagers.push(pager);
            }
            ShellServerFrame::PagerResumed { execution, pager }
            | ShellServerFrame::PagerFinished { execution, pager } => {
                state
                    .pagers
                    .retain(|item| item.execution != execution || item.pager != pager);
            }
            ShellServerFrame::ExecutionFinished { execution, status } => {
                let block = state
                    .executions
                    .iter_mut()
                    .find(|block| block.execution == execution)
                    .expect("finished execution was queued");
                block.state = ShellExecutionState::Finished { status };
            }
            ShellServerFrame::ExecutionFailed { execution } => {
                if let Some(block) = execution.and_then(|execution| {
                    state
                        .executions
                        .iter_mut()
                        .find(|block| block.execution == execution)
                }) {
                    block.state = ShellExecutionState::Failed;
                }
            }
            ShellServerFrame::TerminalOutput {
                start,
                end,
                text,
                styles,
            } => {
                state
                    .terminal_output
                    .replace_range(start as usize..end as usize, &text);
                state.terminal_styles.retain(|span| span.end <= start);
                state.terminal_styles.extend(styles);
            }
            ShellServerFrame::Prompt { prompt, cwd } => {
                state.prompt = prompt;
                state.cwd = cwd;
            }
            ShellServerFrame::Accepted { .. } => {}
            ShellServerFrame::Exited { .. } => panic!("shell exited early"),
        }
    }

    fn render_state(state: &rho_ui_proto::shell::ShellState) -> String {
        let mut text = state.terminal_output.clone();
        for execution in &state.executions {
            if matches!(
                execution.state,
                rho_ui_proto::shell::ShellExecutionState::Queued
            ) {
                continue;
            }
            text.push_str(&execution.prompt);
            text.push_str(&execution.command);
            text.push('\n');
            text.push_str(&execution.output);
        }
        text
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rho_shell_test_child() {
        if std::env::var_os("RHO_SHELL_TEST_CHILD").is_some() {
            rho_shell::run().await.unwrap();
        }
    }
}
