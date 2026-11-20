//! The surface over one agent's transcript.

use std::rc::Rc;

use editor::Editor;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Window, div};
use rho_agent_types::AgentId;
use rho_agents_client::protocol::transcript::{RuntimeKind, RuntimeState};
use rho_agents_client::protocol::worksets::Cancel;
use rho_agents_client::snippets::{self, RUNTIMES, Runtime};
use rho_chat::ThreadId;
use rho_frame::{Addressed as _, Candidate, Row, Subject, Surface};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use super::AgentsSystem;
use crate::agent_view::AgentModel;

pub struct AgentSurface {
    system: Entity<AgentsSystem>,
    agent: AgentId,
    editor: Entity<Editor>,
    _model: Entity<AgentModel>,
}

impl AgentSurface {
    pub fn new(
        system: Entity<AgentsSystem>,
        agent: AgentId,
        model: Entity<AgentModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = model.update(cx, |model, cx| model.build_editor(window, cx));
        cx.observe(&system, |_, _, cx| cx.notify()).detach();
        Self {
            system,
            agent,
            editor,
            _model: model,
        }
    }

    fn runtime(&self, cx: &App) -> Option<RuntimeState> {
        self.system
            .read(cx)
            .registry()
            .agent_facts(self.agent)
            .runtime
            .map(|heard| heard.state)
    }

    /// The line over the transcript: who, where, and what it is doing.
    fn header(&self, cx: &App) -> String {
        let system = self.system.read(cx);
        let registry = system.registry();
        let mut words = vec![registry.agent_display_label(self.agent)];
        if let Some(host) = registry.host_of_agent(self.agent) {
            words.push(registry.host_name(host).to_owned());
        }
        if let Some(directory) = registry.working_directory(self.agent) {
            words.push(directory.to_string());
        }
        if let Some(state) = self.runtime(cx) {
            words.push(state.activity().word().to_owned());
        }
        words.join(" · ")
    }
}

