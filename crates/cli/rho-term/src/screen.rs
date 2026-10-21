//! An agent's transcript on the terminal: which blocks are settled in the
//! scrollback and which still change below them.
//!
//! The terminal's scrollback cannot be edited, so a block goes into history
//! only once nothing will change it again: a finished call, a message with
//! something after it, everything once the turn is over. The blocks still
//! changing stay in the active zone just above the prompt, where they are
//! redrawn in place.

use std::sync::Arc;

use rho_agent_types::AgentId;
use rho_agents_client::state::{UiAgentState, UiAgentStatus, UiBlock, UiToolStatus};
use rho_term_ui::{BlockId, StyledBlock, TermHandle};

use crate::projection::{self, Kind, View};

pub struct Transcript {
    term: TermHandle,
    view: View,
    /// The blocks already in the scrollback, as they were when they went
    /// in. A prefix of the transcript.
    settled: Vec<Arc<UiBlock>>,
    /// The kind of the last block the scrollback shows, for the turn gap.
    settled_kind: Option<Kind>,
    /// The active zone, top to bottom: which block each row group shows and
    /// what it last drew.
    active: Vec<Active>,
}

struct Active {
    index: usize,
    id: BlockId,
    drawn: StyledBlock,
}

impl Transcript {
    pub fn new(term: TermHandle, view: View) -> Self {
        Self {
            term,
            view,
            settled: Vec::new(),
            settled_kind: None,
            active: Vec::new(),
        }
    }

    pub fn view(&self) -> View {
        self.view
    }

    /// Shows the other view: the screen is drawn again from the top.
    pub fn set_view(&mut self, view: View) {
        self.view = view;
        self.clear();
    }

    /// Forgets everything drawn, so the next sync draws it all again.
    pub fn clear(&mut self) {
        self.term.clear_output();
        self.settled.clear();
        self.settled_kind = None;
        self.active.clear();
    }

    /// Brings the screen to `state`.
    pub fn sync(&mut self, state: &UiAgentState, now_ms: u64, label: &dyn Fn(AgentId) -> String) {
        // A block the scrollback holds has changed (a rewind, say): the
        // scrollback is wrong, so it is drawn again.
        let diverged = self.settled.len() > state.blocks.len()
            || self
                .settled
                .iter()
                .zip(&state.blocks)
                .any(|(drawn, now)| !Arc::ptr_eq(drawn, now) && drawn != now);
        if diverged {
            self.clear();
        }

        let settles = settled_prefix(state);
        let mut kind = self.settled_kind;
        let mut active = Vec::new();
        for (index, block) in state.blocks.iter().enumerate().skip(self.settled.len()) {
            let rendered = projection::render(self.view, block, kind, now_ms, label);
            if rendered.is_some() {
                kind = Some(projection::kind(block));
            }
            if index < settles {
                // Settled now: it goes into history once, in order, and its
                // active rows (if it had any) go.
                if let Some(at) = self.active.iter().position(|a| a.index == index) {
                    let gone = self.active.remove(at);
                    self.term.remove_above_active(gone.id);
                    if let Some(rendered) = rendered {
                        self.term.set_block(gone.id, rendered);
                        self.term.push_history(gone.id);
                    } else {
                        self.term.remove_block(gone.id);
                    }
                } else if let Some(rendered) = rendered {
                    let id = self.term.new_block(rendered);
                    self.term.push_history(id);
                }
                self.settled.push(block.clone());
                self.settled_kind = kind;
            } else if let Some(rendered) = rendered {
                active.push((index, rendered));
            }
        }

        // The active zone: kept as it is where it still matches, redrawn
        // from the first block that moved.
        let keep = self
            .active
            .iter()
            .zip(&active)
            .take_while(|(drawn, (index, _))| drawn.index == *index)
            .count();
        for gone in self.active.drain(keep..) {
            self.term.remove_block(gone.id);
        }
        for (slot, (_, rendered)) in self.active.iter_mut().zip(&active) {
            if slot.drawn != *rendered {
                self.term.set_block(slot.id, rendered.clone());
                slot.drawn = rendered.clone();
            }
        }
        for (index, rendered) in active.into_iter().skip(keep) {
            let id = self.term.new_block(rendered.clone());
            self.term.push_above_active(id);
            self.active.push(Active {
                index,
                id,
                drawn: rendered,
            });
        }
        self.term.redraw();
    }
}

