//! The agent hosts this frame is attached to.
//!
//! A host is dialled once attached and dialled again whenever it drops;
//! each system keeps its own streams to it, opened on every connect, and
//! the ledger rides along. What is attached is written down, so the next
//! start attaches the same hosts.

use std::future::Future;
use std::rc::Rc;

use gpui::{Context, Window};
use rho_agent_hosts::connection::ConnEvent;
use rho_agent_hosts::hosts::{HostStatus, Hosts};
use rho_agent_hosts::{AttachTarget, HostId, HostSpec, HostStream, InProcessHost, Link};
use rho_window::style::StyleClass;

use crate::command::{Command, Spelling};
use crate::frame::Frame;
use crate::minibuffer::Candidate;

pub(crate) static ATTACH_HOST: Spelling = Spelling {
    name: "attach host",
    aliases: &[
        ("attach", 9),
        ("connect host", 8),
        ("add host", 8),
        ("connect", 6),
        ("ssh", 4),
    ],
    argument: Some("name=local, name=iroh:<id>@<ssh host> or name=iroh:<id>"),
};

pub(crate) static DETACH_HOST: Spelling = Spelling {
    name: "detach host",
    aliases: &[
        ("detach", 9),
        ("disconnect host", 8),
        ("remove host", 8),
        ("disconnect", 6),
        ("forget host", 5),
    ],
    argument: Some("host"),
};

pub(crate) static LIST_HOSTS: Spelling = Spelling {
    name: "list hosts",
    aliases: &[
        ("hosts", 9),
        ("machines", 6),
        ("reconnect", 6),
        ("enroll", 5),
        ("status", 3),
    ],
    argument: None,
};

/// How the window shows a host (`hosts.Host`). An away host is an
/// everyday state the window says calmly; only a host that needs the
/// user's hand, or an away one the screen in view needs, is an alarm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    Online,
    /// Dialling, before the host first answers.
    Connecting,
    /// Dropped or gone quiet; the window dials again by itself.
    Away,
    /// It will not answer until the user does something: enrol this device.
    NeedsYou,
}

impl Reach {
    pub(crate) fn of(status: &HostStatus) -> Self {
        match status {
            HostStatus::Online => Self::Online,
            HostStatus::Connecting => Self::Connecting,
            HostStatus::Recovering(_) | HostStatus::Disconnected(_) => Self::Away,
            HostStatus::Unenrolled(_) => Self::NeedsYou,
        }
    }
}

/// A host's state in the user's words, with why when it is away or needs
/// them: `online`, `away · connection lost`, `needs you · enroll this
/// device on the host: rho enroll …`. Never how long, so nothing ticks.
pub(crate) fn status_words(status: &HostStatus) -> String {
    match status {
        HostStatus::Online => "online".to_owned(),
        HostStatus::Connecting => "connecting".to_owned(),
        HostStatus::Recovering(_) => "away · reconnecting".to_owned(),
        HostStatus::Disconnected(reason) => format!("away · {reason}"),
        HostStatus::Unenrolled(command) => {
            format!("needs you · enroll this device on the host: {command}")
        }
    }
}

/// One part of the status line's host notice, and whether it is an alarm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub words: String,
    pub alarm: bool,
}

/// The status line's notice of the hosts that do not answer
/// (`window.Status line`): hosts that need the user, as an alarm; an away
/// host the screen in view lives on (`needed`), as an alarm; the rest away
/// or connecting, calmly. Nothing while every host answers.
pub(crate) fn notice<'a>(
    hosts: impl IntoIterator<Item = (&'a str, Reach)>,
    needed: Option<&str>,
) -> Vec<Notice> {
    let mut needs_you = Vec::new();
    let mut away = Vec::new();
    let mut connecting = Vec::new();
    let mut alarm = None;
    for (name, reach) in hosts {
        match reach {
            Reach::Online => {}
            Reach::NeedsYou => needs_you.push(name),
            Reach::Away if needed == Some(name) => alarm = Some(name),
            Reach::Away => away.push(name),
            Reach::Connecting => connecting.push(name),
        }
    }
    let group = |names: &[&str], one: &str, many: &str| match names {
        [] => None,
        [name] => Some(format!("{name} {one}")),
        names => Some(format!("{} hosts {many}", names.len())),
    };
    let calm: Vec<String> = group(&away, "away", "away")
        .into_iter()
        .chain(group(&connecting, "connecting", "connecting"))
        .collect();
    group(&needs_you, "needs you", "need you")
        .into_iter()
        .chain(alarm.map(|name| format!("{name} away")))
        .map(|words| Notice { words, alarm: true })
        .chain((!calm.is_empty()).then(|| Notice {
            words: calm.join(" · "),
            alarm: false,
        }))
        .collect()
}

