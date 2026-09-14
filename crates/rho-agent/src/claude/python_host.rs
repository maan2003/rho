//! Rho's Python notebook, served to Claude Code as an in-process MCP server.
//!
//! Claude Code routes every JSON-RPC message for an SDK-hosted server
//! through its control channel, and the loop hands those here. A
//! `tools/call` of `exec` starts a cell and holds the reply until
//! [`boundary`] — the one decision that opens a native agent's next request
//! — says the model should look: a job ended, the cell notified, a check-in
//! came due, or a user message is waiting. Everything older cells have said
//! since the model last looked rides along with that reply. With no call
//! open, the same decision says when an idle model is woken with a message
//! instead. Nothing here decides anything of its own.

use std::collections::BTreeMap;
use std::sync::Arc;

use rho_agent_tools::{PythonCell, PythonExec, PythonNotebook, ReplyState, SourceWaker};
use rho_claude::mcp::{reply, text_item, tool_result};
#[cfg(test)]
use rho_core::ToolOutputStatus;
use rho_core::{ExecCall, ExecId, ToolOutput, ToolSpec, UnixMs};
use serde_json::Value;
use tokio::sync::Notify;

use crate::boundary::{
    Boundary, ModelAsked, ModelTurn, Observations, SourceKind, Standing, boundary,
};

/// One exec call the CLI is waiting on.
#[derive(Clone, Debug)]
pub(crate) struct PendingExec {
    /// The control request carrying the call, which the reply answers.
    pub request_id: String,
    /// The JSON-RPC id of the `tools/call`.
    pub rpc_id: Value,
    pub exec_id: ExecId,
    cell: u64,
}

/// The core's bookkeeping for one cell, as the native runtime keeps it for a
/// call: how much of its story the model has.
struct Cell {
    session: Box<PythonCell>,
    answer: ReplyState,
}

/// One notebook, its cells, and the exec call (if any) the CLI is waiting on.
pub(crate) struct PythonHost {
    tool: PythonNotebook,
    /// The host functions the notebook exposes, for the prompt.
    host_specs: Vec<ToolSpec>,
    /// Woken by any cell with something new; the loop asks the boundary.
    notify: Arc<Notify>,
    cells: BTreeMap<u64, Cell>,
    next_cell: u64,
    pending: Option<PendingExec>,
    /// The newest exec, whose check-in sets the pace even after its session
    /// is reaped, as in the native runtime.
    latest: Option<(u64, Arc<PythonExec>)>,
    /// What the model's latest turn settled: an open call, or prose.
    turn: Option<ModelTurn>,
    /// When each pending event was first seen by a decision that could act.
    observations: Observations,
    /// Whether the user stopped the agent since anything was asked of it:
    /// a cancelled cell's last words must not wake the model the user just
    /// silenced, as natively (`DECISION-stopped-agents-wait-for-fresh-input`).
    standing: Standing,
}

/// Everything waiting for the model at one boundary.
pub(crate) struct Drained {
    /// The open call's own answer, when there was one.
    pub own: Option<(ExecId, ToolOutput)>,
    /// What older cells have said since the model last looked.
    pub updates: Vec<(ExecId, ToolOutput)>,
}

impl Drained {
    pub(crate) fn is_empty(&self) -> bool {
        self.own.is_none() && self.updates.is_empty()
    }

    pub(crate) fn into_mcp_result(self) -> Value {
        rho_claude::mcp::exec_result(self.own.map(|(_, output)| output), self.updates)
    }
}