impl Surface for AgentSurface {
    fn title(&self, cx: &App) -> String {
        self.system
            .read(cx)
            .registry()
            .agent_display_label(self.agent)
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    fn subject(&self, _cx: &App) -> Subject {
        Subject::Own
    }

    fn rows(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Vec<Row> {
        let agent = self.agent;
        let mut rows = steer_rows(self.system.clone(), agent);
        let origin = {
            let system = self.system.read(cx);
            let registry = system.registry();
            registry
                .mirrored(agent)
                .and_then(|mirrored| mirrored.identity.origin())
                .and_then(|message| {
                    let host = registry.host_of_agent(agent)?;
                    Some(ThreadId {
                        host: registry.host_endpoint(host)?,
                        message,
                    })
                })
        };
        if let Some(thread) = origin {
            rows.push(Row::run(
                "o",
                "open its thread",
                move |frame, window, cx| {
                    frame.open(thread.address(), window, cx);
                },
            ));
        }
        rows
    }
}

/// The rows that steer an agent, from its transcript or from a thread's
/// `with` line. Each carries the change to the agent's host, and says the
/// user has seen the stop the agent is in.
pub(crate) fn steer_rows(system: Entity<AgentsSystem>, agent: AgentId) -> Vec<Row> {
    let eval = |description: &'static str,
                key: &'static str,
                code: Rc<dyn Fn(Option<u32>) -> String>,
                done: fn(Option<u32>, &str) -> Option<String>| {
        let system = system.clone();
        Row::counted(key, description, move |_, count, _, cx| {
            let code = code(count);
            let label = system.read(cx).registry().agent_display_label(agent);
            let done = done(count, &label);
            system.update(cx, |system, cx| {
                system.eval(agent, description, code, done, cx);
                system.steered(agent, cx);
            });
        })
    };
    let mut rows = vec![
        eval(
            "rewind turns",
            "r",
            Rc::new(move |count| snippets::rewind(agent, count.unwrap_or(1))),
            |count, label| Some(rewound(label, count.unwrap_or(1))),
        ),
        eval(
            "compact context",
            "c",
            Rc::new(move |_| snippets::compact(agent)),
            |_, _| None,
        ),
        eval(
            "new cache key",
            "k",
            Rc::new(move |_| snippets::new_cache_key(agent)),
            |_, _| None,
        ),
    ];
    // Read when chosen, not here: the palette asks for rows while this
    // system is updating.
    let runtime_system = system.clone();
    rows.push(Row::run(
        "shift-r",
        "set runtime…",
        move |frame, window, cx| {
            let system = runtime_system.clone();
            // An agent keeps its runtime's kind; of that kind, the others.
            let spec = system.read(cx).registry().agent_spec(agent);
            let current = spec
                .as_ref()
                .map(|spec| format!("{}-{}", spec.model, spec.effort))
                .unwrap_or_default();
            let kind = spec.map(|spec| spec.runtime);
            let current_runtime = current.clone();
            frame.open_prompt(
                format!("runtime ({current}):"),
                Rc::new(move |_, needle, _| {
                    switchable(kind, &current_runtime)
                        .filter(|runtime| runtime.contains(needle))
                        .map(|runtime| Candidate {
                            value: runtime.to_owned(),
                            description: String::new(),
                        })
                        .collect()
                }),
                Rc::new(move |frame, input, _, cx| match Runtime::parse(&input) {
                    Ok(runtime) => system.update(cx, |system, cx| {
                        system.eval(
                            agent,
                            "set runtime",
                            snippets::set_runtime(agent, &runtime),
                            None,
                            cx,
                        );
                        system.steered(agent, cx);
                    }),
                    Err(error) => frame.echo(&error, StyleClass::SystemInfo, cx),
                }),
                window,
                cx,
            );
        },
    ));
    let cancel_system = system.clone();
    rows.push(Row::run(
        "shift-c",
        "cancel the request",
        move |frame, _, cx| {
            let system = cancel_system.read(cx);
            let Some(agents) = system.agents_for(agent) else {
                frame.echo(
                    "the agent's host is not attached",
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            };
            let reply = agents.call(Cancel { agent });
            let link = system.frame.clone();
            cx.spawn(async move |_, cx| {
                if let Err(error) = reply.await {
                    cx.update(|cx| {
                        link.echo(format!("{error:#}"), StyleClass::SystemImportant, cx)
                    });
                }
            })
            .detach();
            cancel_system.update(cx, |system, cx| system.steered(agent, cx));
        },
    ));
    rows
}

/// The runtimes an agent of `kind` running `current` can be set to: those of
/// its kind but the one it has.
fn switchable<'a>(
    kind: Option<RuntimeKind>,
    current: &'a str,
) -> impl Iterator<Item = &'static str> + 'a {
    RUNTIMES.into_iter().filter(move |runtime| {
        *runtime != current
            && Runtime::parse(runtime).is_ok_and(|runtime| Some(runtime.kind) == kind)
    })
}

/// What the echo says once `turns` of the turns of the agent called `label`
/// are taken back: it does nothing until the user writes.
fn rewound(label: &str, turns: u32) -> String {
    let turns = match turns {
        1 => "one turn".to_owned(),
        turns => format!("{turns} turns"),
    };
    format!("rewound {turns}: {label} · waiting for you")
}

impl Focusable for AgentSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for AgentSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .key_context("RhoAgent")
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_color(colors.text_muted)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(self.header(cx)),
            )
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_is_offered_only_the_other_runtimes_of_its_kind() {
        let claude: Vec<_> = switchable(Some(RuntimeKind::Claude), "opus-high").collect();
        assert_eq!(claude, ["fable-high"]);
        let rho: Vec<_> = switchable(Some(RuntimeKind::Rho), "sol-high").collect();
        assert_eq!(
            rho,
            ["sol-medium", "terra-medium", "astra-high", "hydrogen-high"]
        );
        assert!(switchable(None, "").next().is_none());
    }
}