/// What attaching a host asks, and what its answer reads as.
const ATTACH_QUESTION: &str = "attach host · name=local (this machine), \
     name=iroh:<id>@<ssh host> (ssh), or name=iroh:<id> (security key):";

/// The commands about the hosts this window is attached to.
pub(crate) fn commands(frame: &Frame) -> Vec<Command> {
    let hosts = frame.hosts();
    let detach_target = match hosts.iter().collect::<Vec<_>>().as_slice() {
        [] => "no host attached".to_owned(),
        [host] => host.name.clone(),
        _ => "pick host…".to_owned(),
    };
    let listed = match hosts.worst_status() {
        None => "no host attached".to_owned(),
        Some((name, status)) if hosts.len() == 1 => format!("{name} · {}", status_words(&status)),
        Some(_) => format!("{} attached", hosts.len()),
    };
    vec![
        Command::new(&ATTACH_HOST, |frame, argument, window, cx| match argument {
            Some(spec) => attach(frame, &spec, cx),
            None => frame.open_prompt(
                ATTACH_QUESTION,
                Rc::new(|_, _, _| Vec::new()),
                Rc::new(|frame, input, _, cx| attach(frame, &input, cx)),
                window,
                cx,
            ),
        }),
        Command::new(&DETACH_HOST, |frame, argument, window, cx| match argument {
            Some(name) => detach(frame, &name, cx),
            None => frame.open_prompt(
                "detach host:",
                Rc::new(|frame, needle, _| host_candidates(frame, needle)),
                Rc::new(|frame, input, _, cx| detach(frame, &input, cx)),
                window,
                cx,
            ),
        })
        .target(detach_target),
        Command::simple(&LIST_HOSTS, |frame, window, cx| {
            if frame.hosts().len() == 0 {
                return frame.echo(
                    "list hosts: no agent host is attached",
                    StyleClass::SystemInfo,
                    cx,
                );
            }
            frame.open_prompt(
                "hosts · enter dials, enrolls or says how it is:",
                Rc::new(|frame, needle, _| host_candidates(frame, needle)),
                Rc::new(|frame, input, _, cx| {
                    let name = input.trim();
                    match frame.hosts().by_name(name).map(|host| host.id) {
                        Some(host) => frame.pick_host(host, cx),
                        None => frame.echo(
                            &format!("list hosts: no host called {name}"),
                            StyleClass::SystemInfo,
                            cx,
                        ),
                    }
                }),
                window,
                cx,
            );
        })
        .target(listed),
    ]
}

fn attach(frame: &mut Frame, spec: &str, cx: &mut Context<Frame>) {
    match HostSpec::parse(spec.trim(), "rho") {
        Ok(spec) => {
            frame.attach_host(spec, cx);
        }
        Err(error) => frame.echo(&format!("attach host: {error}"), StyleClass::SystemInfo, cx),
    }
}

fn detach(frame: &mut Frame, name: &str, cx: &mut Context<Frame>) {
    let name = name.trim();
    match frame.hosts().by_name(name).map(|host| host.id) {
        Some(host) => {
            frame.detach_host(host, cx);
            frame.echo(&format!("detached: {name}"), StyleClass::SystemInfo, cx);
        }
        None => frame.echo(
            &format!("detach host: no host called {name}"),
            StyleClass::SystemInfo,
            cx,
        ),
    }
}

