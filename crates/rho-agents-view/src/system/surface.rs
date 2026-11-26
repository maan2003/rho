//! The surface over a transcript: a workset's work, or one agent's of it
//! ([`Scope`]).

use editor::Editor;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, KeyBinding, Window, div};
use rho_agent_types::{Activity, AgentId, Message, MessageId};
use rho_frame::{CardHere, Subject, Surface};
use rho_window::style::StyleClass;

use super::{AgentsSystem, Scope};
use crate::agent_view::{AgentModel, AgentModelEvent};

gpui::actions!(rho_agent, [NextError, PreviousError]);

enum Inspect {
    /// The agent's log is read until it holds the step that wrote the
    /// message.
    Reading(Message),
    Ready(Message),
}

pub struct TranscriptSurface {
    system: Entity<AgentsSystem>,
    scope: Scope,
    editor: Entity<Editor>,
    model: Entity<AgentModel>,
    inspect_mail: Option<Inspect>,
    default_landing: bool,
}

impl TranscriptSurface {
    pub fn new(
        system: Entity<AgentsSystem>,
        scope: Scope,
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
            scope,
            editor,
            model,
            inspect_mail: None,
            default_landing: false,
        }
    }

    /// The work, its host, and the folder and runtime of the agent it
    /// shows: `tide · Flaky clock test · lab · ~/rho · opus-high`. Narrowed
    /// to one of several agents, its code follows the work.
    fn title_with_folder(&self, compact: bool, cx: &App) -> String {
        let system = self.system.read(cx);
        let registry = system.registry();
        let (Some((shown, _)), Some((label, host))) = (
            system.scope_agents(self.scope),
            system.scope_label(self.scope, cx),
        ) else {
            return String::new();
        };
        let folder = registry
            .working_directory(shown)
            .map(|directory| match compact {
                true => directory
                    .file_name()
                    .unwrap_or(directory.as_str())
                    .to_owned(),
                false => directory.to_string(),
            });
        let runtime = registry.agent_spec(shown).map(|spec| spec.runtime_label());
        super::work_name::transcript_title(&label, &host, folder.as_deref(), runtime.as_deref())
    }

    /// What the screen's agents are doing, the most pressing first: one
    /// failing outweighs the rest working.
    fn activity(&self, cx: &App) -> Option<Activity> {
        let system = self.system.read(cx);
        let (_, agents) = system.scope_agents(self.scope)?;
        agents
            .into_iter()
            .filter_map(|agent| system.registry().agent_facts(agent).runtime)
            .map(|heard| heard.state.activity())
            .min_by_key(|activity| pressing(*activity))
    }

    /// Opens on the step that wrote `mail`, which `agent` wrote, once the
    /// agent's log holds it.
    pub(super) fn inspect(
        &mut self,
        agent: AgentId,
        mail: Message,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.system.read(cx).read_for_inspection(agent, mail.id) {
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
            self.activity(cx)
                .is_some_and(|activity| pressing(activity) < pressing(Activity::Working)),
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

impl Surface for TranscriptSurface {
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
        let system = self.system.read(cx);
        let (Some((shown, _)), Some((label, _))) = (
            system.scope_agents(self.scope),
            system.scope_label(self.scope, cx),
        ) else {
            return Vec::new();
        };
        let mut titles = vec![self.title_with_folder(true, cx)];
        if let Some(spec) = system.registry().agent_spec(shown) {
            titles.push(format!("{label} · {}", spec.runtime_label()));
        }
        titles.push(label);
        if self.activity(cx).is_some() {
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
        let activity = self.activity(cx)?.word();
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

    fn subject(&self, _: Option<rho_frame::surface::Seen>, _cx: &App) -> Subject {
        Subject::Own
    }

    fn card(&self, _cx: &App) -> CardHere {
        CardHere::Inspected
    }
}

/// How much an agent doing this wants looking at: lower is more. Work in
/// motion outweighs waiting: an agent between turns waits too, and an ask
/// reaches the user as a card anyway.
fn pressing(activity: Activity) -> u8 {
    match activity {
        Activity::Failed | Activity::Lost => 0,
        Activity::Retrying => 1,
        Activity::Working => 2,
        Activity::Waiting => 3,
        Activity::Archived => 4,
    }
}

impl Focusable for TranscriptSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for TranscriptSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(self.editor.clone())
    }
}
