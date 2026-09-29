//! Rho's Python notebook, served to Claude Code as an in-process MCP server.
//!
//! Claude Code routes every JSON-RPC message for an SDK-hosted server
//! through its control channel, and the loop hands those here. A
//! `tools/call` of `exec` starts a cell and holds the reply until the Rho
//! runtime's wake rules ([`wake::decide`]) say the model should look: the
//! cell finished, it notified, a task failed, the check-in came due, or a
//! user message is waiting. Everything the notebook has said since the
//! model last looked rides along with that reply. With no call open, the
//! same rules say when an idle model is woken with a message instead.

use std::sync::Arc;

use rho_agent_types::transcript::{ExecId, ImageContent, ToolOutput};
use rho_agent_types::{ToolOutputStatus, UnixMs};
use rho_claude::mcp::{reply, text_item, tool_result};
use rho_notebook::{CellHandle, Notebook};
use serde_json::Value;
use tokio::sync::Notify;

use crate::entry::Wake;
use crate::worker::shared::Progress;
use crate::worker::shared::wake::{self, Decision};
use crate::{WakeFacts, WakeTrigger};

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
    Now { wake: WakeFacts },
}

/// One notebook, its latest cell, and the exec call (if any) the CLI is
/// waiting on.
pub(crate) struct PythonHost {
    notebook: Notebook,
    /// Woken by the notebook whenever something changes.
    notify: Arc<Notify>,
    pending: Option<PendingExec>,
    latest: Option<(ExecId, CellHandle)>,
    published: bool,
    /// The model has been told the latest cell finished.
    progress: Progress,
    /// The user stopped the agent, or its requests failed, since anything
    /// was asked of it: only the user wakes it again.
    stopped: bool,
}

/// Everything waiting for the model at one boundary.
pub(crate) struct Drained {
    /// The open call's own answer, when there was one.
    pub own: Option<(ExecId, ToolOutput)>,
    /// What the notebook said with no call open to carry it.
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
    pub(crate) fn new(notebook: Notebook, notify: Arc<Notify>) -> Self {
        Self {
            notebook,
            notify,
            pending: None,
            latest: None,
            published: false,
            progress: Progress::default(),
            stopped: false,
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
        self.published = false;
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

    /// Only a send from the latest cell retires its tentative preview.
    pub(crate) fn sent(&mut self, cell: u64) -> bool {
        if self
            .latest
            .as_ref()
            .is_some_and(|(_, latest)| latest.source_id() == cell)
        {
            self.published = true;
            return true;
        }
        false
    }

    pub(crate) fn latest_call(&self) -> Option<&str> {
        self.latest.as_ref().map(|(id, _)| id.as_str())
    }

    pub(crate) fn latest_finished(&self, id: &str) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|(latest, cell)| latest.as_str() == id && cell.facts().finished.is_some())
    }

    pub(crate) fn published_call(&self) -> Option<&str> {
        self.latest
            .as_ref()
            .and_then(|(id, _)| self.published.then(|| id.as_str()))
    }

    /// The user spoke: a stop, if there was one, is lifted.
    pub(crate) fn user_spoke(&mut self) {
        self.stopped = false;
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

    /// The open exec ends the turn: its answer tells the CLI to stop, and
    /// the idle model is woken only by news.
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
        if self.stopped || self.progress.prose >= Progress::MAX_PROSE || self.progress.ended {
            None
        } else {
            self.progress
                .last_response
                .map(|at| at + self.notebook.checkin())
        }
    }

    pub(crate) fn retire_settled(&self) -> bool {
        self.stopped
            || self.progress.prose >= Progress::MAX_PROSE
            || (self.running_tasks() == 0 && self.checkin_at().is_none())
    }

    pub(crate) fn can_admit(&self) -> bool {
        self.pending.is_none() && !self.stopped
    }

    pub(crate) fn failed(&mut self, _at: UnixMs, _error: Arc<str>) {
        self.stopped = true;
    }

    /// Whether an exec call is open, waiting on the wake rules.
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Should the model look now? `available` says whether it could: an exec
    /// call is open, or the model is idle and can be sent a message. A model
    /// in the middle of a turn with no call open hears everything at its
    /// next call or turn end. `retained_output` is an earlier answer that
    /// never reached the CLI, which goes out as soon as it can.
    pub(crate) fn decide(
        &mut self,
        available: bool,
        user_oldest_at: Option<UnixMs>,
        agent_oldest_at: Option<UnixMs>,
        archived: bool,
        retained_output: bool,
        now: UnixMs,
    ) -> Boundary {
        if !available {
            return Boundary::No { recheck: None };
        }
        let sources = self.notebook.facts();
        let mut facts = self.progress.facts(
            Some(&self.notebook),
            self.latest.as_ref().map(|(_, cell)| cell),
        );
        facts.human = user_oldest_at;
        facts.agent = agent_oldest_at;
        facts.archived = archived;
        facts.prose_silenced = self.stopped || self.progress.prose >= Progress::MAX_PROSE;
        let checkin_at = facts.checkin;
        let why = match wake::decide(&facts, now) {
            Decision::Now(why) => Some(why),
            Decision::Later(_) if retained_output => None,
            Decision::Later(recheck) => return Boundary::No { recheck },
        };
        let running = sources
            .iter()
            .filter(|facts| {
                matches!(
                    facts.kind,
                    rho_notebook::Kind::Cell | rho_notebook::Kind::Task
                ) && facts.finished.is_none()
            })
            .count() as u64;
        Boundary::Now {
            wake: WakeFacts {
                trigger: why.map_or(WakeTrigger::Delivery, trigger),
                events: Vec::new(),
                foreground_running: running,
                background_running: 0,
                tools_suppressed: false,
                checkin_at,
            },
        }
    }

