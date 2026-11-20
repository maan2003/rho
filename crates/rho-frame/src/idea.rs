//! One idea box, used on the desk and from any other place. Addressing
//! takes the minibuffer; writing leaves the prior screen and its cursor
//! in place. The project owns delivery and starts the work.

use std::rc::Rc;

use editor::{Editor, EditorEvent};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Window, div};
use rho_dealer::Address;
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::keys::{CancelIdea, SendIdea};
use crate::minibuffer::{Candidate, CandidateSource};
use crate::place::{self, Write};
use crate::system::AnySystem;
use crate::{Frame, FrameLink};

/// A project address as it is written, with the stable identity of the
/// project that owns delivery. Neither its displayed name nor its host's
/// alias is used to identify it once the user has chosen it.
#[derive(Clone, Debug)]
pub struct IdeaProject {
    pub address: Address,
    pub name: String,
}

pub type IdeaSent = Rc<dyn Fn(String, &mut App)>;

/// A request to send the user's words. The project calls `sent` only
/// when the mail has gone to its outbox (a first start may ask where).
#[derive(Clone)]
pub struct IdeaMail {
    pub project: Address,
    pub text: String,
    pub sent: IdeaSent,
}

pub(crate) struct IdeaBox {
    editor: Entity<Editor>,
    /// The project already chosen for the unsent idea.
    to: Option<Address>,
    /// The box is temporarily in view above the phone bar or in a strip
    /// beneath the usual screen. On the desk the empty box is always shown.
    return_focus: Option<FocusHandle>,
}

impl IdeaBox {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Frame>, db: &rho_db::RhoDb) -> Self {
        let saved = place::idea_draft(db);
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_use_modal_editing(false);
            if let Some((_, text)) = &saved {
                editor.set_text(text.clone(), window, cx);
            }
            editor
        });
        cx.subscribe(&editor, |frame, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                frame.save_idea(cx);
            }
        })
        .detach();
        Self {
            editor,
            to: saved.map(|(to, _)| to),
            return_focus: None,
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.return_focus.is_some()
    }
}

impl Frame {
    fn last_idea_project(&self, cx: &App) -> Option<IdeaProject> {
        self.systems
            .iter()
            .find_map(|system| system.last_idea_project(cx))
    }

    fn default_idea_project(&self, cx: &App) -> Option<IdeaProject> {
        self.project_for(self.current_address(), cx)
            .map(|(_, project)| project)
            .or_else(|| self.last_idea_project(cx))
    }

    fn project_for(&self, address: &Address, cx: &App) -> Option<(AnySystem, IdeaProject)> {
        self.systems.iter().find_map(|system| {
            let project = system.idea_project(address, cx)?;
            Some((system.clone(), project))
        })
    }

    fn draft_project(&self, cx: &App) -> Option<IdeaProject> {
        match &self.idea.to {
            // A missing project is not a reason to silently send an
            // addressed draft somewhere else.
            Some(address) => self.project_for(address, cx).map(|(_, project)| project),
            None => self.last_idea_project(cx),
        }
    }

    fn save_idea(&self, cx: &App) {
        let text = self.idea.editor.read(cx).text(cx);
        let draft = self
            .idea
            .to
            .clone()
            .filter(|_| !text.is_empty())
            .map(|address| (address, text));
        self.place.write(Write::IdeaDraft(draft));
    }

