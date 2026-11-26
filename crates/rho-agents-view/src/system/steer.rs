//! Steering an agent: each change the user picks, carried to its host.
//! Which agent is the command's to resolve ([`super::commands`]); here
//! the agent is known.

use std::rc::Rc;

use gpui::{Context, Entity, Window};
use rho_agent_hosts::HostStatus;
use rho_agent_types::{AgentId, RuntimeCatalogue};
use rho_agents_client::protocol::transcript::RuntimeKind;
use rho_agents_client::protocol::worksets::{Cancel, CancelResult, RewindAgent, StopAgent};
use rho_agents_client::snippets;
use rho_frame::{Candidate, Frame};
use rho_window::style::StyleClass;

use super::AgentsSystem;

/// The agent's host was detached, or never attached.
pub(super) const NOT_ATTACHED: &str = "the agent's host is not attached";
/// The agent's host is attached but does not answer now.
pub(super) const DISCONNECTED: &str = "the agent's host is disconnected";

/// What the echo line says when an agent's host cannot take a command now,
/// given how the frame last heard the host: `None` when it is not attached.
/// Only a host that is online takes one; a host that dials, reconnects or
/// does not trust this device yet is away for now.
pub(super) fn unreachable(status: Option<&HostStatus>) -> Option<&'static str> {
    match status {
        None => Some(NOT_ATTACHED),
        Some(HostStatus::Online) => None,
        Some(
            HostStatus::Connecting
            | HostStatus::Recovering(_)
            | HostStatus::Disconnected(_)
            | HostStatus::Unenrolled(_),
        ) => Some(DISCONNECTED),
    }
}

/// Takes back the agent's last `turns` turns; it waits for the user.
pub(super) fn rewind(
    system: &Entity<AgentsSystem>,
    agent: AgentId,
    turns: u32,
    cx: &mut Context<Frame>,
) {
    let system = system.read(cx);
    let frame = system.frame.clone();
    let Some(agents) = system.agents_for(agent) else {
        frame.echo(NOT_ATTACHED, StyleClass::SystemInfo, cx);
        return;
    };
    let label = system.label(agent, cx).0;
    let reply = agents.call(RewindAgent { agent, turns });
    cx.spawn(async move |_, cx| {
        let result = reply.await;
        cx.update(|cx| match result {
            Ok(undone) => frame.echo(
                rewound(&label, turns, &undone.taken_back),
                StyleClass::SystemInfo,
                cx,
            ),
            Err(error) => frame.echo(
                format!("rewind agent: {error:#}"),
                StyleClass::SystemImportant,
                cx,
            ),
        });
    })
    .detach();
}

/// Shortens what the agent holds in mind.
pub(super) fn compact(system: &Entity<AgentsSystem>, agent: AgentId, cx: &mut Context<Frame>) {
    system.update(cx, |system, cx| {
        let label = system.label(agent, cx).0;
        system.eval(
            agent,
            "compact agent",
            snippets::compact(agent),
            Some(format!("compacted: {label}")),
            cx,
        );
    });
}

/// Starts the agent's model cache afresh.
pub(super) fn reset_cache(system: &Entity<AgentsSystem>, agent: AgentId, cx: &mut Context<Frame>) {
    system.update(cx, |system, cx| {
        let label = system.label(agent, cx).0;
        system.eval(
            agent,
            "reset agent cache",
            snippets::new_cache_key(agent),
            Some(format!("reset cache: {label}")),
            cx,
        );
    });
}

