//! The agent hosts this frame is attached to.
//!
//! A host is dialled once attached and dialled again whenever it drops;
//! each system keeps its own streams to it, opened on every connect, and
//! the ledger rides along. What is attached is written down, so the next
//! start attaches the same hosts.

use std::rc::Rc;

use gpui::Window;
use rho_agent_hosts::connection::{ConnEvent, GitApprovalDecision};
use rho_agent_hosts::hosts::{HostStatus, Hosts};
use rho_agent_hosts::{HostId, HostSpec};
use rho_window::style::StyleClass;

use crate::frame::Frame;
use crate::minibuffer::Candidate;

impl Frame {
    /// Attaches an agent host: every system gets its streams before any
    /// frame from it can arrive.
    pub fn attach_host(&mut self, spec: HostSpec, cx: &mut gpui::Context<Self>) -> HostId {
        // The command line and the saved set can name one machine twice; it
        // is attached once, under the first name.
        if let Some(held) = self.hosts.by_key(&spec.target.key()) {
            tracing::warn!(host = %spec.name, "this machine is already attached; skipped");
            return held.id;
        }
        let systems = self.systems.clone();
        let ledger = self.attention.stream();
        let runtime = gpui_tokio::Tokio::handle(cx);
        let name = spec.name.clone();
        let endpoint = spec.target.endpoint();
        let host = self.hosts.attach(
            spec.name.clone(),
            spec.target,
            |host, link| {
                let mut opened = vec![ledger];
                for system in &systems {
                    opened.extend(system.streams(host, &name, endpoint, &link, cx));
                }
                opened
            },
            &runtime,
        );
        self.save_hosts();
        cx.notify();
        host
    }

    /// Writes the attached set down, so that the next start attaches the
    /// same hosts in the same order.
    fn save_hosts(&self) {
        let specs = self
            .hosts
            .iter()
            .map(|host| HostSpec {
                name: host.name.clone(),
                target: host.target.clone(),
            })
            .collect::<Vec<_>>();
        rho_agent_hosts::saved::save(&self.db, &specs);
    }

    pub fn detach_host(&mut self, host: HostId, cx: &mut gpui::Context<Self>) {
        for system in self.systems.clone() {
            system.detached(host, cx);
        }
        self.hosts.detach(host);
        self.save_hosts();
        self.systems_changed(cx);
    }

    pub fn hosts(&self) -> &Hosts {
        &self.hosts
    }

    /// A pairing link (`rho://…`) arrived: asks the user what to call the
    /// host it reaches, and attaches it under that name. Nothing attaches
    /// without them.
    pub fn prompt_link_name(
        &mut self,
        link: String,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.open_prompt(
            "name this host:",
            std::rc::Rc::new(|_, _, _| Vec::new()),
            std::rc::Rc::new(move |frame, name, _, cx| {
                let name = name.trim();
                match HostSpec::parse(&format!("{name}={link}"), "rho") {
                    Ok(spec) => {
                        frame.attach_host(spec, cx);
                    }
                    Err(error) => {
                        frame.echo(&format!("attach: {error}"), StyleClass::SystemInfo, cx)
                    }
                }
            }),
            window,
            cx,
        );
    }

    pub(crate) fn host_event(
        &mut self,
        host: HostId,
        event: ConnEvent,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let name = self.hosts.host_label(host);
        match event {
            ConnEvent::Ready | ConnEvent::Recovered => {
                self.hosts.set_status(host, HostStatus::Online);
            }
            ConnEvent::Many(events) => {
                for event in events {
                    self.host_event(host, event, window, cx);
                }
                return;
            }
            ConnEvent::ServerError(message) => {
                self.echo(
                    &format!("{name}: {message}"),
                    StyleClass::SystemImportant,
                    cx,
                );
            }
            ConnEvent::Recovering(elapsed) => {
                self.hosts.set_status(host, HostStatus::Recovering(elapsed));
            }
            ConnEvent::Disconnected(reason) => {
                self.echo(
                    &format!("{name}: disconnected · {reason}"),
                    StyleClass::Disconnect,
                    cx,
                );
                self.hosts
                    .set_status(host, HostStatus::Disconnected(reason));
            }
            ConnEvent::Unenrolled { command } => {
                let status = HostStatus::Unenrolled(command);
                self.echo(
                    &format!("{name}: {}", status.label()),
                    StyleClass::Disconnect,
                    cx,
                );
                self.hosts.set_status(host, status);
            }
            ConnEvent::GitTransportApproval {
                request_id,
                agent,
                asks,
                response,
            } => {
                let who = match agent {
                    Some(agent) => self
                        .systems
                        .iter()
                        .find_map(|system| system.agent_name(agent, cx))
                        .unwrap_or_else(|| format!("agent {} on {name}", agent.encoded())),
                    None => name,
                };
                self.questions.push_back(HostQuestion {
                    key: QuestionKey {
                        host,
                        request: request_id,
                    },
                    prompt: format!("{who} · git asks: {asks} · allow or deny:"),
                    response,
                });
                if !self.ask_next(window, cx) {
                    self.echo(
                        &format!("{who} asks to use your ssh login · after this prompt"),
                        StyleClass::SystemImportant,
                        cx,
                    );
                }
            }
            ConnEvent::GitTransportDone { request_id } => {
                // Answered, here or on another device, or timed out.
                let key = QuestionKey {
                    host,
                    request: request_id,
                };
                self.questions.retain(|question| question.key != key);
                if self
                    .minibuffer
                    .as_ref()
                    .is_some_and(|prompt| prompt.question == Some(key))
                {
                    self.minibuffer_cancel(window, cx);
                }
            }
        }
        cx.notify();
    }