impl PythonHost {
    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.cancel(UnixMs::now());
        self.tool.shutdown().await.map_err(anyhow::Error::msg)?;
        self.cells.clear();
        self.pending = None;
        self.latest = None;
        Ok(())
    }

    pub(crate) fn new(tool: PythonNotebook, host_specs: Vec<ToolSpec>) -> Self {
        Self {
            tool,
            host_specs,
            notify: Arc::new(Notify::new()),
            cells: BTreeMap::new(),
            next_cell: 1,
            pending: None,
            latest: None,
            turn: None,
            observations: Observations::default(),
            standing: Standing::Nothing,
        }
    }

    pub(crate) fn host_specs(&self) -> &[ToolSpec] {
        &self.host_specs
    }

    pub(crate) fn notify(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// Starts a cell for a `tools/call`. The reply waits on the boundary,
    /// except for a second call while one is open, which is refused on the
    /// spot: the notebook takes one exec per model response, as natively.
    pub(crate) fn exec(
        &mut self,
        request_id: String,
        rpc_id: Value,
        exec_id: ExecId,
        source: String,
        now: UnixMs,
    ) -> Option<Value> {
        if !self.can_admit() {
            return Some(reply(
                rpc_id,
                tool_result(
                    vec![text_item(
                        "The notebook is stopped or already has an open exec reply; this call was not run.",
                    )],
                    true,
                ),
            ));
        }
        let cell = self.next_cell;
        self.next_cell += 1;
        let call = ExecCall {
            id: exec_id.clone(),
            source,
        };
        let session = self
            .tool
            .exec(call, SourceWaker::new(Arc::clone(&self.notify)));
        self.latest = Some((cell, session.execution()));
        self.cells.insert(
            cell,
            Cell {
                session,
                answer: ReplyState::Owed,
            },
        );
        self.pending = Some(PendingExec {
            request_id,
            rpc_id,
            exec_id,
            cell,
        });
        self.turn = Some(ModelTurn {
            spoke_at: now,
            asked: ModelAsked::Calls,
        });
        None
    }

    /// The user spoke: a stop, if there was one, is lifted.
    pub(crate) fn user_spoke(&mut self) {
        self.standing = Standing::Nothing;
    }

    /// The model ended a turn with prose: nothing is owed, and only what the
    /// cells go on to say is a reason to wake it.
    pub(crate) fn turn_ended(&mut self, now: UnixMs) {
        self.turn = Some(ModelTurn {
            spoke_at: now,
            asked: ModelAsked::Nothing,
        });
    }

    /// Every source, as the boundary wants them. `user_oldest_at` is the
    /// longest-waiting message in the CLI's queue: company for an open call,
    /// which returns so the model can read it.
    fn sources(&self, user_oldest_at: Option<UnixMs>) -> Vec<SourceKind> {
        let mut sources = vec![
            SourceKind::User {
                interrupt: false,
                oldest_at: user_oldest_at,
            },
            SourceKind::Mail {
                oldest_at: None,
                newest_at: None,
            },
        ];
        for (id, cell) in &self.cells {
            let latest = self.latest.as_ref().is_some_and(|(latest, _)| latest == id);
            sources.extend(
                cell.session
                    .sources()
                    .into_iter()
                    .map(|(_, facts)| match facts {
                        rho_agent_tools::SourceFacts::Cell(facts) => {
                            SourceKind::Cell { facts, latest }
                        }
                        rho_agent_tools::SourceFacts::Job(facts) => SourceKind::Job { facts },
                    }),
            );
        }
        if let Some((id, exec)) = &self.latest
            && !self.cells.contains_key(id)
        {
            sources.push(SourceKind::Cell {
                facts: exec.facts(),
                latest: true,
            });
        }
        sources
    }

    pub(crate) fn can_admit(&self) -> bool {
        self.pending.is_none() && !self.standing.stopped(None)
    }

    pub(crate) fn failed(&mut self, at: UnixMs, error: Arc<str>) {
        self.standing = Standing::Failed { at, error };
    }

    /// Whether an exec call is open, waiting on the boundary.
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Should the model look now? `available` says whether it could: an exec
    /// call is open, or the model is idle and can be sent a message. A model
    /// in the middle of a turn with no call open is a request in flight, as
    /// the boundary sees it: nothing is decided, and no event's clock starts
    /// until the model can be reached.
    pub(crate) fn decide(
        &mut self,
        available: bool,
        user_oldest_at: Option<UnixMs>,
        retained_output: bool,
        now: UnixMs,
    ) -> Boundary {
        let mut sources = self.sources(user_oldest_at);
        if retained_output {
            sources.push(SourceKind::Delivery);
        }
        boundary(
            &sources,
            self.turn.as_ref(),
            available.then_some(&self.standing),
            &mut self.observations,
            now,
        )
    }

    /// Answers the open call with everything waiting.
    pub(crate) fn answer_pending(&mut self) -> Option<(PendingExec, Drained)> {
        let pending = self.pending.clone()?;
        let drained = self.drain(Some(pending.cell));
        Some((pending, drained))
    }

    /// Everything waiting, for a model with no call open.
    pub(crate) fn drain_idle(&mut self) -> Drained {
        self.drain(None)
    }

    /// Every cell's contribution, the open call's cell first, as the native
    /// drain does it: a first contribution answers the call and every later
    /// one is an update.
    fn drain(&mut self, own: Option<u64>) -> Drained {
        let mut drained = Drained {
            own: None,
            updates: Vec::new(),
        };
        let mut order = self.cells.keys().copied().collect::<Vec<_>>();
        order.sort_by_key(|id| Some(*id) != own);
        for id in order {
            let cell = self.cells.get_mut(&id).expect("listed above");
            match cell.answer {
                ReplyState::Owed => {
                    let output = cell.session.first_output();
                    if Some(id) == own {
                        drained.own = Some((cell.session.execution().id().clone(), output));
                    } else {
                        drained
                            .updates
                            .push((cell.session.execution().id().clone(), output));
                    }
                }
                ReplyState::Sent => {
                    if let Some(output) = cell.session.more_output() {
                        drained
                            .updates
                            .push((cell.session.execution().id().clone(), output));
                    }
                }
            }
        }
        drained
    }

    /// The transport (or durable outbox) now owns the leased contributions.
    pub(crate) fn acknowledge(&mut self) {
        for cell in self.cells.values_mut() {
            if cell.session.acknowledge_output() {
                cell.answer = ReplyState::Sent;
            }
        }
        self.cells.retain(|_, cell| !cell.session.done());
        self.pending = None;
        self.observations.clear();
    }

    /// Stops every cell and forgets the open call, which the caller answers
    /// or lets the CLI abandon. Until the user speaks again, nothing the
    /// cells say on their way out wakes the model.
    pub(crate) fn cancel(&mut self, now: UnixMs) -> Option<PendingExec> {
        for cell in self.cells.values_mut() {
            cell.session.cancel();
        }
        self.standing = Standing::Cancelled { at: now };
        self.pending.take()
    }

    /// Forgets the open call without touching the cells: the CLI that was
    /// waiting on it is gone, the notebook is not.
    pub(crate) fn take_pending(&mut self) -> Option<PendingExec> {
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_refuses_buffered_exec_and_retained_output_can_answer_a_fresh_call() {
        let temp = tempfile::tempdir().unwrap();
        let notebook = PythonNotebook::new(
            rho_tool_shell::ShellTools::in_directory(
                std::time::Duration::from_secs(5),
                temp.path().to_str().unwrap().into(),
                Default::default(),
            ),
            Vec::new(),
        )
        .unwrap();
        let mut host = PythonHost::new(notebook, Vec::new());
        host.cancel(UnixMs(10));
        assert!(!host.can_admit());
        assert!(
            host.exec(
                "late".into(),
                serde_json::json!(1),
                "late".try_into().unwrap(),
                "raise AssertionError('must not run')".into(),
                UnixMs(11)
            )
            .is_some()
        );
        assert!(host.cells.is_empty());
        assert!(matches!(
            host.decide(true, None, true, UnixMs(11)),
            Boundary::No { recheck: None }
        ));
        host.user_spoke();
        assert!(host.can_admit());
        assert!(
            host.exec(
                "fresh".into(),
                serde_json::json!(2),
                "fresh".try_into().unwrap(),
                "pass".into(),
                UnixMs(12)
            )
            .is_none()
        );
        assert!(matches!(
            host.decide(true, None, true, UnixMs(12)),
            Boundary::Now { .. }
        ));
        let (pending, drained) = host
            .answer_pending()
            .expect("retained output cannot block an open MCP reply");
        assert_eq!(pending.exec_id.as_str(), "fresh");
        assert!(drained.own.is_some());
        host.acknowledge();
    }

    #[test]
    fn drained_output_becomes_mcp_content() {
        let output = |text: &str, status| ToolOutput {
            output: Arc::new(text.to_owned()),
            full_output: None,
            images: Arc::new(vec![rho_core::ImageContent {
                media_type: "image/png".into(),
                data: vec![1, 2, 3],
                detail: Default::default(),
            }]),
            status,
        };
        let result = Drained {
            own: Some((
                "current".try_into().unwrap(),
                output("ran", ToolOutputStatus::Error),
            )),
            updates: vec![(
                "older".try_into().unwrap(),
                output("later", ToolOutputStatus::Success),
            )],
        }
        .into_mcp_result();
        assert_eq!(result["isError"], true);
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 4);
        assert_eq!(content[0]["text"], "ran");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["data"], "AQID");
        assert_eq!(content[2]["text"], "Later output from exec older:\nlater");
        let result = Drained {
            own: None,
            updates: vec![(
                "older".try_into().unwrap(),
                output("later", ToolOutputStatus::Error),
            )],
        }
        .into_mcp_result();
        assert_eq!(
            result["isError"], false,
            "an update's failure is not the call's"
        );
    }
}