/// How many leading blocks will not change again. Once the turn is over
/// every block that the fold holds is settled, even a cell that ended the
/// turn and so never reports an end of its own; while the turn runs, a
/// block is settled once a later one has arrived and it is not a call
/// still running.
/// The trailing blocks the client composes (queued, unsent, status, the
/// host's draft) are never settled: they go when their message is
/// delivered.
pub fn settled_prefix(state: &UiAgentState) -> usize {
    let transient = |block: &UiBlock| {
        matches!(
            block,
            UiBlock::QueuedMessage { .. }
                | UiBlock::Unsent { .. }
                | UiBlock::Status { .. }
                | UiBlock::MessageDraft { .. }
        )
    };
    let running = |block: &UiBlock| matches!(block, UiBlock::Tool(tool) if tool.status == UiToolStatus::Running);
    let durable = state
        .blocks
        .iter()
        .position(|block| transient(block))
        .unwrap_or(state.blocks.len());
    let idle = matches!(state.status, UiAgentStatus::Idle | UiAgentStatus::Error);
    let mut settled = 0;
    for (index, block) in state.blocks[..durable].iter().enumerate() {
        let last = index + 1 == durable;
        if !idle && (running(block) || last) {
            break;
        }
        settled = index + 1;
    }
    settled
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use rho_agents_client::protocol::transcript::ArgumentsFormat;
    use rho_agents_client::state::{UiAgentState, UiAgentStatus, UiBlock, UiTool, UiToolStatus};
    use rho_term_ui::Term;

    use super::{Transcript, settled_prefix};
    use crate::projection::View;

    fn state(status: UiAgentStatus, blocks: Vec<UiBlock>) -> UiAgentState {
        UiAgentState {
            blocks: blocks.into_iter().map(Arc::new).collect(),
            status,
            runtime: None,
            context_used: None,
            usage: Default::default(),
            exec_timings: Default::default(),
        }
    }
    fn user(text: &str) -> UiBlock {
        UiBlock::UserMessage { text: text.into() }
    }
    fn said(text: &str) -> UiBlock {
        UiBlock::AssistantMessage {
            text: text.into(),
            phase: None,
        }
    }
    fn shell(command: &str, status: UiToolStatus) -> UiBlock {
        UiBlock::Tool(UiTool {
            id: command.into(),
            name: "shell".into(),
            arguments: command.into(),
            preview: None,
            status,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            timing: Default::default(),
            format: ArgumentsFormat::Text,
        })
    }

    /// The last block of an open turn may still grow, a running call may
    /// still end, and the client's own trailing rows go when their message
    /// is delivered; everything before the first of those is settled.
    #[test]
    fn what_is_settled() {
        let streaming = UiAgentStatus::Streaming;
        let running = UiAgentStatus::ToolCalling { waiting: None };
        assert_eq!(
            settled_prefix(&state(streaming, vec![user("a"), said("b")])),
            1
        );
        assert_eq!(
            settled_prefix(&state(UiAgentStatus::Idle, vec![user("a"), said("b")])),
            2
        );
        // A running call holds back what came after it, though more came.
        assert_eq!(
            settled_prefix(&state(
                running,
                vec![
                    user("a"),
                    shell("make", UiToolStatus::Running),
                    said("meanwhile")
                ]
            )),
            1
        );
        // Queued rows trail and are never history, even when idle.
        assert_eq!(
            settled_prefix(&state(
                UiAgentStatus::Idle,
                vec![
                    user("a"),
                    said("b"),
                    UiBlock::QueuedMessage {
                        text: "next".into(),
                        sender: None
                    }
                ]
            )),
            2
        );
    }

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);

    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn screen(output: &Output) -> Vec<String> {
        let mut parser = vt100::Parser::new(40, 60, 0);
        parser.process(&output.0.lock().unwrap());
        parser
            .screen()
            .rows(0, 60)
            .map(|row| row.trim_end().to_owned())
            .filter(|row| !row.is_empty())
            .collect()
    }

    /// A turn as it streams: text grows, a call runs and ends, the turn
    /// closes. Each block is on screen once, in order, whatever moved
    /// between history and the live zone on the way; switching views and
    /// back draws the same chat again.
    #[test]
    fn a_streamed_turn_is_drawn_once_in_order() {
        let output = Output::default();
        let (term, handle, _input) = Term::new_virtual(60, 40, "> ", Box::new(output.clone()));
        let mut transcript = Transcript::new(handle.clone(), View::Chat);
        let label = |_| String::new();
        let steps = [
            state(UiAgentStatus::Streaming, vec![user("hello"), said("Look")]),
            state(
                UiAgentStatus::ToolCalling { waiting: None },
                vec![
                    user("hello"),
                    said("Looking around."),
                    shell("ls", UiToolStatus::Running),
                ],
            ),
            state(
                UiAgentStatus::Streaming,
                vec![
                    user("hello"),
                    said("Looking around."),
                    shell("ls", UiToolStatus::Success),
                    said("Found two"),
                ],
            ),
            state(
                UiAgentStatus::Idle,
                vec![
                    user("hello"),
                    said("Looking around."),
                    shell("ls", UiToolStatus::Success),
                    said("Found two files."),
                ],
            ),
        ];
        for step in &steps {
            transcript.sync(step, 0, &label);
            handle.redraw_sync();
        }
        let chat = ["› hello", "Looking around.", "Found two files.", ">"];
        assert_eq!(screen(&output), chat);

        transcript.set_view(View::Activity);
        transcript.sync(&steps[3], 0, &label);
        handle.redraw_sync();
        assert_eq!(screen(&output), ["$ ls ok", ">"]);

        transcript.set_view(View::Chat);
        transcript.sync(&steps[3], 0, &label);
        handle.redraw_sync();
        assert_eq!(screen(&output), chat);
        drop(term);
    }
}