    /// Answers the open call with everything waiting.
    pub(crate) fn answer_pending(&mut self) -> Option<(PendingExec, Drained)> {
        let pending = self.pending.clone()?;
        let failed = self
            .latest
            .as_ref()
            .and_then(|(_, cell)| cell.facts().finished)
            .is_some_and(|end| end.failed);
        let output = self
            .report()
            .unwrap_or_else(|| output(String::new(), Vec::new()));
        let output = ToolOutput {
            status: if failed {
                ToolOutputStatus::Error
            } else {
                ToolOutputStatus::Success
            },
            ..output
        };
        Some((
            pending.clone(),
            Drained {
                own: Some((pending.exec_id, output)),
                updates: Vec::new(),
            },
        ))
    }

    /// Everything waiting, for a model with no call open.
    pub(crate) fn drain_idle(&mut self) -> Drained {
        let id = self.latest.as_ref().map(|(id, _)| id.clone());
        Drained {
            own: None,
            updates: self
                .report()
                .map(|output| {
                    (
                        id.unwrap_or_else(|| {
                            ExecId::try_from("notebook".to_owned()).expect("valid id")
                        }),
                        output,
                    )
                })
                .into_iter()
                .collect(),
        }
    }

    /// The notebook's report, which it forgets once taken.
    fn report(&mut self) -> Option<ToolOutput> {
        if let Some((_, cell)) = &self.latest
            && cell.facts().finished.is_some()
        {
            self.progress.told_returned = true;
        }
        let report = self.notebook.report()?.render();
        Some(output(
            report.text,
            report
                .images
                .into_iter()
                .map(|image| ImageContent {
                    media_type: image.media_type,
                    data: image.data,
                    detail: Default::default(),
                })
                .collect(),
        ))
    }

    /// The transport (or durable outbox) now owns what was drained.
    pub(crate) fn acknowledge(&mut self) {
        self.pending = None;
    }

    /// Stops every cell and forgets the open call, which the caller answers
    /// or lets the CLI abandon. Until the user speaks again, nothing the
    /// cells say on their way out wakes the model.
    pub(crate) fn cancel(&mut self, _now: UnixMs) -> Option<PendingExec> {
        self.notebook.cancel();
        self.stopped = true;
        self.pending.take()
    }

    /// Forgets the open call without touching the cells: the CLI that was
    /// waiting on it is gone, the notebook is not.
    pub(crate) fn take_pending(&mut self) -> Option<PendingExec> {
        self.pending.take()
    }
}

fn output(text: String, images: Vec<ImageContent>) -> ToolOutput {
    ToolOutput {
        output: Arc::new(text),
        full_output: None,
        images: Arc::new(images),
        status: ToolOutputStatus::Success,
    }
}