/// The attached hosts whose names hold `needle`, each with how it is.
fn host_candidates(frame: &Frame, needle: &str) -> Vec<Candidate> {
    frame
        .hosts()
        .iter()
        .filter(|host| host.name.contains(needle.trim()))
        .map(|host| Candidate {
            value: host.name.clone(),
            description: status_words(&host.status),
        })
        .collect()
}

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
        let runtime = gpui_tokio::Tokio::handle(cx);
        self.attach_with(spec, cx, |hosts, name, target, streams| {
            hosts.attach(name, target, streams, &runtime)
        })
    }

    /// Attaches a host that lives in this process, for a test: nothing
    /// dials it, and every stream opened to it arrives on the receiver,
    /// far end first, for the test to answer as the host would.
    pub fn attach_host_in_process(
        &mut self,
        spec: HostSpec,
        cx: &mut gpui::Context<Self>,
    ) -> (HostId, InProcessHost) {
        let mut opened = None;
        let host = self.attach_with(spec, cx, |hosts, name, target, streams| {
            let (host, streams) = hosts.attach_in_process(name, target, streams);
            opened = Some(streams);
            host
        });
        (host, opened.expect("attach_with attaches"))
    }

    fn attach_with(
        &mut self,
        spec: HostSpec,
        cx: &mut gpui::Context<Self>,
        attach: impl FnOnce(&mut Hosts, String, AttachTarget, HostStreams<'_>) -> HostId,
    ) -> HostId {
        let systems = self.systems.clone();
        let ledger = self.attention.stream();
        let name = spec.name.clone();
        let endpoint = spec.target.endpoint();
        let host = attach(
            &mut self.hosts,
            spec.name,
            spec.target,
            Box::new(|host, link| {
                let mut opened = vec![ledger];
                for system in &systems {
                    opened.extend(system.streams(host, &name, endpoint, &link, cx));
                }
                opened
            }),
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
        cx: &mut gpui::Context<Self>,
    ) {
        let name = self.hosts.host_label(host);
        match event {
            ConnEvent::Ready | ConnEvent::Recovered => {
                self.hosts.set_status(host, HostStatus::Online);
                let prefix = format!("{name} away · ");
                self.clear_echo_if(|text| text.starts_with(&prefix));
            }
            ConnEvent::Many(events) => {
                for event in events {
                    self.host_event(host, event, cx);
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
                // Away is an everyday state: said once, calmly.
                self.echo(
                    &format!("{name} away · {reason}"),
                    StyleClass::SystemInfo,
                    cx,
                );
                self.hosts
                    .set_status(host, HostStatus::Disconnected(reason));
            }
            ConnEvent::Unenrolled { command } => {
                let status = HostStatus::Unenrolled(command);
                self.echo(
                    &format!("{name}: {}", status_words(&status)),
                    StyleClass::Disconnect,
                    cx,
                );
                self.hosts.set_status(host, status);
            }
        }
        cx.notify();
    }

    /// What the window calls an agent a host names, such as `eng-b8os`,
    /// when a system holds it.
    pub(crate) fn agent_name(
        &self,
        agent: rho_agent_types::AgentId,
        cx: &gpui::App,
    ) -> Option<String> {
        self.systems
            .iter()
            .find_map(|system| system.agent_name(agent, cx))
    }

    /// Runs `then` on the host a command means: the only one attached, or
    /// the one the user names at `host:`. With none attached the echo line
    /// says so, under `command`'s name (`window.commands are global`).
    pub fn choose_host(
        &mut self,
        command: &'static str,
        then: OnHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let only = match self.hosts.ids().as_slice() {
            [] => {
                self.echo(
                    &format!("{command}: no agent host is attached"),
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            }
            [host] => Some(*host),
            _ => None,
        };
        if let Some(host) = only {
            return then(self, host, window, cx);
        }
        self.open_prompt(
            "host:",
            Rc::new(|frame, needle, _| host_candidates(frame, needle)),
            Rc::new(move |frame, input, window, cx| {
                let name = input.trim();
                match frame.hosts.by_name(name).map(|host| host.id) {
                    Some(host) => then(frame, host, window, cx),
                    None => frame.echo(
                        &format!("{command}: no host called {name}"),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }),
            window,
            cx,
        );
    }

    /// Waits for `host`'s `answer` and hands it to `then`, which asks the
    /// user what to do with it. Meanwhile the echo line says whom it asked;
    /// a refusal or a lost host is echoed under `command`'s name. An answer
    /// that comes while the user has another prompt open does not
    /// take it from them.
    pub fn when_host_answers<T: 'static>(
        &mut self,
        command: &'static str,
        host: HostId,
        answer: impl Future<Output = anyhow::Result<T>> + 'static,
        then: impl FnOnce(&mut Frame, T, &mut Window, &mut Context<Frame>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self.hosts.host_label(host);
        self.echo(
            &format!("{command}: asking {name}…"),
            StyleClass::SystemInfo,
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            let _ = this.update_in(cx, |frame, window, cx| match answer {
                Err(error) => frame.echo(
                    &format!("{command}: {name}: {error:#}"),
                    StyleClass::StatusError,
                    cx,
                ),
                Ok(_) if frame.minibuffer.is_some() => frame.echo(
                    &format!(
                        "{command}: {name} answered while another prompt was open · run it again"
                    ),
                    StyleClass::SystemInfo,
                    cx,
                ),
                Ok(answer) => then(frame, answer, window, cx),
            });
        })
        .detach();
    }

    /// What choosing a host in `list hosts` does: a host that does not
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
                format!("{name}: {}", status_words(status))
            }
        };
        self.echo(&said, StyleClass::SystemInfo, cx);
    }
}

/// What a host gets its streams from, once it has an id and a link.
type HostStreams<'a> = Box<dyn FnOnce(HostId, Link) -> Vec<std::sync::Arc<dyn HostStream>> + 'a>;

/// What a host command does once it knows which host it means.
pub type OnHost = Rc<dyn Fn(&mut Frame, HostId, &mut Window, &mut Context<Frame>)>;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn notice_of(hosts: &[(&str, Reach)], needed: Option<&str>) -> Vec<(String, bool)> {
        notice(hosts.iter().copied(), needed)
            .into_iter()
            .map(|notice| (notice.words, notice.alarm))
            .collect()
    }

    fn calm(words: &str) -> (String, bool) {
        (words.to_owned(), false)
    }

    fn alarm(words: &str) -> (String, bool) {
        (words.to_owned(), true)
    }

    /// Dropped, gone quiet, or never enrolled: four states, in words that
    /// never count seconds.
    #[test]
    fn every_status_is_one_of_four_states_in_still_words() {
        for (status, reach, words) in [
            (HostStatus::Online, Reach::Online, "online"),
            (HostStatus::Connecting, Reach::Connecting, "connecting"),
            (
                HostStatus::Recovering(Duration::from_secs(12)),
                Reach::Away,
                "away · reconnecting",
            ),
            (
                HostStatus::Disconnected("connection lost".into()),
                Reach::Away,
                "away · connection lost",
            ),
            (
                HostStatus::Unenrolled("rho enroll 7f3".into()),
                Reach::NeedsYou,
                "needs you · enroll this device on the host: rho enroll 7f3",
            ),
        ] {
            assert_eq!(Reach::of(&status), reach, "{status:?}");
            assert_eq!(status_words(&status), words);
        }
        assert_eq!(
            status_words(&HostStatus::Recovering(Duration::from_secs(13))),
            status_words(&HostStatus::Recovering(Duration::from_secs(12))),
            "nothing ticks"
        );
    }

    /// An away host is everyday: a calm note on every screen, never red.
    #[test]
    fn an_away_host_is_a_calm_note() {
        assert!(notice_of(&[("lab", Reach::Online)], None).is_empty());
        assert_eq!(
            notice_of(&[("lab", Reach::Away), ("reef", Reach::Online)], None),
            [calm("lab away")]
        );
        assert_eq!(
            notice_of(
                &[
                    ("lab", Reach::Away),
                    ("reef", Reach::Away),
                    ("sea", Reach::Connecting)
                ],
                Some("kelp")
            ),
            [calm("2 hosts away · sea connecting")],
            "a screen on another host needs none of them"
        );
    }

    /// Red only for a host the screen in view needs, and for one that
    /// needs the user's hand, wherever they are.
    #[test]
    fn red_is_for_the_host_in_view_and_for_one_that_needs_you() {
        assert_eq!(
            notice_of(&[("lab", Reach::Away), ("reef", Reach::Away)], Some("lab")),
            [alarm("lab away"), calm("reef away")]
        );
        assert_eq!(
            notice_of(&[("lab", Reach::Connecting)], Some("lab")),
            [calm("lab connecting")],
            "dialling is not away"
        );
        assert_eq!(
            notice_of(&[("lab", Reach::NeedsYou), ("reef", Reach::Away)], None),
            [alarm("lab needs you"), calm("reef away")]
        );
        assert_eq!(
            notice_of(&[("lab", Reach::NeedsYou), ("reef", Reach::NeedsYou)], None),
            [alarm("2 hosts need you")]
        );
    }
}
