//! Rho's Python notebook, served to Claude Code as an in-process MCP server.
//!
//! Claude Code routes every JSON-RPC message for an SDK-hosted server
//! through its control channel, and the loop hands those here. A
//! `tools/call` of `exec` starts a cell and holds the reply until the Rho
//! runtime's wake rules ([`wake::decide`]) say the model should look: the
//! cell finished, it notified, a task failed, the check-in came due, or a
//! user message is waiting. A cell that called `end_turn()` keeps its call
//! open, so the model waits inside it. With no call open, the same rules say
//! when an idle model is woken. The loop composes what either one carries
//! ([DECISION-roles](../../../../specs/DECISION-roles.md)).

use std::sync::Arc;

use rho_agent_types::UnixMs;
use rho_agent_types::transcript::ExecId;
use rho_claude::mcp::{reply, text_item, tool_result};
use rho_notebook::{CellHandle, Notebook};
use serde_json::Value;
use tokio::sync::Notify;

use crate::entry::Wake;
use crate::worker::shared::Progress;
use crate::worker::shared::wake::{self, Decision};

/// One exec call the CLI is waiting on.
#[derive(Clone, Debug)]
pub(crate) struct PendingExec {
    /// The control request carrying the call, which the reply answers.
    pub request_id: String,
    /// The JSON-RPC id of the `tools/call`.
    pub rpc_id: Value,
    pub exec_id: ExecId,
}

/// Whether the model should look now.
#[derive(Debug)]
pub(crate) enum Boundary {
    No { recheck: Option<UnixMs> },
    Now { why: Wake },
}

/// One notebook, its latest cell, and the exec call (if any) the CLI is
/// waiting on.
pub(crate) struct PythonHost {
    notebook: Notebook,
    /// Woken by the notebook whenever something changes.
    notify: Arc<Notify>,
    pending: Option<PendingExec>,
    latest: Option<(ExecId, CellHandle)>,
    /// The model has been told the latest cell finished.
    progress: Progress,
    /// The user stopped the agent, or its requests failed, at this time,
    /// since anything was asked of it: only fresh input wakes it again.
    stopped: Option<UnixMs>,
}

