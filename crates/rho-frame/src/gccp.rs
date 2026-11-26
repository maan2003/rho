//! What the user says about GCCP on a host: the other harnesses whose
//! agents the host's agents may talk to, and the addresses on them its
//! agents accepted. The same as `rho gccp` and
//! `rho iroh approve --role gccp` on the host, from `Commands` on any
//! screen (`hosts.Host`'s `gccp`).

use std::future::Future;
use std::rc::Rc;

use gpui::{Context, Window};
use rho_agent_hosts::HostId;
use rho_agent_hosts::protocol::{GccpApprove, GccpContact, GccpList, GccpRemove, GccpState};
use rho_agent_types::AgentId;
use rho_window::style::StyleClass;

use crate::command::{Command, Spelling};
use crate::frame::Frame;
use crate::minibuffer::Candidate;

const LIST: &str = "list gccp agents";
const APPROVE: &str = "approve gccp agent";
const REMOVE: &str = "remove gccp agent";

pub static LIST_GCCP: Spelling = Spelling {
    name: LIST,
    aliases: &[("gccp", 8), ("harnesses", 6), ("endpoint", 5)],
    argument: None,
};

pub static APPROVE_GCCP: Spelling = Spelling {
    name: APPROVE,
    aliases: &[("trust harness", 7), ("approve harness", 7), ("gccp", 5)],
    argument: None,
};

pub static REMOVE_GCCP: Spelling = Spelling {
    name: REMOVE,
    aliases: &[("forget gccp address", 7), ("revoke gccp", 7), ("gccp", 4)],
    argument: None,
};

/// The gccp commands, each asking for a host when more than one is
/// attached.
pub(crate) fn commands() -> Vec<Command> {
    vec![
        Command::simple(&LIST_GCCP, |frame, window, cx| {
            frame.choose_host(LIST, Rc::new(list), window, cx)
        }),
        Command::simple(&APPROVE_GCCP, |frame, window, cx| {
            frame.choose_host(APPROVE, Rc::new(approve), window, cx)
        }),
        Command::simple(&REMOVE_GCCP, |frame, window, cx| {
            frame.choose_host(REMOVE, Rc::new(remove), window, cx)
        }),
    ]
}

