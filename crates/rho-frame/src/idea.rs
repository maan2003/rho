//! The full-window idea draft. Its address field completes against existing
//! projects; creating one is a separate, explicit choice.

use std::cell::RefCell;
use std::rc::Rc;

use editor::{Editor, EditorMode};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Window, div};
use language::{Buffer, BufferEvent};
use multi_buffer::MultiBuffer;
use rho_agent_types::EndpointId;
use rho_dealer::Address;
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

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

/// A validated project name on the host that will own it.
#[derive(Clone)]
pub struct NewIdeaProject {
    pub host: EndpointId,
    name: String,
}

impl NewIdeaProject {
    pub fn name(&self) -> &str {
        &self.name
    }
}

pub(crate) const NEW_PROJECT: &str = "new project";

pub type ProjectCreated = Rc<dyn Fn(IdeaProject, &mut App)>;

#[derive(Clone)]
enum ProjectChoice {
    Existing(IdeaProject),
    New(AnySystem, NewIdeaProject),
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
    pub(crate) text: Entity<Buffer>,
    project: Option<Address>,
    recipient: String,
    overlay: Option<Overlay>,
    _edits: gpui::Subscription,
}

struct Overlay {
    editor: Entity<Editor>,
    address: Entity<Editor>,
    return_focus: FocusHandle,
    _edits: gpui::Subscription,
}

impl IdeaBox {
    pub(crate) fn new(cx: &mut Context<Frame>, db: &rho_db::RhoDb) -> Self {
        let (project, recipient, words) = place::idea_draft(db)
            .map(|saved| {
                (
                    saved.address.and_then(|a| Address::parse(&a)),
                    saved.recipient,
                    saved.text,
                )
            })
            .unwrap_or_default();
        let text = cx.new(|cx| Buffer::local(words, cx));
        Self {
            _edits: Self::subscribe(&text, cx),
            text,
            project,
            recipient,
            overlay: None,
        }
    }

