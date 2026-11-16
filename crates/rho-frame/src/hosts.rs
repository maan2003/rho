//! The agent hosts this frame is attached to.
//!
//! A host is dialled once attached and dialled again whenever it drops;
//! each system keeps its own streams to it, opened on every connect, and
//! the ledger rides along. What is attached is written down, so the next
//! start attaches the same hosts.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::Window;
use rho_agent_hosts::connection::{ConnEvent, GitApprovalDecision};
use rho_agent_hosts::hosts::{HostStatus, Hosts};
use rho_agent_hosts::{HostId, HostSpec};
use rho_window::style::StyleClass;

use crate::frame::Frame;

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
                self.log(
                    format!("{name}: {message}"),
                    StyleClass::SystemImportant,
                    cx,
                );
            }
            ConnEvent::Recovering(elapsed) => {
                self.hosts.set_status(host, HostStatus::Recovering(elapsed));
            }
            ConnEvent::Disconnected(reason) => {
                self.log(
                    format!("{name}: disconnected · {reason}"),
                    StyleClass::Disconnect,
                    cx,
                );
                self.hosts
                    .set_status(host, HostStatus::Disconnected(reason));
            }
            ConnEvent::GitTransportApproval {
                prompt, response, ..
            } => {
                // The host's git transport wants the user's key for a push
                // or a fetch: the user says yes in the minibuffer, and
                // anything else, including escape, is no.
                let response = Rc::new(RefCell::new(Some(response)));
                let answer = |decision: GitApprovalDecision| {
                    let response = response.clone();
                    move || {
                        if let Some(response) = response.borrow_mut().take() {
                            let _ = response.send(decision);
                        }
                    }
                };
                let allow = answer(GitApprovalDecision::Allow);
                let deny = answer(GitApprovalDecision::Deny);
                let deny_on_cancel = answer(GitApprovalDecision::Deny);
                self.open_prompt_cancellable(
                    format!("{name}: git asks `{prompt}` · allow? (y/n):"),
                    Rc::new(|_, _, _| Vec::new()),
                    Rc::new(move |_, input, _, _| {
                        if input.trim().eq_ignore_ascii_case("y") {
                            allow()
                        } else {
                            deny()
                        }
                    }),
                    Some(Rc::new(move |_, _, _| deny_on_cancel())),
                    window,
                    cx,
                );
            }
            ConnEvent::GitTransportDone { .. } => {}
        }
        cx.notify();
    }
}
