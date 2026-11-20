//! The surface over one agent's transcript.

use std::rc::Rc;

use editor::Editor;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, KeyBinding, Window, div};
use rho_agent_types::{Activity, AgentId, Message, MessageId};
use rho_agents_client::protocol::transcript::{RuntimeKind, RuntimeState};
use rho_agents_client::protocol::worksets::{Cancel, RewindAgent};
use rho_agents_client::snippets::{self, RUNTIMES, Runtime};
use rho_frame::{Addressed as _, Candidate, Row, Subject, Surface};
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use super::AgentsSystem;
use crate::agent_view::{AgentModel, AgentModelEvent};

gpui::actions!(rho_agent, [NextError, PreviousError]);

enum Inspect {
    Reading(Message),
    Ready(Message),
}

pub struct AgentSurface {
    system: Entity<AgentsSystem>,
    agent: AgentId,
    editor: Entity<Editor>,
    model: Entity<AgentModel>,
    inspect_mail: Option<Inspect>,
    default_landing: bool,
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
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(
            &model,
            window,
            |this, _, event: &AgentModelEvent, window, cx| {
                if let AgentModelEvent::RestorePoints(points) = event
                    && let Some((_, point)) =
                        points.iter().find(|(id, _)| *id == this.editor.entity_id())
                {
                    this.model.update(cx, |model, cx| {
                        model.go_to_store_point(&this.editor, *point, window, cx)
                    });
                }
                if matches!(event, AgentModelEvent::Loaded) {
                    let mut landed = false;
                    if matches!(&this.inspect_mail, Some(Inspect::Ready(_))) {
                        landed = this.finish_inspection(window, cx);
                    }
                    if !landed && this.inspect_mail.is_none() && this.default_landing {
                        this.land(window, cx);
                    }
                    this.default_landing = false;
                    this.model
                        .update(cx, |model, cx| model.place_pending_point(window, cx));
                }
                cx.notify();
            },
        )
        .detach();
        Self {
            system,
            agent,
            editor,
            model,
            inspect_mail: None,
            default_landing: false,
        }
    }

    fn title_with_folder(&self, compact: bool, cx: &App) -> String {
        let system = self.system.read(cx);
        let registry = system.registry();
        let label = if compact {
            registry.agent_id_label(self.agent)
        } else {
            registry.agent_display_label(self.agent)
        };
        let title = registry
            .host_of_agent(self.agent)
            .map_or(label.clone(), |host| {
                format!("{} › {label}", registry.host_name(host))
            });
        let title = registry
            .working_directory(self.agent)
            .map_or(title.clone(), |directory| {
                let folder = if compact {
                    directory.file_name().unwrap_or(directory.as_str())
                } else {
                    directory.as_str()
                };
                format!("{title} · {folder}")
            });
        registry
            .agent_spec(self.agent)
            .map_or(title.clone(), |spec| {
                format!("{title} · {}", spec.runtime_label())
            })
    }

    fn runtime(&self, cx: &App) -> Option<RuntimeState> {
        self.system
            .read(cx)
            .registry()
            .agent_facts(self.agent)
            .runtime
            .map(|heard| heard.state)
    }

    pub(super) fn inspect(&mut self, mail: Message, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .system
            .read(cx)
            .read_for_inspection(self.agent, mail.id)
        {
            self.inspect_mail = Some(Inspect::Reading(mail));
        } else {
            self.inspect_mail = Some(Inspect::Ready(mail));
            if self.model.read(cx).initial_load_ready() {
                self.finish_inspection(window, cx);
            }
        }
    }

    pub(super) fn read_inspection(
        &mut self,
        id: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&self.inspect_mail, Some(Inspect::Reading(mail)) if mail.id == id)
            && let Some(Inspect::Reading(mail)) = self.inspect_mail.take()
        {
            self.inspect_mail = Some(Inspect::Ready(mail));
            if self.model.read(cx).initial_load_ready() {
                self.finish_inspection(window, cx);
            }
        }
    }

    fn finish_inspection(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(Inspect::Ready(mail)) = self.inspect_mail.take() else {
            return false;
        };
        let Some(point) = self.model.read(cx).mail_point(&mail) else {
            self.system.read(cx).frame.clone().echo(
                "this transcript does not record where the mail was sent",
                StyleClass::SystemInfo,
                cx,
            );
            return false;
        };
        self.model.update(cx, |model, cx| {
            model.go_to_store_point(&self.editor, point, window, cx)
        });
        true
    }

    fn land(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.land_at(
            self.runtime(cx)
                .is_some_and(|state| in_trouble(state.activity())),
            window,
            cx,
        )
    }

    fn land_at(&self, trouble: bool, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let error = (trouble && model.current_errors() != 0)
            .then(|| model.errors().next_back())
            .flatten()
            .map(|(point, _, _)| point);
        if let Some(point) = error.or_else(|| model.newest_step()) {
            self.model.update(cx, |model, cx| {
                model.go_to_store_point(&self.editor, point, window, cx)
            });
        }
    }

    fn jump_error(
        &self,
        current: Option<usize>,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.read(cx);
        let point = if forward {
            model
                .errors()
                .map(|(point, _, _)| point)
                .find(|point| current.is_none_or(|cur| point.block > cur))
        } else {
            model
                .errors()
                .rev()
                .map(|(point, _, _)| point)
                .find(|point| current.is_none_or(|cur| point.block < cur))
        };
        if let Some(point) = point {
            self.model.update(cx, |model, cx| {
                model.go_to_store_point(&self.editor, point, window, cx)
            });
        } else {
            let words = if model.errors().next().is_none() {
                "no errors"
            } else if forward {
                "last error"
            } else {
                "first error"
            };
            self.system
                .read(cx)
                .frame
                .clone()
                .echo(words, StyleClass::SystemInfo, cx);
        }
    }

    fn move_error(&self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .model
            .read(cx)
            .store_point(&self.editor, cx)
            .map(|point| point.block);
        self.jump_error(current, forward, window, cx);
    }

    pub(super) fn bind_keys(cx: &mut App) {
        for context in rho_frame::normal_contexts("RhoAgent", " && vim_operator == none") {
            cx.bind_keys([
                KeyBinding::new("] e", NextError, Some(&context)),
                KeyBinding::new("[ e", PreviousError, Some(&context)),
            ]);
        }
    }
}

