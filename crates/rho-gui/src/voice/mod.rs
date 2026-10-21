//! The voice session: whether the desk is listening, to which agent, and
//! whether the microphone is open.
//!
//! Voice is one session for the whole client and it belongs to one agent,
//! the one whose thread the user started it from, and so to that agent's
//! host — the agent host does the listening, so the session dies when that
//! agent host goes. It also survives its own failures: a session the user asked
//! for is started again when its task ends unexpectedly, which is why wanting
//! a session and having one are two different facts here.
//!
//! The task itself is spawned by the host, because it reports back into the
//! workspace when it ends. What is held here is everything that says what
//! to do about that.

pub(crate) mod rtc;

use rho_agent_hosts::HostId;
use rho_agent_types::AgentId;

/// A session that is actually running.
struct Running {
    /// Kept so dropping this state stops the session.
    _task: gpui::Task<()>,
    /// Taken when a stop is asked for. The task is what ends, and it ends
    /// a moment later, so this is `None` while a stop is in flight.
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    input_muted: tokio::sync::watch::Sender<bool>,
    /// Whether the media and the sideband are both up, so the user is heard.
    connected: bool,
}

/// What the user is shown of a session they asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    /// Asked for, not heard yet: starting, or starting again after a
    /// failure.
    Connecting,
    Listening,
    Muted,
}

/// What the desk knows about voice.
#[derive(Default)]
pub(crate) struct Voice {
    running: Option<Running>,
    /// Whether the microphone is closed. Kept across a restart: a session
    /// that comes back after a failure comes back muted if it was muted.
    muted: bool,
    /// Whether the user wants a session at all, as opposed to having one.
    /// A session that ends on its own is started again; one the user ended
    /// is not.
    wanted: bool,
    /// The agent the user talks to, and the agent host running the session.
    /// The session is torn down with that host.
    target: Option<(HostId, AgentId)>,
}

impl Voice {
    /// Whether a session is running now.
    pub(crate) fn running(&self) -> bool {
        self.running.is_some()
    }

    /// Whether the user has asked for a session and not ended it.
    pub(crate) fn wanted(&self) -> bool {
        self.wanted
    }

    /// The agent the session talks to and the agent host it is on, if
    /// there is one.
    pub(crate) fn target(&self) -> Option<(HostId, AgentId)> {
        self.target
    }

    /// Whether voice is running on this host, so a host going away knows
    /// whether it is taking the session with it.
    pub(crate) fn is_on(&self, host: HostId) -> bool {
        self.target.is_some_and(|(on, _)| on == host)
    }

    /// The agent a session the user wants talks to, and how it is.
    /// `None` once the user has ended it, even while it is still stopping.
    pub(crate) fn shown(&self) -> Option<(AgentId, Shown)> {
        let (_, agent) = self.target.filter(|_| self.wanted)?;
        let connected = self.running.as_ref().is_some_and(|it| it.connected);
        Some((
            agent,
            match (connected, self.muted) {
                (false, _) => Shown::Connecting,
                (true, false) => Shown::Listening,
                (true, true) => Shown::Muted,
            },
        ))
    }

    /// The running session is up: the user is heard from now on.
    pub(crate) fn connected(&mut self) {
        if let Some(running) = &mut self.running {
            running.connected = true;
        }
    }

    /// Whether the microphone is closed.
    pub(crate) fn muted(&self) -> bool {
        self.muted
    }

    /// Opens or closes the microphone, tells the running session, and
    /// answers with what it now is.
    pub(crate) fn toggle_mute(&mut self) -> bool {
        self.muted = !self.muted;
        if let Some(running) = &self.running {
            running.input_muted.send_replace(self.muted);
        }
        self.muted
    }

    /// Opens the microphone, for a session about to start: a new session
    /// starts listening.
    pub(crate) fn unmute(&mut self) {
        self.muted = false;
    }

    /// The user has asked for a session with `agent` on `host`. Recorded
    /// before the session exists, and kept if it fails to start: which agent
    /// voice belongs to is the ask, not the outcome.
    pub(crate) fn wants(&mut self, host: HostId, agent: AgentId) {
        self.target = Some((host, agent));
        self.wanted = true;
    }

    /// Records a session that has just started.
    pub(crate) fn started(
        &mut self,
        task: gpui::Task<()>,
        stop: tokio::sync::oneshot::Sender<()>,
        input_muted: tokio::sync::watch::Sender<bool>,
    ) {
        self.running = Some(Running {
            _task: task,
            stop: Some(stop),
            input_muted,
            connected: false,
        });
    }

    /// Asks the running session to stop. It ends a moment later, on its
    /// own, and says so through [`Self::finished`].
    pub(crate) fn stop(&mut self) {
        if let Some(stop) = self.running.as_mut().and_then(|it| it.stop.take()) {
            let _ = stop.send(());
        }
    }

    /// The user has ended the session: it is not to be started again.
    pub(crate) fn end(&mut self) {
        self.wanted = false;
    }

    /// The session has ended, however it ended.
    pub(crate) fn finished(&mut self) {
        self.running = None;
    }
}
