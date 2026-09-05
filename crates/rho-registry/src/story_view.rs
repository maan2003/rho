//! An agent's transcript, made from the story the client holds.
//!
//! The daemon renders a live agent's transcript and streams it as frames.
//! That is the right thing while a turn is running and the wrong thing for
//! everything else: opening an old agent had to wait for a load, and with
//! the daemon down there was nothing to show at all. The story the client
//! already mirrors says what happened, so it can be read as a transcript
//! directly, and the daemon's frame replaces it whole when it arrives.
//!
//! What the story does not carry, it does not invent: a tool call is its
//! name and its one line, never its output, and a reply is what the agent
//! said. Bodies come on demand later (`AGENT-LOG-DESIGN.md`, slice D).

use rho_ui_proto::remote::{
    UiAgentState, UiAgentStatus, UiAgentUsage, UiBlock, UiTool, UiToolStatus,
};
use rho_ui_proto::story::{UiStoryEvent, UiStoryPos, UiToolLine, UiTurnOutcome};

/// The transcript of a story, oldest first. `from` is the position of the
/// first event given, so a rewind can find what it undoes.
pub fn story_transcript(from: UiStoryPos, events: &[UiStoryEvent]) -> UiAgentState {
    let mut blocks: Vec<UiBlock> = Vec::new();
    // Where each block came from, so a rewind drops exactly what was told
    // after it and keeps the rest.
    let mut told_at: Vec<UiStoryPos> = Vec::new();
    let mut turn_running = false;
    let mut errored = false;
    let mut usage = UiAgentUsage::default();

    for (offset, event) in events.iter().enumerate() {
        let pos = UiStoryPos(from.0 + offset as u64);
        let push = |block: UiBlock, blocks: &mut Vec<UiBlock>, told: &mut Vec<UiStoryPos>| {
            blocks.push(block);
            told.push(pos);
        };
        match event {
            UiStoryEvent::UserMessage { text, .. } => {
                errored = false;
                push(
                    UiBlock::UserMessage { text: text.clone() },
                    &mut blocks,
                    &mut told_at,
                );
            }
            UiStoryEvent::AgentMail { from, text, .. } => push(
                UiBlock::AgentMessage {
                    sender: *from,
                    text: text.clone(),
                },
                &mut blocks,
                &mut told_at,
            ),
            UiStoryEvent::Reply { text, .. } => push(
                UiBlock::AssistantMessage {
                    text: text.clone(),
                    phase: None,
                },
                &mut blocks,
                &mut told_at,
            ),
            UiStoryEvent::ToolCall { name, what, at } => push(
                UiBlock::Tool(UiTool {
                    // Stable across re-renders: the same call is the same
                    // position however often the story is read again.
                    id: format!("story-{}", pos.0),
                    name: name.clone(),
                    arguments: tool_line(what),
                    preview: None,
                    // The story tells that a call was made, never how it
                    // ended. `Success` is how a finished call reads; an
                    // error would be a claim the story never made.
                    status: UiToolStatus::Success,
                    output: None,
                    error: None,
                    started_at: Some(*at),
                    finished_at: None,
                    metadata: None,
                }),
                &mut blocks,
                &mut told_at,
            ),
            UiStoryEvent::TurnStarted { .. } => {
                turn_running = true;
                errored = false;
            }
            UiStoryEvent::TurnEnded { outcome, .. } => {
                turn_running = false;
                errored = matches!(outcome, UiTurnOutcome::Errored { .. });
                if let UiTurnOutcome::Errored { message } = outcome {
                    push(
                        UiBlock::Notice {
                            text: message.clone(),
                        },
                        &mut blocks,
                        &mut told_at,
                    );
                }
            }
            UiStoryEvent::Compacted { .. } => push(
                UiBlock::Notice {
                    text: "compacted".to_owned(),
                },
                &mut blocks,
                &mut told_at,
            ),
            UiStoryEvent::HistoryUnavailableBefore { .. } => push(
                UiBlock::Notice {
                    text: "history before this point is unavailable".to_owned(),
                },
                &mut blocks,
                &mut told_at,
            ),
            // A rewind is told rather than unwritten, so the reader is the
            // one that hides what it undid.
            UiStoryEvent::Rewound { to, .. } => {
                let kept = told_at.iter().take_while(|told| told.0 < to.0).count();
                blocks.truncate(kept);
                told_at.truncate(kept);
            }
            UiStoryEvent::Cost { usage: bucket, .. } => usage.total = bucket.clone(),
            UiStoryEvent::Created { .. }
            | UiStoryEvent::Parented { .. }
            | UiStoryEvent::Wants { .. }
            | UiStoryEvent::Titled { .. }
            | UiStoryEvent::Activity { .. }
            | UiStoryEvent::RoleChanged { .. }
            | UiStoryEvent::WorkdirAdded { .. } => {}
        }
    }

    UiAgentState {
        blocks,
        // Never `Streaming`: this is the story, not the live stream. A turn
        // that was running when the client last heard is the daemon's to
        // report again.
        status: if errored {
            UiAgentStatus::Error
        } else if turn_running {
            UiAgentStatus::Unloaded
        } else {
            UiAgentStatus::Idle
        },
        context_used: None,
        usage,
    }
}