    /// What picking a host in the palette does: a host that does not
    /// trust this device puts its enrollment command on the clipboard, to
    /// run on the host; one that is away dials again now; one that answers
    /// says so.
    pub(crate) fn pick_host(&mut self, host: HostId, cx: &mut gpui::Context<Self>) {
        let Some(entry) = self.hosts.get(host) else {
            return;
        };
        let name = entry.name.clone();
        let said = match &entry.status {
            HostStatus::Unenrolled(command) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(command.clone()));
                format!("copied: {command} · run it on {name}")
            }
            HostStatus::Disconnected(_) | HostStatus::Recovering(_) => {
                entry.connection().retry_now();
                format!("{name}: dialing again")
            }
            status @ (HostStatus::Online | HostStatus::Connecting) => {
                format!("{name}: {}", status.label())
            }
        };
        self.echo(&said, StyleClass::SystemInfo, cx);
    }

    /// Asks the oldest question a host waits on, unless a prompt or a menu
    /// has the keys: then it waits for them to close. Says whether the
    /// question is now asked.
    pub(crate) fn ask_next(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> bool {
        if self.is_asking() {
            return false;
        }
        // A host that stopped waiting has nobody to answer.
        self.questions
            .retain(|question| !question.response.is_closed());
        let Some(question) = self.questions.front() else {
            return false;
        };
        let key = question.key;
        self.open_prompt_cancellable(
            question.prompt.clone(),
            Rc::new(|_, _, _| {
                vec![
                    Candidate {
                        value: ALLOW.to_owned(),
                        description: "let it use your ssh login".to_owned(),
                    },
                    Candidate {
                        value: DENY.to_owned(),
                        description: "refuse".to_owned(),
                    },
                ]
            }),
            Rc::new(move |frame, answer, _, _| {
                let decision = match answer.trim() {
                    ALLOW => GitApprovalDecision::Allow,
                    _ => GitApprovalDecision::Deny,
                };
                frame.answer_question(key, decision);
            }),
            Some(Rc::new(move |frame, _, _| {
                frame.answer_question(key, GitApprovalDecision::Deny)
            })),
            window,
            cx,
        );
        let prompt = self.minibuffer.as_mut().expect("the prompt opened");
        prompt.question = Some(key);
        // Enter takes only a word the user typed or a row they chose, so a
        // stray `y` and enter, typed for the screen under it, answer nothing.
        prompt.complete_only_when_chosen();
        prompt.validate(|answer| {
            (![ALLOW, DENY].contains(&answer.trim())).then_some("type allow or deny")
        });
        true
    }

    fn answer_question(&mut self, key: QuestionKey, decision: GitApprovalDecision) {
        let Some(index) = self
            .questions
            .iter()
            .position(|question| question.key == key)
        else {
            return;
        };
        let question = self.questions.remove(index).expect("the question is here");
        let _ = question.response.send(decision);
    }
}

const ALLOW: &str = "allow";
const DENY: &str = "deny";

/// Which question: a host's request, as the host numbers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QuestionKey {
    host: HostId,
    request: u64,
}

/// A host's question for the user, which waits its turn for the prompt:
/// the oldest is asked first, and none is asked over a prompt the user
/// opened. Answered once, then gone; dropping it refuses.
pub(crate) struct HostQuestion {
    key: QuestionKey,
    prompt: String,
    response: tokio::sync::oneshot::Sender<GitApprovalDecision>,
}
