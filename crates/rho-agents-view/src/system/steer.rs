//! Steering an agent: each change the user picks, carried to its host.
//! Which agent is the command's to resolve ([`super::commands`]); here
//! the agent is known.

use std::rc::Rc;

use gpui::{Context, Entity, Window};
use rho_agent_types::AgentId;
use rho_agents_client::protocol::transcript::RuntimeKind;
use rho_agents_client::protocol::worksets::{Cancel, CancelResult, RewindAgent, StopAgent};
use rho_agents_client::snippets::{self, RUNTIMES, Runtime};
use rho_frame::{Candidate, Frame};
use rho_window::style::StyleClass;

use super::AgentsSystem;

const AWAY: &str = "the agent's host is not attached";

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
        frame.echo(AWAY, StyleClass::SystemInfo, cx);
        return;
    };
    let label = system.registry().agent_display_label(agent);
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
                format!("rewind turns: {error:#}"),
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
        let label = system.registry().agent_display_label(agent);
        system.eval(
            agent,
            "compact context",
            snippets::compact(agent),
            Some(format!("compacted: {label}")),
            cx,
        );
    });
}

/// Starts the agent's model cache afresh.
pub(super) fn new_cache_key(
    system: &Entity<AgentsSystem>,
    agent: AgentId,
    cx: &mut Context<Frame>,
) {
    system.update(cx, |system, cx| {
        let label = system.registry().agent_display_label(agent);
        system.eval(
            agent,
            "new cache key",
            snippets::new_cache_key(agent),
            Some(format!("new cache key: {label}")),
            cx,
        );
    });
}

/// Asks which runtime the agent changes to, offering the host's others
/// of its kind, then changes it.
pub(super) fn set_runtime(
    system: &Entity<AgentsSystem>,
    agent: AgentId,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let (current, choices) = {
        let system = system.read(cx);
        let registry = system.registry();
        let Some(spec) = registry.agent_spec(agent) else {
            frame.echo("the agent is unavailable", StyleClass::SystemInfo, cx);
            return;
        };
        let current = spec.runtime_label();
        let can_change_kind = registry
            .agent_facts(agent)
            .runtime
            .is_some_and(|heard| heard.state.may_change_runtime_kind);
        let choices = switchable(spec.runtime, &current, can_change_kind).collect::<Vec<_>>();
        (current, choices)
    };
    let system = system.clone();
    frame.open_prompt(
        format!("runtime ({current}):"),
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
        Rc::new(move |frame, input, _, cx| {
            if input.trim().is_empty() {
                frame.echo("choose a runtime", StyleClass::SystemInfo, cx);
                return;
            }
            match snippets::parse_runtime(&input) {
                Ok(runtime) => system.update(cx, |system, cx| {
                    system.eval(
                        agent,
                        "set runtime",
                        snippets::set_runtime(agent, &runtime),
                        Some(format!(
                            "runtime: {} · {} (was {current})",
                            system.registry().agent_display_label(agent),
                            runtime.label(),
                        )),
                        cx,
                    );
                }),
                Err(error) => frame.echo(&error, StyleClass::SystemInfo, cx),
            }
        }),
        window,
        cx,
    );
}

/// Ends the agent's work; its transcript stays.
pub(super) fn stop(system: &Entity<AgentsSystem>, agent: AgentId, cx: &mut Context<Frame>) {
    let system = system.read(cx);
    let frame = system.frame.clone();
    let Some(agents) = system.agents_for(agent) else {
        frame.echo(AWAY, StyleClass::SystemImportant, cx);
        return;
    };
    let reply = agents.call(StopAgent { agent });
    let label = system.registry().agent_display_label(agent);
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
        frame.echo(AWAY, StyleClass::SystemInfo, cx);
        return;
    };
    let reply = agents.call(Cancel { agent });
    let label = system.registry().agent_display_label(agent);
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
                format!("cancel the request: {error:#}"),
                StyleClass::SystemImportant,
                cx,
            ),
        });
    })
    .detach();
}

/// The other kind is offered only where no conversation would be lost.
/// The runtime already in use is not a change.
fn switchable(
    kind: RuntimeKind,
    current: &str,
    can_change_kind: bool,
) -> impl Iterator<Item = String> {
    let other = match kind {
        RuntimeKind::Claude => RuntimeKind::Rho,
        RuntimeKind::Rho => RuntimeKind::Claude,
    };
    // Keep the usual start choices visible without scrolling, then include
    // every other supported model and effort.
    let usual = RUNTIMES.into_iter().filter(move |choice| {
        Runtime::parse(choice).is_ok_and(|runtime| runtime.kind() == kind || can_change_kind)
    });
    let all = Runtime::choices(kind)
        .chain(
            can_change_kind
                .then_some(other)
                .into_iter()
                .flat_map(Runtime::choices),
        )
        .filter(|choice| !RUNTIMES.contains(&choice.as_str()));
    usual
        .map(str::to_owned)
        .chain(all)
        .filter(move |choice| choice != current)
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
    use std::collections::BTreeSet;

    use super::*;

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

    /// The catalogue's pairs of `kinds`, without `current`: what a prompt
    /// should offer, worked out apart from its order.
    fn expected(kinds: &[RuntimeKind], current: &str) -> BTreeSet<String> {
        kinds
            .iter()
            .flat_map(|kind| Runtime::choices(*kind))
            .filter(|choice| choice != current)
            .collect()
    }

    /// Each choice once, the usual ones of `kinds` first, in their order.
    fn assert_offered(choices: &[String], kinds: &[RuntimeKind], current: &str) {
        let offered: BTreeSet<String> = choices.iter().cloned().collect();
        assert_eq!(offered.len(), choices.len(), "no choice twice: {choices:?}");
        assert_eq!(offered, expected(kinds, current));
        let usual: Vec<&str> = RUNTIMES
            .into_iter()
            .filter(|choice| *choice != current)
            .filter(|choice| kinds.contains(&Runtime::parse(choice).unwrap().kind()))
            .collect();
        assert_eq!(&choices[..usual.len()], usual.as_slice());
    }

    #[test]
    fn runtime_prompt_lists_every_supported_effort_and_model_of_the_current_kind() {
        let choices: Vec<_> = switchable(RuntimeKind::Claude, "opus-high", false).collect();
        assert_offered(&choices, &[RuntimeKind::Claude], "opus-high");
        for choice in ["fable-max", "opus-medium", "sonnet-low", "haiku-xhigh"] {
            assert!(choices.iter().any(|offered| offered == choice), "{choice}");
        }
        assert!(
            !choices
                .iter()
                .any(|choice| choice == "opus-high" || choice == "sol-medium")
        );
    }

    #[test]
    fn a_stopped_agent_without_history_can_choose_another_kind() {
        let choices: Vec<_> = switchable(RuntimeKind::Claude, "opus-high", true).collect();
        assert_offered(
            &choices,
            &[RuntimeKind::Claude, RuntimeKind::Rho],
            "opus-high",
        );
        assert!(choices.iter().any(|choice| choice == "terra-xhigh"));
        assert!(choices.iter().all(|choice| Runtime::parse(choice).is_ok()));
    }
}