impl Surface for AgentSurface {
    fn opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.read(cx).initial_load_ready() {
            self.land(window, cx);
        } else {
            self.default_landing = true;
        }
    }

    fn title(&self, cx: &App) -> String {
        self.title_with_folder(false, cx)
    }

    fn phone_titles(&self, cx: &App) -> Vec<String> {
        let registry = self.system.read(cx).registry();
        let label = registry.agent_id_label(self.agent);
        let title = registry
            .host_of_agent(self.agent)
            .map(|host| format!("{} › {label}", registry.host_name(host)));
        let full = self.title_with_folder(true, cx);
        let runtime = registry
            .agent_spec(self.agent)
            .map(|spec| spec.runtime_label());
        let mut titles = Vec::new();
        if let Some(runtime) = runtime {
            titles.push(full);
            titles.extend(title.iter().map(|title| format!("{title} · {runtime}")));
            titles.push(format!("{label} · {runtime}"));
        } else {
            titles.push(full);
            titles.extend(title);
        }
        titles.push(label);
        if self.runtime(cx).is_some() {
            for title in &mut titles {
                title.push_str(" ·");
            }
        }
        titles
    }

    fn phone_tools(&self, _: bool, cx: &App) -> Vec<rho_frame::PhoneTool> {
        if self.model.read(cx).errors().next().is_none() {
            return Vec::new();
        }
        vec![
            rho_frame::PhoneTool::new("!", "error", |window, cx| {
                window.dispatch_action(Box::new(NextError), cx);
            })
            .in_desk_slot(),
        ]
    }

    fn status(&self, cx: &App) -> Option<String> {
        let activity = self.runtime(cx)?.activity().word();
        let errors = self.model.read(cx).current_errors();
        Some(match errors {
            0 => activity.to_owned(),
            1 => format!("{activity} · 1 error"),
            _ => format!("{activity} · {errors} errors"),
        })
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
        let origin = self.system.read(cx).agent_thread(agent, cx);
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

fn in_trouble(activity: Activity) -> bool {
    matches!(activity, Activity::Retrying | Activity::Failed)
}

/// The rows that steer an agent, from its transcript or from a thread's
/// `with` line. Each carries the change to the agent's host.
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
            });
        })
    };
    let rewind_system = system.clone();
    let mut rows = vec![
        Row::counted("r", "rewind turns", move |_, count, _, cx| {
            let turns = count.unwrap_or(1);
            let system = rewind_system.read(cx);
            let frame = system.frame.clone();
            let Some(agents) = system.agents_for(agent) else {
                frame.echo(
                    "the agent's host is not attached",
                    StyleClass::SystemInfo,
                    cx,
                );
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
        }),
        eval(
            "compact context",
            "c",
            Rc::new(move |_| snippets::compact(agent)),
            |_, label| Some(format!("compacted: {label}")),
        ),
        eval(
            "new cache key",
            "k",
            Rc::new(move |_| snippets::new_cache_key(agent)),
            |_, label| Some(format!("new cache key: {label}")),
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
                let choices =
                    switchable(spec.runtime, &current, can_change_kind).collect::<Vec<_>>();
                (current, choices)
            };
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
                    match Runtime::parse(&input) {
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
            use rho_agents_client::protocol::worksets::CancelResult;
            let reply = agents.call(Cancel { agent });
            let label = system.registry().agent_display_label(agent);
            let link = system.frame.clone();
            cx.spawn(async move |_, cx| {
                let result = reply.await;
                cx.update(|cx| match result {
                    Ok(CancelResult::WasAsking) => {
                        link.echo(format!("cancelled: {label}"), StyleClass::SystemInfo, cx)
                    }
                    Ok(CancelResult::NoRequest) => link.echo(
                        format!("not cancelled: {label} · it was not asking its model"),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                    Err(error) => link.echo(
                        format!("cancel the request: {error:#}"),
                        StyleClass::SystemImportant,
                        cx,
                    ),
                });
            })
            .detach();
        },
    ));
    rows
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
        Runtime::parse(choice).is_ok_and(|runtime| runtime.kind == kind || can_change_kind)
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

impl Focusable for AgentSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for AgentSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_text = self
            .editor
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        let colors = cx.theme().colors();
        let brief = self.system.read(cx).brief(self.agent, cx);
        div()
            .id("rho-agent-transcript")
            .key_context("RhoAgent")
            .on_action(
                cx.listener(|this, _: &NextError, window, cx| this.move_error(true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PreviousError, window, cx| {
                    this.move_error(false, window, cx)
                }),
            )
            .size_full()
            .flex()
            .flex_col()
            .children(brief.map(|brief| {
                div()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .font_family(editor_text.font_family)
                    .text_size(editor_text.font_size)
                    .line_height(editor_text.line_height)
                    .child(div().text_color(colors.text_muted).child("brief"))
                    .child(div().line_clamp(4).child(brief))
            }))
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn runtime_prompt_lists_every_supported_effort_and_model_of_the_current_kind() {
        let choices: Vec<_> = switchable(RuntimeKind::Claude, "opus-high", false).collect();
        assert_eq!(choices.len(), 19);
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
        assert_eq!(choices.len(), 35);
        assert!(choices.iter().any(|choice| choice == "terra-xhigh"));
        assert!(choices.iter().all(|choice| Runtime::parse(choice).is_ok()));
    }
}