    pub(crate) fn prompt_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() || self.menu.is_some() {
            return;
        }
        let default = if self.idea.active() {
            self.draft_project(cx)
        } else {
            self.default_idea_project(cx)
        };
        let label = match &default {
            Some(project) => format!("to ({}):", project.name),
            None => "to:".into(),
        };
        let complete: CandidateSource = Rc::new(|frame, needle, cx| {
            let needle = needle.trim().to_lowercase();
            let mut projects: Vec<_> = frame
                .systems
                .iter()
                .flat_map(|system| system.idea_projects(cx))
                .filter(|project| project.name.to_lowercase().contains(&needle))
                .map(|project| Candidate {
                    value: project.name,
                    description: "project".into(),
                })
                .collect();
            projects.sort_by(|a, b| a.value.cmp(&b.value));
            projects
        });
        self.open_prompt(
            label,
            complete,
            Rc::new(move |frame, input, window, cx| {
                let input = input.trim();
                let project = if input.is_empty() {
                    default.as_ref().and_then(|project| {
                        frame
                            .project_for(&project.address, cx)
                            .map(|(_, project)| project)
                    })
                } else {
                    frame
                        .systems
                        .iter()
                        .flat_map(|system| system.idea_projects(cx))
                        .find(|project| project.name == input)
                };
                if let Some(project) = project {
                    frame.edit_idea_to(project, window, cx);
                } else {
                    let why = if input.is_empty() {
                        "no project chosen".into()
                    } else {
                        format!("no project named {input}")
                    };
                    frame.echo(&why, StyleClass::SystemInfo, cx);
                }
            }),
            window,
            cx,
        );
        // Completion replaces the whole address, including project names
        // with spaces. Enter on untouched input uses the label's default.
        if let Some(prompt) = self.minibuffer_mut() {
            prompt.complete_project_address();
        }
    }

    pub(crate) fn edit_desk_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.draft_project(cx) {
            Some(project) => self.edit_idea_to(project, window, cx),
            None => self.prompt_idea(window, cx),
        }
    }

    fn edit_idea_to(&mut self, project: IdeaProject, window: &mut Window, cx: &mut Context<Self>) {
        if !self.idea.active() {
            self.idea.return_focus =
                Some(window.focused(cx).unwrap_or_else(|| self.focus_handle(cx)));
        }
        self.idea.to = Some(project.address);
        self.save_idea(cx);
        window.focus(&self.idea.editor.focus_handle(cx), cx);
        cx.notify();
    }

    pub(crate) fn cancel_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(focus) = self.idea.return_focus.take() {
            window.focus(&focus, cx);
            cx.notify();
        }
    }

    pub(crate) fn send_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.idea.active() {
            return;
        }
        let text = self.idea.editor.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        let Some((system, project)) = self
            .idea
            .to
            .as_ref()
            .and_then(|address| self.project_for(address, cx))
        else {
            self.echo(
                "idea: choose an available project",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        // Returning to the screen now also gives a first-start question a
        // place to return to. The draft itself stays until the project posts.
        self.cancel_idea(window, cx);
        let sent_text = text.clone();
        let sent_to = project.address.clone();
        let frame = FrameLink::new(
            window
                .window_handle()
                .downcast::<Frame>()
                .expect("the frame owns the window"),
        );
        system.send_idea(
            IdeaMail {
                project: project.address,
                text,
                sent: Rc::new(move |name, cx| {
                    let sent_to = sent_to.clone();
                    let sent_text = sent_text.clone();
                    frame.with(cx, move |frame, window, cx| {
                        if frame.idea.to.as_ref() == Some(&sent_to)
                            && frame.idea.editor.read(cx).text(cx) == sent_text
                        {
                            frame.idea.to = None;
                            frame
                                .idea
                                .editor
                                .update(cx, |editor, cx| editor.clear(window, cx));
                            frame.save_idea(cx);
                        }
                        frame.echo(&format!("sent to {name}"), StyleClass::SystemInfo, cx);
                    });
                }),
            },
            cx,
        );
    }

    pub(crate) fn idea_editor(&self) -> Option<Entity<Editor>> {
        self.idea.active().then(|| self.idea.editor.clone())
    }

    pub(crate) fn render_idea(
        &self,
        text_style: &gpui::TextStyle,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let active = self.idea.active();
        if !self.desk_in_view() && !active {
            return None;
        }
        let label = match self.draft_project(cx) {
            Some(project) => format!("idea to {} · ctrl-enter sends", project.name),
            None => "idea · choose a project".into(),
        };
        let colors = cx.theme().colors();
        Some(
            div()
                .id("rho-idea-box")
                .key_context("RhoIdea")
                .flex_none()
                .w_full()
                .border_t_1()
                .border_color(colors.border_variant)
                .px_2()
                .py_1()
                .on_action(cx.listener(|this, _: &SendIdea, window, cx| this.send_idea(window, cx)))
                .on_action(
                    cx.listener(|this, _: &CancelIdea, window, cx| this.cancel_idea(window, cx)),
                )
                .child(
                    div()
                        .id("rho-idea-address")
                        .cursor_pointer()
                        .font_family(text_style.font_family.clone())
                        .text_size(text_style.font_size)
                        .line_height(text_style.line_height)
                        .text_color(colors.text_muted)
                        .child(label)
                        .on_click(cx.listener(move |frame, _, window, cx| {
                            if active {
                                frame.prompt_idea(window, cx);
                            } else {
                                frame.edit_desk_idea(window, cx);
                            }
                            cx.stop_propagation();
                        })),
                )
                .children(active.then(|| self.idea.editor.clone())),
        )
    }
}