/// Changes the agent's runtime to the one typed after the command's name,
/// or asks which, offering what its host's catalogue has of the agent's
/// kind.
pub(super) fn set_runtime(
    system: &Entity<AgentsSystem>,
    agent: AgentId,
    typed: Option<String>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let (current, catalogue, choices) = {
        let system = system.read(cx);
        let registry = system.registry();
        let Some(spec) = registry.agent_spec(agent) else {
            frame.echo("the agent is unavailable", StyleClass::SystemInfo, cx);
            return;
        };
        let Some(catalogue) = registry
            .host_of_agent(agent)
            .and_then(|host| registry.host_endpoint(host))
            .and_then(|endpoint| system.mail.read(cx).runtime_catalogue(endpoint))
        else {
            frame.echo(
                "the agent's host has not said its runtimes",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let current = spec.runtime_label();
        let can_change_kind = registry
            .agent_facts(agent)
            .runtime
            .is_some_and(|heard| heard.state.may_change_runtime_kind);
        let choices = switchable(&catalogue, spec.runtime, &current, can_change_kind);
        (current, catalogue, choices)
    };
    let prompt = format!("runtime ({current}):");
    let system = system.clone();
    let change = move |frame: &mut Frame, input: &str, cx: &mut Context<Frame>| {
        let input = input.trim();
        if input.is_empty() {
            frame.echo("choose a runtime", StyleClass::SystemInfo, cx);
            return;
        }
        match catalogue.label(input) {
            Ok(runtime) => system.update(cx, |system, cx| {
                system.eval(
                    agent,
                    "set agent runtime",
                    snippets::set_runtime(agent, &runtime),
                    Some(format!(
                        "runtime: {} · {} (was {current})",
                        system.label(agent, cx).0,
                        runtime.label(),
                    )),
                    cx,
                );
            }),
            Err(error) => frame.echo(
                &format!("set agent runtime: {error}"),
                StyleClass::SystemInfo,
                cx,
            ),
        }
    };
    if let Some(typed) = typed {
        return change(frame, &typed, cx);
    }
    frame.open_prompt(
        prompt,
        Rc::new(move |_, needle, _| {
            let needle = needle.to_ascii_lowercase();
            choices
                .iter()
                .filter(|runtime| runtime.contains(&needle))
                .map(|runtime| Candidate {
                    value: runtime.clone(),
                    description: String::new(),
                })
                .collect()
        }),
        Rc::new(move |frame, input, _, cx| change(frame, &input, cx)),
        window,
        cx,
    );
}

/// Ends the agent's work; its transcript stays.
pub(super) fn stop(system: &Entity<AgentsSystem>, agent: AgentId, cx: &mut Context<Frame>) {
    let system = system.read(cx);
    let frame = system.frame.clone();
    let Some(agents) = system.agents_for(agent) else {
        frame.echo(NOT_ATTACHED, StyleClass::SystemImportant, cx);
        return;
    };
    let reply = agents.call(StopAgent { agent });
    let label = system.label(agent, cx).0;
    cx.spawn(async move |_, cx| {
        let reply = reply.await;
        cx.update(|cx| match reply {
            Ok(()) => frame.echo(
                format!("stopped by you: {label}"),
                StyleClass::SystemInfo,
                cx,
            ),
            Err(error) => frame.echo(
                format!("did not stop {label}: {error:#}"),
                StyleClass::SystemImportant,
                cx,
            ),
        });
    })
    .detach();
}

/// Stops what the agent is asking its model now.
pub(super) fn cancel(system: &Entity<AgentsSystem>, agent: AgentId, cx: &mut Context<Frame>) {
    let system = system.read(cx);
    let frame = system.frame.clone();
    let Some(agents) = system.agents_for(agent) else {
        frame.echo(NOT_ATTACHED, StyleClass::SystemInfo, cx);
        return;
    };
    let reply = agents.call(Cancel { agent });
    let label = system.label(agent, cx).0;
    cx.spawn(async move |_, cx| {
        let result = reply.await;
        cx.update(|cx| match result {
            Ok(CancelResult::WasAsking) => {
                frame.echo(format!("cancelled: {label}"), StyleClass::SystemInfo, cx)
            }
            Ok(CancelResult::NoRequest) => frame.echo(
                format!("not cancelled: {label} · it was not asking its model"),
                StyleClass::SystemInfo,
                cx,
            ),
            Err(error) => frame.echo(
                format!("cancel agent request: {error:#}"),
                StyleClass::SystemImportant,
                cx,
            ),
        });
    })
    .detach();
}

/// The other kind is offered only where no conversation would be lost.
/// The runtime already in use is not a change. The host's order puts its
/// usual runtimes first.
fn switchable(
    catalogue: &RuntimeCatalogue,
    kind: RuntimeKind,
    current: &str,
    can_change_kind: bool,
) -> Vec<String> {
    catalogue
        .choices
        .iter()
        .filter(|choice| choice.kind == kind || can_change_kind)
        .map(|choice| choice.label.as_str())
        .filter(|label| *label != current)
        .map(str::to_owned)
        .collect()
}

/// What the echo says once `turns` of the turns of the agent called `label`
/// are taken back: it does nothing until the user writes.
fn rewound(label: &str, turns: u32, taken_back: &[String]) -> String {
    let turns = match turns {
        1 => "one turn".to_owned(),
        turns => format!("{turns} turns"),
    };
    let mut echo = format!("rewound {turns}: {label}");
    if let Some(words) = taken_back.first() {
        echo.push_str(&format!(
            " · took back \"{}\"",
            crate::render::preview(words)
        ));
        if taken_back.len() > 1 {
            echo.push_str(&format!(" and {} more", taken_back.len() - 1));
        }
    }
    echo.push_str(" · waiting for you");
    echo
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A host that is attached but away is told apart from one that is not
    /// attached: the first comes back by itself, the second needs the user.
    #[test]
    fn an_away_host_is_disconnected_and_a_missing_one_is_not_attached() {
        assert_eq!(unreachable(None), Some(NOT_ATTACHED));
        assert_eq!(unreachable(Some(&HostStatus::Online)), None);
        for away in [
            HostStatus::Connecting,
            HostStatus::Recovering(Duration::from_secs(12)),
            HostStatus::Disconnected("timed out".into()),
            HostStatus::Unenrolled("rho enroll …".into()),
        ] {
            assert_eq!(unreachable(Some(&away)), Some(DISCONNECTED), "{away:?}");
        }
        assert_ne!(NOT_ATTACHED, DISCONNECTED);
    }

    #[test]
    fn rewind_echo_uses_the_host_receipt_not_the_transcripts_current_text() {
        let earlier = "Could you clarify the timer?";
        assert_eq!(
            rewound("eng-ka35", 1, &[earlier.into()]),
            "rewound one turn: eng-ka35 · took back \"Could you clarify the timer?\" · waiting for you"
        );
        assert_eq!(
            rewound("eng-ka35", 2, &[]),
            "rewound 2 turns: eng-ka35 · waiting for you"
        );
    }

    fn catalogue() -> RuntimeCatalogue {
        let choice = |label: &str, kind, usual| rho_agent_types::RuntimeChoice {
            label: rho_agent_types::RuntimeLabel::parse(label).unwrap(),
            kind,
            usual,
        };
        // Not this build's list: the prompt offers what the host said.
        RuntimeCatalogue {
            choices: vec![
                choice("sol-high", RuntimeKind::Rho, true),
                choice("opus-high", RuntimeKind::Claude, true),
                choice("sol-low", RuntimeKind::Rho, false),
                choice("sonnet-low", RuntimeKind::Claude, false),
            ],
        }
    }

    #[test]
    fn the_runtime_prompt_offers_the_hosts_runtimes_of_the_agents_kind() {
        assert_eq!(
            switchable(&catalogue(), RuntimeKind::Claude, "opus-high", false),
            ["sonnet-low"]
        );
        assert_eq!(
            switchable(&catalogue(), RuntimeKind::Rho, "sol-low", false),
            ["sol-high"]
        );
    }

    #[test]
    fn a_stopped_agent_without_history_can_choose_another_kind() {
        assert_eq!(
            switchable(&catalogue(), RuntimeKind::Claude, "opus-high", true),
            ["sol-high", "sol-low", "sonnet-low"]
        );
    }
}