/// Everything the host says of GCCP, one row each; enter copies the row's
/// id, which is what the other harness's owner or `rho gccp` asks for.
fn list(frame: &mut Frame, host: HostId, window: &mut Window, cx: &mut Context<Frame>) {
    let answer = frame.hosts().call(host, GccpList);
    frame.when_host_answers(
        LIST,
        host,
        answer,
        move |frame, state, window, cx| {
            let name = frame.hosts().host_label(host);
            let rows = Rc::new(listed(frame, &name, state, cx));
            let offered = rows.clone();
            frame.open_prompt(
                format!("gccp on {name} · enter copies:"),
                Rc::new(move |_, needle, _| matching(&offered, needle)),
                Rc::new(move |frame, input, _, cx| {
                    let input = input.trim();
                    match rows.iter().any(|row| row.value == input) {
                        true => {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                input.to_owned(),
                            ));
                            frame.echo(&format!("copied: {input}"), StyleClass::SystemInfo, cx);
                        }
                        false => frame.echo(
                            &format!("{LIST}: nothing on {name} is {input:?}"),
                            StyleClass::SystemInfo,
                            cx,
                        ),
                    }
                }),
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

/// The rows of `gccp list`: the host's own endpoint, each harness the user
/// approved, and each address an agent accepted on one.
fn listed(frame: &Frame, name: &str, state: GccpState, cx: &gpui::App) -> Vec<Candidate> {
    let GccpState {
        endpoint,
        hosts,
        contacts,
    } = state;
    let mut rows = Vec::new();
    rows.push(match endpoint {
        Some(endpoint) => Candidate {
            value: endpoint,
            description: format!("{name} · give it to another harness's owner to approve"),
        },
        None => Candidate {
            value: "no endpoint".to_owned(),
            description: format!("{name} has no iroh endpoint, so it speaks no gccp"),
        },
    });
    rows.extend(hosts.into_iter().map(|harness| Candidate {
        value: harness.endpoint,
        description: format!("approved harness {:?}", harness.name),
    }));
    rows.extend(contacts.into_iter().map(|contact| Candidate {
        description: format!(
            "{} accepted it as {:?}",
            agent_label(frame, &contact.agent, cx),
            contact.name
        ),
        value: contact.address,
    }));
    rows
}

/// Approves a harness under a name, given on one line: approving one
/// again renames it.
fn approve(frame: &mut Frame, host: HostId, window: &mut Window, cx: &mut Context<Frame>) {
    let name = frame.hosts().host_label(host);
    frame.open_prompt(
        format!("approve a gccp harness on {name} · <endpoint id> <name>:"),
        Rc::new(|_, _, _| Vec::new()),
        Rc::new(move |frame, input, _, cx| {
            let call = match parse_approve(&input) {
                Ok(call) => call,
                Err(reason) => {
                    frame.echo(&format!("{APPROVE}: {reason}"), StyleClass::SystemInfo, cx);
                    return;
                }
            };
            let said = format!(
                "{name}: approved gccp harness {} as {:?}",
                call.endpoint, call.name
            );
            let answer = frame.hosts().call(host, call);
            tell(frame, APPROVE, host, answer, said, cx);
        }),
        window,
        cx,
    );
}

/// `<endpoint id> <name>`: the name is the rest of the line, as the user
/// calls the harness.
fn parse_approve(input: &str) -> Result<GccpApprove, &'static str> {
    let (endpoint, name) = input
        .trim()
        .split_once(char::is_whitespace)
        .ok_or("give the harness's endpoint id, then a name for it")?;
    let name = name.trim();
    if name.is_empty() {
        return Err("give the harness's endpoint id, then a name for it");
    }
    Ok(GccpApprove {
        endpoint: endpoint.to_owned(),
        name: name.to_owned(),
    })
}

/// Takes back one agent's acceptance of an address, chosen from what the
/// host says its agents accepted.
fn remove(frame: &mut Frame, host: HostId, window: &mut Window, cx: &mut Context<Frame>) {
    let answer = frame.hosts().call(host, GccpList);
    frame.when_host_answers(
        REMOVE,
        host,
        answer,
        move |frame, state, window, cx| {
            let name = frame.hosts().host_label(host);
            if state.contacts.is_empty() {
                frame.echo(
                    &format!("{REMOVE}: no agent on {name} accepted a gccp address"),
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            }
            let choices: Rc<Vec<(GccpContact, String, Candidate)>> = Rc::new(
                state
                    .contacts
                    .into_iter()
                    .map(|contact| {
                        let agent = agent_label(frame, &contact.agent, cx);
                        let candidate = Candidate {
                            value: format!("{agent} {}", contact.address),
                            description: format!("{:?}", contact.name),
                        };
                        (contact, agent, candidate)
                    })
                    .collect(),
            );
            let offered: Vec<Candidate> = choices.iter().map(|(_, _, row)| row.clone()).collect();
            frame.open_prompt(
                format!("remove gccp address on {name}:"),
                Rc::new(move |_, needle, _| matching(&offered, needle)),
                Rc::new(move |frame, input, _, cx| {
                    let input = input.trim();
                    let Some((contact, agent, _)) =
                        choices.iter().find(|(_, _, row)| row.value == input)
                    else {
                        frame.echo(
                            &format!("{REMOVE}: no agent on {name} accepted {input:?}"),
                            StyleClass::SystemInfo,
                            cx,
                        );
                        return;
                    };
                    let said = format!("{name}: {agent} no longer accepts {}", contact.address);
                    let call = GccpRemove {
                        agent: contact.agent.clone(),
                        address: contact.address.clone(),
                    };
                    let answer = frame.hosts().call(host, call);
                    tell(frame, REMOVE, host, answer, said, cx);
                }),
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

/// Makes a call that changes the host, and echoes `said` once the host
/// has done it, or why it would not.
pub(crate) fn tell(
    frame: &mut Frame,
    command: &'static str,
    host: HostId,
    answer: impl Future<Output = anyhow::Result<()>> + 'static,
    said: String,
    cx: &mut Context<Frame>,
) {
    let name = frame.hosts().host_label(host);
    cx.spawn(async move |this, cx| {
        let answer = answer.await;
        let _ = this.update(cx, |frame, cx| match answer {
            Ok(()) => frame.echo(&said, StyleClass::SystemInfo, cx),
            Err(error) => frame.echo(
                &format!("{command}: {name}: {error:#}"),
                StyleClass::StatusError,
                cx,
            ),
        });
    })
    .detach();
}

/// The rows that hold every typed word, in their id or what they are.
pub(crate) fn matching(rows: &[Candidate], needle: &str) -> Vec<Candidate> {
    let needle = needle.trim().to_lowercase();
    rows.iter()
        .filter(|row| {
            row.value.to_lowercase().contains(&needle)
                || row.description.to_lowercase().contains(&needle)
        })
        .cloned()
        .collect()
}

/// What the window calls the agent the host names by its id, such as
/// `eng-b8os`; its id when no system holds it.
fn agent_label(frame: &Frame, encoded: &str, cx: &gpui::App) -> String {
    AgentId::from_encoded(encoded)
        .ok()
        .and_then(|agent| frame.agent_name(agent, cx))
        .unwrap_or_else(|| encoded.to_owned())
}
