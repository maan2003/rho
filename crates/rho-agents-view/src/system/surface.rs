//! The surface over one agent's transcript.

use std::rc::Rc;

use editor::Editor;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Window, div};
use rho_agent_types::AgentId;
use rho_agents_client::protocol::transcript::{InferenceState, RuntimeState};
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
            words.push(status_word(&state).to_owned());
        }
        words.join(" · ")
    }
}

/// What an agent's runtime is doing, in a word.
pub(crate) fn status_word(state: &RuntimeState) -> &'static str {
    match &state.inference {
        InferenceState::Failed { .. } => "failed",
        InferenceState::Retrying { .. } => "retrying",
        _ if state.awaiting_human => "waiting",
        _ if state.is_working() => "working",
        _ if state.archived => "archived",
        _ => "idle",
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
        let system = self.system.clone();
        let eval = |description: &'static str,
                    key: &'static str,
                    code: Rc<dyn Fn(Option<u32>) -> String>| {
            let system = system.clone();
            Row::counted(key, description, move |_, count, _, cx| {
                let code = code(count);
                system.update(cx, |system, cx| system.eval(agent, code, cx));
            })
        };
        let mut rows = vec![
            eval(
                "rewind turns",
                "r",
                Rc::new(move |count| snippets::rewind(agent, count.unwrap_or(1))),
            ),
            eval(
                "compact context",
                "c",
                Rc::new(move |_| snippets::compact(agent)),
            ),
            eval(
                "new cache key",
                "k",
                Rc::new(move |_| snippets::new_cache_key(agent)),
            ),
        ];
        let current = self
            .system
            .read(cx)
            .registry()
            .agent_spec(agent)
            .map(|spec| format!("{}-{}", spec.model, spec.effort))
            .unwrap_or_default();
        let runtime_system = system.clone();
        rows.push(Row::run("t", "set runtime…", move |frame, window, cx| {
            let system = runtime_system.clone();
            frame.open_prompt(
                format!("runtime ({current}):"),
                Rc::new(|_, needle, _| {
                    RUNTIMES
                        .iter()
                        .filter(|runtime| runtime.contains(needle))
                        .map(|runtime| Candidate {
                            value: (*runtime).to_owned(),
                            description: "runtime".to_owned(),
                        })
                        .collect()
                }),
                Rc::new(move |frame, input, _, cx| match Runtime::parse(&input) {
                    Ok(runtime) => system.update(cx, |system, cx| {
                        system.eval(agent, snippets::set_runtime(agent, &runtime), cx)
                    }),
                    Err(error) => frame.echo(&error, StyleClass::SystemInfo, cx),
                }),
                window,
                cx,
            );
        }));
        let cancel_system = system.clone();
        rows.push(Row::run("x", "cancel the request", move |frame, _, cx| {
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
        }));
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