/// How the wake reads in the log's terms.
fn trigger(why: Wake) -> WakeTrigger {
    match why {
        Wake::Message => WakeTrigger::User,
        Wake::AgentMessage => WakeTrigger::Mail,
        Wake::Notify => WakeTrigger::Notify,
        Wake::Returned | Wake::Failure => WakeTrigger::Finished,
        Wake::Checkin => WakeTrigger::Checkin,
        Wake::Prose
        | Wake::Restarted
        | Wake::Rewound
        | Wake::Compaction
        | Wake::CompactionReply => WakeTrigger::Asked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(directory: &std::path::Path) -> PythonHost {
        let notify = Arc::new(Notify::new());
        let notebook = Notebook::new(
            rho_tool_shell::ShellTools::in_directory(
                std::time::Duration::from_secs(5),
                directory.to_str().unwrap().into(),
                Default::default(),
            ),
            Vec::new(),
            Arc::clone(&notify),
        )
        .unwrap();
        PythonHost::new(notebook, notify)
    }

    async fn settle(host: &mut PythonHost) -> WakeFacts {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Boundary::Now { wake } =
                host.decide(true, None, None, false, false, UnixMs::now())
            {
                return wake;
            }
            assert!(tokio::time::Instant::now() < deadline, "never woke");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn only_the_latest_cell_send_retires_its_draft() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path());
        host.exec(
            "request".into(),
            serde_json::json!(1),
            "call".try_into().unwrap(),
            "print('ran')".into(),
            UnixMs::now(),
        );
        let cell = host.latest.as_ref().unwrap().1.source_id();
        assert_eq!(host.latest_call(), Some("call"));
        assert!(
            !host.sent(cell + 1),
            "a different cell cannot retire this draft"
        );
        assert_eq!(host.published_call(), None);
        assert!(host.sent(cell));
        assert_eq!(host.published_call(), Some("call"));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_open_call_returns_when_its_cell_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path());
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
        let wake = settle(&mut host).await;
        assert_eq!(wake.trigger, WakeTrigger::Finished);
        let (pending, drained) = host.answer_pending().unwrap();
        assert_eq!(pending.exec_id.as_str(), "call");
        let (_, output) = drained.own.unwrap();
        assert!(output.output.contains("ran"), "{}", output.output);
        assert_eq!(output.status, ToolOutputStatus::Success);
        host.acknowledge();
        assert!(host.can_admit());
        // Told once: the finished cell does not wake the model again.
        assert!(matches!(
            host.decide(true, None, None, false, false, UnixMs::now()),
            Boundary::No { .. }
        ));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_refuses_exec_until_the_user_speaks_and_retained_output_still_goes_out() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path());
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
            host.decide(true, None, None, false, false, UnixMs(11)),
            Boundary::No { recheck: None }
        ));
        assert!(matches!(
            host.decide(true, None, None, false, true, UnixMs(11)),
            Boundary::Now { wake } if wake.trigger == WakeTrigger::Delivery
        ));
        host.user_spoke();
        assert!(host.can_admit());
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn prose_correction_is_bounded_and_only_human_revives_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path());
        for count in 1..=Progress::MAX_PROSE {
            host.turn_ended(UnixMs(count as u64 * 1000), false);
            assert_eq!(host.prose_correction(), count < Progress::MAX_PROSE);
        }
        assert!(matches!(
            host.decide(true, None, Some(UnixMs(0)), false, false, UnixMs(100_000)),
            Boundary::No { recheck: None }
        ));
        assert_eq!(host.checkin_at(), None);
        host.user_spoke();
        assert!(!host.prose_correction());
        assert!(matches!(
            host.decide(true, None, Some(UnixMs(0)), false, false, UnixMs(100_000)),
            Boundary::Now { wake } if wake.trigger == WakeTrigger::Mail
        ));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_ended_turn_still_answers_its_call_then_wakes_only_for_news() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path());
        assert!(
            host.exec(
                "request".into(),
                serde_json::json!(1),
                "call".try_into().unwrap(),
                "print('done')".into(),
                UnixMs(10_000),
            )
            .is_none()
        );
        host.end_turn();
        let wake = settle(&mut host).await;
        assert_eq!(wake.trigger, WakeTrigger::Finished);
        assert!(host.answer_pending().is_some());
        assert!(host.ended(), "the answer carries the end of the turn");
        host.acknowledge();
        host.turn_ended(UnixMs(11_000), true);
        assert_eq!(host.checkin_at(), None);
        assert!(matches!(
            host.decide(true, None, None, false, false, UnixMs(1_000_000)),
            Boundary::No { recheck: None }
        ));
        assert!(matches!(
            host.decide(true, None, Some(UnixMs(12_000)), false, false, UnixMs(1_000_000)),
            Boundary::Now { wake } if wake.trigger == WakeTrigger::Mail
        ));
        // The next exec is a new turn.
        assert!(
            host.exec(
                "request".into(),
                serde_json::json!(2),
                "next".try_into().unwrap(),
                "pass".into(),
                UnixMs(20_000),
            )
            .is_none()
        );
        assert!(!host.ended());
        assert_eq!(host.checkin_at(), Some(UnixMs(140_000)));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn checkin_continues_after_result_without_an_open_mcp_call() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path());
        host.turn_ended(UnixMs(10_000), true);
        assert_eq!(host.checkin_at(), Some(UnixMs(130_000)));
        assert!(matches!(
            host.decide(true, None, None, false, false, UnixMs(129_999)),
            Boundary::No {
                recheck: Some(UnixMs(130_000))
            }
        ));
        assert!(
            matches!(host.decide(true, None, None, false, false, UnixMs(130_000)), Boundary::Now { wake } if wake.trigger == WakeTrigger::Checkin)
        );
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retirement_requires_no_running_cells_and_no_armed_checkin() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = host(temp.path());
        assert!(host.retire_settled());
        host.turn_ended(UnixMs(10), true);
        assert!(
            !host.retire_settled(),
            "CLI idle still owes a notebook check-in"
        );
        host.failed(UnixMs(20), Arc::from("stopped"));
        assert!(host.retire_settled());
        host.user_spoke();
        assert!(!host.retire_settled());
        host.shutdown().await.unwrap();
    }

    #[test]
    fn drained_output_becomes_mcp_content() {
        let output = |text: &str, status| ToolOutput {
            output: Arc::new(text.to_owned()),
            full_output: None,
            images: Arc::new(vec![ImageContent {
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
    }
}