impl PythonHost {
    pub(crate) fn new(notebook: Notebook, notify: Arc<Notify>) -> Self {
        Self {
            notebook,
            notify,
            pending: None,
            latest: None,
            progress: Progress::default(),
            stopped: None,
        }
    }

    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.cancel(UnixMs::now());
        self.notebook.shutdown().await.map_err(anyhow::Error::msg)?;
        self.latest = None;
        Ok(())
    }

    pub(crate) fn notify(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// Starts a cell for a `tools/call`. The reply waits on the wake rules,
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
        self.notebook.reset_checkin();
        self.latest = Some((exec_id.clone(), self.notebook.run(source)));
        self.progress.told_returned = false;
        self.progress.last_response = Some(now);
        self.progress.prose = 0;
        self.progress.ended = false;
        self.pending = Some(PendingExec {
            request_id,
            rpc_id,
            exec_id,
        });
        None
    }

    /// A person or another agent wrote: a stop, if there was one, is lifted.
    /// The model must be able to call exec, the only way it reads an agent's
    /// message.
    pub(crate) fn input_arrived(&mut self) {
        self.stopped = None;
        self.progress.prose = 0;
    }

    /// A CLI result is not a user answer. Prose with no exec triggers a
    /// correction wake, bounded to prevent an endless provider loop.
    pub(crate) fn turn_ended(&mut self, now: UnixMs, ran_exec: bool) {
        self.progress.last_response = Some(now);
        if !ran_exec {
            self.progress.prose += 1;
        }
    }

    /// The model waits inside the open exec: its cell's return tells it
    /// nothing, and only news answers the call.
    pub(crate) fn end_turn(&mut self) {
        self.progress.ended = true;
    }

    pub(crate) fn ended(&self) -> bool {
        self.progress.ended
    }

    pub(crate) fn prose_correction(&self) -> bool {
        self.progress.prose > 0 && self.progress.prose < Progress::MAX_PROSE
    }

    pub(crate) fn running_tasks(&self) -> u32 {
        self.notebook
            .facts()
            .iter()
            .filter(|facts| {
                matches!(
                    facts.kind,
                    rho_notebook::Kind::Cell | rho_notebook::Kind::Task
                ) && facts.finished.is_none()
            })
            .count() as u32
    }

    pub(crate) fn checkin_at(&self) -> Option<UnixMs> {
        if self.stopped.is_some()
            || self.progress.prose >= Progress::MAX_PROSE
            || self.progress.ended
        {
            None
        } else {
            self.progress
                .last_response
                .map(|at| at + self.notebook.checkin())
        }
    }

    pub(crate) fn retire_settled(&self) -> bool {
        self.stopped.is_some()
            || self.progress.prose >= Progress::MAX_PROSE
            || (self.running_tasks() == 0 && self.checkin_at().is_none())
    }

    pub(crate) fn can_admit(&self) -> bool {
        self.pending.is_none() && self.stopped.is_none()
    }

    pub(crate) fn failed(&mut self, at: UnixMs, _error: Arc<str>) {
        self.stopped = Some(at);
    }

    /// Whether an exec call is open, waiting on the wake rules.
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Should the model look now? `available` says whether it could: an exec
    /// call is open, or the model is idle and can be sent a message. A model
    /// in the middle of a turn with no call open hears everything at its
    /// next call or turn end.
    pub(crate) fn decide(
        &mut self,
        available: bool,
        user_oldest_at: Option<UnixMs>,
        agent_oldest_at: Option<UnixMs>,
        archived: bool,
        now: UnixMs,
    ) -> Boundary {
        if !available {
            return Boundary::No { recheck: None };
        }
        let mut facts = self.progress.facts(
            Some(&self.notebook),
            self.latest.as_ref().map(|(_, cell)| cell),
        );
        facts.human = user_oldest_at;
        facts.agent = agent_oldest_at;
        facts.archived = archived;
        facts.prose_silenced = self.stopped.is_some() || self.progress.prose >= Progress::MAX_PROSE;
        // Answer an open call before the CLI's own timeout fails it.
        if self.pending.is_some()
            && let Some(opened) = self.progress.last_response
        {
            let limit =
                opened + (rho_claude::mcp::EXEC_TIMEOUT - std::time::Duration::from_secs(60));
            facts.checkin = Some(facts.checkin.map_or(limit, |at| at.min(limit)));
        }
        match wake::decide(&facts, now) {
            Decision::Now(why) => Boundary::Now { why },
            Decision::Later(recheck) => Boundary::No { recheck },
        }
    }

    /// Whether the open call's cell raised.
    pub(crate) fn latest_failed(&self) -> bool {
        self.latest
            .as_ref()
            .and_then(|(_, cell)| cell.facts().finished)
            .is_some_and(|end| end.failed)
    }

    /// What the notebook has to say, which it forgets once taken.
    pub(crate) fn report(&mut self) -> Option<rho_notebook::Report> {
        if let Some((_, cell)) = &self.latest
            && cell.facts().finished.is_some()
        {
            self.progress.told_returned = true;
        }
        self.notebook.report()
    }

    /// Stops every cell and forgets the open call, which the caller answers
    /// or lets the CLI abandon. Until the user speaks again, nothing the
    /// cells say on their way out wakes the model.
    pub(crate) fn cancel(&mut self, now: UnixMs) -> Option<PendingExec> {
        self.notebook.cancel();
        self.stopped = Some(now);
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

    async fn host(directory: &std::path::Path) -> PythonHost {
        let notify = Arc::new(Notify::new());
        let notebook = Notebook::new(
            rho_tool_shell::ShellTools::in_directory(
                directory.to_str().unwrap().into(),
                Default::default(),
            ),
            Vec::new(),
            Arc::clone(&notify),
            None,
        )
        .await
        .unwrap();
        PythonHost::new(notebook, notify)
    }

    async fn settle(host: &mut PythonHost) -> Wake {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Boundary::Now { why } = host.decide(true, None, None, false, UnixMs::now()) {
                return why;
            }
            assert!(tokio::time::Instant::now() < deadline, "never woke");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn an_open_call_returns_when_its_cell_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path()).await;
        assert!(
            host.exec(
                "request".into(),
                serde_json::json!(1),
                "call".try_into().unwrap(),
                "print('ran')".into(),
                UnixMs::now(),
            )
            .is_none()
        );
        assert!(!host.can_admit(), "one open exec at a time");
        assert_eq!(settle(&mut host).await, Wake::Returned);
        let text = host.report().unwrap().render().text;
        assert!(text.contains("ran"), "{text}");
        assert!(!host.latest_failed());
        assert_eq!(host.take_pending().unwrap().exec_id.as_str(), "call");
        assert!(host.can_admit());
        // Told once: the finished cell does not wake the model again.
        assert!(matches!(
            host.decide(true, None, None, false, UnixMs::now()),
            Boundary::No { .. }
        ));
        host.shutdown().await.unwrap();
    }

    /// With no call open too, taking the report is what tells the model:
    /// the notebook wakes nobody a second time for what it said.
    #[tokio::test]
    async fn news_taken_into_a_request_wakes_nothing_again() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path()).await;
        assert!(
            host.exec(
                "request".into(),
                serde_json::json!(1),
                "call".try_into().unwrap(),
                "notify('hello')".into(),
                UnixMs::now(),
            )
            .is_none()
        );
        host.take_pending();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let far = UnixMs::now() + std::time::Duration::from_secs(60);
        loop {
            if host.running_tasks() == 0
                && matches!(
                    host.decide(true, None, None, false, far),
                    Boundary::Now { .. }
                )
            {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "never woke");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let text = host.report().unwrap().render().text;
        assert!(text.contains("hello"), "{text}");
        // The check-in is the model's clock, not news: look before it.
        host.turn_ended(far, true);
        assert!(matches!(
            host.decide(true, None, None, false, far),
            Boundary::No { .. }
        ));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_refuses_exec_until_the_user_speaks() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path()).await;
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
        assert!(host.latest.is_none());
        assert!(matches!(
            host.decide(true, None, None, false, UnixMs(11)),
            Boundary::No { recheck: None }
        ));
        host.input_arrived();
        assert!(host.can_admit());
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn prose_correction_is_bounded_and_only_human_revives_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path()).await;
        for count in 1..=Progress::MAX_PROSE {
            host.turn_ended(UnixMs(count as u64 * 1000), false);
            assert_eq!(host.prose_correction(), count < Progress::MAX_PROSE);
        }
        assert!(matches!(
            host.decide(true, None, Some(UnixMs(0)), false, UnixMs(100_000)),
            Boundary::No { recheck: None }
        ));
        assert_eq!(host.checkin_at(), None);
        host.input_arrived();
        assert!(!host.prose_correction());
        assert!(matches!(
            host.decide(true, None, Some(UnixMs(0)), false, UnixMs(100_000)),
            Boundary::Now {
                why: Wake::AgentMessage
            }
        ));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_cell_that_ended_the_turn_holds_its_call_until_news() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path()).await;
        let t0 = UnixMs::now().0;
        assert!(
            host.exec(
                "request".into(),
                serde_json::json!(1),
                "call".try_into().unwrap(),
                "print('done')".into(),
                UnixMs(t0),
            )
            .is_none()
        );
        host.end_turn();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while host.running_tasks() > 0 {
            assert!(tokio::time::Instant::now() < deadline, "never finished");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Its return tells the model nothing; the call stays open.
        assert_eq!(host.checkin_at(), None);
        assert!(matches!(
            host.decide(true, None, None, false, UnixMs(t0 + 1_000_000)),
            Boundary::No { .. }
        ));
        assert!(host.has_pending());
        // News answers it, with what the cell printed.
        assert!(matches!(
            host.decide(
                true,
                None,
                Some(UnixMs(t0 + 2_000)),
                false,
                UnixMs(t0 + 1_000_000)
            ),
            Boundary::Now {
                why: Wake::AgentMessage
            }
        ));
        let text = host.report().unwrap().render().text;
        assert!(text.contains("done"), "{text}");
        host.take_pending();
        // The next exec is a new turn.
        assert!(
            host.exec(
                "request".into(),
                serde_json::json!(2),
                "next".try_into().unwrap(),
                "pass".into(),
                UnixMs(t0 + 20_000),
            )
            .is_none()
        );
        assert!(!host.ended());
        assert_eq!(host.checkin_at(), Some(UnixMs(t0 + 140_000)));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_open_call_is_answered_before_the_cli_times_it_out() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path()).await;
        assert!(
            host.exec(
                "request".into(),
                serde_json::json!(1),
                "call".try_into().unwrap(),
                "import asyncio\nset_max_wait(10**9)\nawait asyncio.sleep(600)".into(),
                UnixMs(10_000),
            )
            .is_none()
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while host.notebook.checkin() != std::time::Duration::from_secs(1_000_000_000) {
            assert!(tokio::time::Instant::now() < deadline, "set_max_wait ran");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let limit =
            UnixMs(10_000) + (rho_claude::mcp::EXEC_TIMEOUT - std::time::Duration::from_secs(60));
        assert!(matches!(
            host.decide(true, None, None, false, UnixMs(limit.0 - 1)),
            Boundary::No { recheck: Some(at) } if at == limit
        ));
        assert!(matches!(
            host.decide(true, None, None, false, limit),
            Boundary::Now { why: Wake::Checkin }
        ));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn checkin_continues_after_result_without_an_open_mcp_call() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path()).await;
        host.turn_ended(UnixMs(10_000), true);
        assert_eq!(host.checkin_at(), Some(UnixMs(130_000)));
        assert!(matches!(
            host.decide(true, None, None, false, UnixMs(129_999)),
            Boundary::No {
                recheck: Some(UnixMs(130_000))
            }
        ));
        assert!(matches!(
            host.decide(true, None, None, false, UnixMs(130_000)),
            Boundary::Now { why: Wake::Checkin }
        ));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retirement_requires_no_running_cells_and_no_armed_checkin() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path()).await;
        assert!(host.retire_settled());
        host.turn_ended(UnixMs(10), true);
        assert!(
            !host.retire_settled(),
            "CLI idle still owes a notebook check-in"
        );
        host.failed(UnixMs(20), Arc::from("stopped"));
        assert!(host.retire_settled());
        host.input_arrived();
        assert!(!host.retire_settled());
        host.shutdown().await.unwrap();
    }
}