    fn subscribe(text: &Entity<Buffer>, cx: &mut Context<Frame>) -> gpui::Subscription {
        cx.subscribe(text, |frame, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                frame.save_idea(cx);
            }
        })
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

    pub(crate) fn project_for(
        &self,
        address: &Address,
        cx: &App,
    ) -> Option<(AnySystem, IdeaProject)> {
        self.systems.iter().find_map(|system| {
            let project = system.idea_project(address, cx)?;
            Some((system.clone(), project))
        })
    }

    pub(crate) fn idea_here(&self, _cx: &App) -> bool {
        self.idea.overlay.is_some()
    }

    fn resolved_idea_project(&self, cx: &App) -> Option<(AnySystem, IdeaProject)> {
        let typed = self.idea.recipient.trim();
        if let Some((system, project)) = self
            .idea
            .project
            .as_ref()
            .and_then(|address| self.project_for(address, cx))
            && project.name.eq_ignore_ascii_case(typed)
        {
            return Some((system, project));
        }
        for system in &self.systems {
            let Some(project) = system
                .idea_projects(cx)
                .into_iter()
                .find(|project| project.name.eq_ignore_ascii_case(typed))
            else {
                continue;
            };
            return Some((system.clone(), project));
        }
        None
    }

    fn save_idea(&self, cx: &App) {
        let text = self.idea.text.read(cx).text();
        let draft =
            (!text.is_empty() || !self.idea.recipient.is_empty()).then(|| place::IdeaDraft {
                address: self.idea.project.as_ref().map(ToString::to_string),
                recipient: self.idea.recipient.clone(),
                text,
            });
        self.place.write(Write::IdeaDraft(draft));
    }

    pub(crate) fn prompt_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() || self.menu.is_some() {
            return;
        }
        if self.idea.overlay.is_some() {
            self.choose_idea_project(window, cx);
            return;
        }
        // A saved addressee belongs to the draft; a default may only fill a
        // draft that has not been written yet.
        if self.idea.recipient.is_empty() {
            let project = match &self.idea.project {
                Some(address) => self.project_for(address, cx).map(|(_, project)| project),
                None if self.idea.text.read(cx).is_empty() => self.default_idea_project(cx),
                None => None,
            };
            if let Some(project) = project {
                self.idea.recipient = project.name;
                self.idea.project = Some(project.address);
            }
        }
        let return_focus = window
            .focused(cx)
            .unwrap_or_else(|| self.current_surface().focus_handle(cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(self.idea.text.clone(), cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: true,
                    sizing_behavior: editor::SizingBehavior::default(),
                },
                buffer,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor
        });
        let address = cx.new(|cx| {
            let mut address = Editor::single_line(window, cx);
            rho_window::editor_config::configure(&mut address, window, cx);
            address.set_use_modal_editing(false);
            address.set_text(self.idea.recipient.as_str(), window, cx);
            address
        });
        let address_edits = cx.subscribe(
            &address,
            |frame, address, event: &editor::EditorEvent, cx| {
                if matches!(event, editor::EditorEvent::Edited { .. }) {
                    frame.idea.recipient = address.read(cx).text(cx);
                    frame.save_idea(cx);
                }
            },
        );
        self.idea.overlay = Some(Overlay {
            editor: editor.clone(),
            address,
            return_focus,
            _edits: address_edits,
        });
        window.focus(&editor.focus_handle(cx), cx);
        window.on_next_frame(move |window, cx| {
            if editor.focus_handle(cx).is_focused(window) {
                if editor.read(cx).is_empty(cx) {
                    vim::enter_insert_mode(&editor, window, cx);
                }
            }
        });
        cx.notify();
    }

    pub(crate) fn focus_idea_address(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(overlay) = &self.idea.overlay {
            let handle = if overlay.address.focus_handle(cx).is_focused(window) {
                overlay.editor.focus_handle(cx)
            } else {
                overlay.address.focus_handle(cx)
            };
            window.focus(&handle, cx);
        }
    }

    fn choose_idea_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let default = self.resolved_idea_project(cx).map(|(_, project)| project);
        let prefill = default.is_none().then(|| self.idea.recipient.clone());
        let label = "to:";
        let choices = Rc::new(RefCell::new(Vec::new()));
        let shown = choices.clone();
        let contextual = default.clone();
        let complete: CandidateSource = Rc::new(move |frame, needle, cx| {
            let rows = frame.project_choices(needle, contextual.as_ref(), cx);
            let candidates = rows.iter().map(|(row, _)| row.clone()).collect();
            *shown.borrow_mut() = rows;
            candidates
        });
        self.open_prompt(
            label,
            complete,
            Rc::new(move |frame, input, window, cx| {
                let choice = choices
                    .borrow()
                    .iter()
                    .find(|(row, _)| row.value == input)
                    .map(|(_, choice)| choice.clone());

                match choice {
                    Some(ProjectChoice::Existing(project)) => {
                        frame.edit_idea_to(project, window, cx)
                    }
                    Some(ProjectChoice::New(system, project)) => {
                        let link = FrameLink::new(
                            window
                                .window_handle()
                                .downcast::<Frame>()
                                .expect("the frame owns the window"),
                        );
                        let before = frame.idea.recipient.clone();
                        let place = frame.current_address().clone();
                        system.create_idea_project(
                            project,
                            Rc::new(move |project, cx| {
                                let before = before.clone();
                                let place = place.clone();
                                link.with(cx, move |frame, window, cx| {
                                    if frame.idea.overlay.is_some()
                                        && frame.idea.recipient == before
                                        && frame.current_address() == &place
                                        && frame.minibuffer.is_none()
                                        && frame.menu.is_none()
                                    {
                                        frame.edit_idea_to(project, window, cx);
                                    }
                                });
                            }),
                            cx,
                        );
                    }
                    None => frame.echo("no project chosen", StyleClass::SystemInfo, cx),
                }
            }),
            window,
            cx,
        );
        // Tapping a project chooses it, just like Enter on its row.
        if let Some(prompt) = self.minibuffer_mut() {
            prompt.complete_project_address();
            if let Some(prefill) = prefill {
                prompt.set_input(prefill, window, cx);
            }
        }
    }

    fn project_choices(
        &self,
        input: &str,
        default: Option<&IdeaProject>,
        cx: &App,
    ) -> Vec<(Candidate, ProjectChoice)> {
        let input = rho_agent_types::one_line(input);
        let needle = input.to_lowercase();
        let mut rows: Vec<_> = self
            .systems
            .iter()
            .flat_map(|system| system.idea_projects(cx))
            .filter(|project| {
                default.is_none_or(|default| !project.name.eq_ignore_ascii_case(&default.name))
            })
            .chain(default.cloned())
            .filter(|project| project.name.to_lowercase().contains(&needle))
            .map(|project| {
                (
                    Candidate {
                        value: project.name.clone(),
                        description: "project".into(),
                    },
                    ProjectChoice::Existing(project),
                )
            })
            .collect();
        rows.sort_by(|(a, _), (b, _)| {
            let key = |row: &Candidate| {
                (
                    !row.value.eq_ignore_ascii_case(&input),
                    default.is_none_or(|project| project.name != row.value),
                )
            };
            (key(a), &a.value).cmp(&(key(b), &b.value))
        });
        if input.is_empty() || !rows.is_empty() {
            return rows;
        }
        let (name, requested_host) = input
            .rsplit_once('@')
            .map_or((input.as_str(), None), |(name, host)| {
                (name.trim_end(), Some(host))
            });
        if name.is_empty() || name.contains('@') || name.contains('|') {
            return rows;
        }
        let host_hint = requested_host.or_else(|| {
            default.and_then(|project| project.name.rsplit_once('@').map(|(_, host)| host))
        });
        let mut new = self
            .systems
            .iter()
            .flat_map(|system| {
                system
                    .idea_project_hosts(cx)
                    .into_iter()
                    .map(move |host| (system, host))
            })
            .filter(|(_, (_, alias))| host_hint.is_none_or(|host| alias.eq_ignore_ascii_case(host)))
            .filter_map(|(system, (host, alias))| {
                let value = format!("{name}@{alias}");
                (!rows
                    .iter()
                    .any(|(row, _)| row.value.eq_ignore_ascii_case(&value)))
                .then(|| {
                    (
                        Candidate {
                            value,
                            description: NEW_PROJECT.into(),
                        },
                        ProjectChoice::New(
                            system.clone(),
                            NewIdeaProject {
                                host,
                                name: name.to_owned(),
                            },
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();
        new.sort_by(|(a, _), (b, _)| a.value.cmp(&b.value));
        rows.extend(new);
        rows
    }

    fn edit_idea_to(&mut self, project: IdeaProject, window: &mut Window, cx: &mut Context<Self>) {
        let Some(overlay) = &self.idea.overlay else {
            return;
        };
        self.idea.project = Some(project.address);
        self.idea.recipient = project.name;
        overlay.address.update(cx, |address, cx| {
            address.set_text(self.idea.recipient.as_str(), window, cx)
        });
        self.save_idea(cx);
        window.focus(&overlay.editor.focus_handle(cx), cx);
        cx.notify();
    }

    pub(crate) fn close_idea_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(overlay) = self.idea.overlay.take() {
            window.focus(&overlay.return_focus, cx);
            cx.notify();
        }
    }

    pub(crate) fn cancel_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_idea_overlay(window, cx);
    }

    fn clear_idea(&mut self, cx: &mut Context<Self>) {
        let text = cx.new(|cx| Buffer::local("", cx));
        self.idea._edits = IdeaBox::subscribe(&text, cx);
        self.idea.text = text;
        self.idea.project = None;
        self.idea.recipient.clear();
        self.save_idea(cx);
    }

    pub(crate) fn send_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.idea_here(cx) {
            return;
        }
        let text = self.idea.text.read(cx).text();
        if text.trim().is_empty() {
            self.echo("send: nothing written", StyleClass::SystemInfo, cx);
            return;
        }
        let Some((system, project)) = self.resolved_idea_project(cx) else {
            self.choose_idea_project(window, cx);
            return;
        };
        self.idea.project = Some(project.address.clone());
        self.idea.recipient = project.name;
        self.save_idea(cx);
        // Keep the words until the project sends the mail, and restore the
        // previous screen before asking any first-start questions.
        self.close_idea_overlay(window, cx);
        let sent_text = text.clone();
        let sent_to = project.address.clone();
        let short = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim();
        let idea: String = short.chars().take(60).collect();
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
                    let idea = idea.clone();
                    frame.with(cx, move |frame, _, cx| {
                        if frame.idea.overlay.is_none()
                            && frame.idea.project.as_ref() == Some(&sent_to)
                            && frame.idea.text.read(cx).text() == sent_text
                        {
                            frame.clear_idea(cx);
                        }
                        frame.echo(
                            &format!("sent to {name}: {idea}"),
                            StyleClass::SystemInfo,
                            cx,
                        );
                    });
                }),
            },
            cx,
        );
    }

    pub(crate) fn idea_editor(&self) -> Option<Entity<Editor>> {
        self.idea
            .overlay
            .as_ref()
            .map(|overlay| overlay.editor.clone())
    }

    pub(crate) fn render_idea(&self, cx: &App) -> Option<gpui::AnyElement> {
        let overlay = self.idea.overlay.as_ref()?;
        let colors = cx.theme().colors();
        Some(
            div()
                .id("rho-idea")
                .key_context("RhoIdea")
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .key_context("RhoIdeaAddress")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .text_color(colors.text_muted)
                        .child("to:")
                        .child(div().flex_1().child(overlay.address.clone())),
                )
                .child(div().flex_1().min_h_0().child(overlay.editor.clone()))
                .into_any_element(),
        )
    }
}