/// The one line a tool call carries: the path it read, the command it ran,
/// the query it made, or the agent it spoke to.
fn tool_line(what: &UiToolLine) -> String {
    match what {
        UiToolLine::Path(path) => path.to_string(),
        UiToolLine::Command(command) => command.clone(),
        UiToolLine::Query(query) => query.clone(),
        UiToolLine::Agent(agent) => agent.encoded(),
        UiToolLine::Nothing => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;

    use super::*;

    fn told(events: Vec<UiStoryEvent>) -> UiAgentState {
        story_transcript(UiStoryPos(0), &events)
    }

    #[test]
    fn a_told_turn_reads_as_a_transcript() {
        let state = told(vec![
            UiStoryEvent::UserMessage {
                text: "have a look".to_owned(),
                at: UnixMs(1),
            },
            UiStoryEvent::TurnStarted { at: UnixMs(2) },
            UiStoryEvent::ToolCall {
                name: "Read".to_owned(),
                what: UiToolLine::Path("/tmp/README.md".into()),
                at: UnixMs(3),
            },
            UiStoryEvent::Reply {
                text: "done looking".to_owned(),
                at: UnixMs(4),
            },
            UiStoryEvent::TurnEnded {
                outcome: UiTurnOutcome::Completed,
                at: UnixMs(5),
            },
        ]);
        assert_eq!(state.status, UiAgentStatus::Idle);
        assert_eq!(state.blocks.len(), 3);
        assert!(matches!(state.blocks[0], UiBlock::UserMessage { .. }));
        let UiBlock::Tool(tool) = &state.blocks[1] else {
            panic!("the call is a tool block");
        };
        assert_eq!(tool.arguments, "/tmp/README.md");
        assert_eq!(tool.output, None, "the story never carries tool output");
        assert!(matches!(state.blocks[2], UiBlock::AssistantMessage { .. }));
    }

    /// The error text is the trailing notice, which is what `Error` status
    /// means to every reader of a live frame.
    #[test]
    fn an_errored_turn_ends_in_a_notice() {
        let state = told(vec![
            UiStoryEvent::TurnStarted { at: UnixMs(1) },
            UiStoryEvent::TurnEnded {
                outcome: UiTurnOutcome::Errored {
                    message: "the deploy script exited 1".to_owned(),
                },
                at: UnixMs(2),
            },
        ]);
        assert_eq!(state.status, UiAgentStatus::Error);
        assert_eq!(
            state.blocks,
            vec![UiBlock::Notice {
                text: "the deploy script exited 1".to_owned()
            }]
        );
    }

    /// A rewind hides what it undid and keeps what came before it.
    #[test]
    fn a_rewind_hides_what_it_undid() {
        let state = told(vec![
            UiStoryEvent::UserMessage {
                text: "first".to_owned(),
                at: UnixMs(1),
            },
            UiStoryEvent::Reply {
                text: "second".to_owned(),
                at: UnixMs(2),
            },
            UiStoryEvent::Rewound {
                to: UiStoryPos(1),
                at: UnixMs(3),
            },
        ]);
        assert_eq!(
            state.blocks,
            vec![UiBlock::UserMessage {
                text: "first".to_owned()
            }]
        );
    }
}
