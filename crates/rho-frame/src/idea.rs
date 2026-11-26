//! The full-window idea draft. Its address field completes against existing
//! projects; creating one is a separate, explicit choice.

use std::cell::RefCell;
use std::rc::Rc;

use editor::{Editor, EditorMode};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Window, div};
use language::{Buffer, BufferEvent, Point};
use multi_buffer::MultiBuffer;
use rho_agent_types::{EndpointId, RUNTIMES, RuntimeKind, RuntimeLabel};
use rho_dealer::Address;
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::minibuffer::{Candidate, CandidateSource};
use crate::place::{self, Write};
use crate::system::AnySystem;
use crate::{Frame, FrameLink, Mode, Surface};

/// A project address as it is written, with the stable identity of the
/// project that owns delivery. Neither its displayed name nor its host's
/// alias is used to identify it once the user has chosen it.
#[derive(Clone, Debug)]
pub struct IdeaProject {
    pub address: Address,
    pub name: String,
    pub runtime: Option<RuntimeLabel>,
}

/// A validated project name on the host that will own it.
#[derive(Clone)]
pub struct NewIdeaProject {
    pub host: EndpointId,
    pub name: rho_agent_types::ProjectName,
}

/// The common catalogue used by both the project and idea prompts.
pub fn runtime_candidates(needle: &str, first: Option<(&RuntimeLabel, &str)>) -> Vec<Candidate> {
    let needle = needle.trim().to_ascii_lowercase();
    let mut candidates: Vec<_> = RUNTIMES
        .into_iter()
        .map(str::to_owned)
        .chain(
            [RuntimeKind::Rho, RuntimeKind::Claude]
                .into_iter()
                .flat_map(RuntimeLabel::choices)
                .filter(|label| !RUNTIMES.contains(&label.as_str())),
        )
        .filter(|label| label.contains(&needle))
        .map(|value| {
            let description = first
                .filter(|(label, _)| label.as_str() == value)
                .map_or("", |(_, description)| description)
                .to_owned();
            Candidate { value, description }
        })
        .collect();
    if let Some((label, _)) = first {
        candidates.sort_by_key(|candidate| candidate.value != label.as_str());
    }
    candidates
}

pub(crate) const NEW_PROJECT: &str = "new project";

pub type ProjectCreated = Rc<dyn Fn(IdeaProject, &mut App)>;

#[derive(Clone)]
enum ProjectChoice {
    Existing(IdeaProject),
    New(AnySystem, NewIdeaProject),
}

#[derive(Clone, Copy)]
enum ProjectIntent {
    Address,
    Send,
}

/// A durable idea submission. Repeating it after a lost receipt uses the
/// same client id, so the host can answer without posting the idea twice.
pub struct IdeaMail {
    pub client_id: u64,
    pub project: Address,
    pub text: String,
    pub runtime: Option<RuntimeLabel>,
}

/// The idea the user is writing, kept across restarts until it is sent.
/// Sending hands it to the outbox and clears it at once, so a second idea
/// can be written while the first waits for its host.
pub(crate) struct IdeaBox {
    pub(crate) text: Entity<Buffer>,
    project: Option<Address>,
    recipient: String,
    runtime: Option<String>,
    _edits: gpui::Subscription,
}

/// The idea draft as a screen (`chat.Write an idea`): its `to:` and `on:`
/// lines over its words, which fill the window. It is a screen like any
/// other, so the window's keys act on it and never on the one before.
pub struct IdeaView {
    editor: Entity<Editor>,
    pub(crate) address: Entity<Editor>,
    runtime: Entity<Editor>,
    _edits: [gpui::Subscription; 2],
}

impl IdeaBox {
    pub(crate) fn new(cx: &mut Context<Frame>, db: &rho_db::RhoDb) -> Self {
        let (project, recipient, runtime, words) = place::idea_draft(db)
            .map(|saved| {
                (
                    saved.address.and_then(|a| Address::parse(&a)),
                    saved.recipient,
                    saved.runtime,
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
            runtime,
        }
    }

    fn subscribe(text: &Entity<Buffer>, cx: &mut Context<Frame>) -> gpui::Subscription {
        cx.subscribe(text, |frame, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                if let Some(view) = frame.idea_view() {
                    view.read(cx).editor.clone().update(cx, Self::style_draft);
                }
                frame.save_idea(cx);
            }
        })
    }

    fn is_empty(&self, cx: &App) -> bool {
        self.text.read(cx).is_empty() && self.recipient.is_empty() && self.runtime.is_none()
    }

    fn style_draft(editor: &mut Editor, cx: &mut Context<Editor>) {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let buffer = snapshot.as_singleton().expect("the idea has one buffer");
        let range = snapshot
            .buffer_anchor_range_to_anchor_range(
                buffer.anchor_before(Point::zero())..buffer.anchor_after(buffer.max_point()),
            )
            .expect("the idea excerpt contains its buffer");
        crate::draft::style(editor, range, "idea", cx);
    }
}

impl Surface for IdeaView {
    fn title(&self, _cx: &App) -> String {
        "idea".to_owned()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    /// The `to:` and `on:` lines are plain lines that always type; the
    /// words are a modal buffer, whose mode is vim's to say.
    fn mode(&self, window: &Window, cx: &App) -> Option<Mode> {
        [&self.address, &self.runtime]
            .into_iter()
            .any(|line| line.focus_handle(cx).is_focused(window))
            .then_some(Mode::Insert)
    }

    fn edits_at_point(&self, _cx: &App) -> bool {
        true
    }

    /// A fresh idea opens ready to type.
    fn opened(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        let editor = self.editor.clone();
        window.on_next_frame(move |window, cx| {
            if editor.focus_handle(cx).is_focused(window) && editor.read(cx).is_empty(cx) {
                vim::enter_insert_mode(&editor, window, cx);
            }
        });
    }
}

impl Focusable for IdeaView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for IdeaView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let line = |context: &'static str, label: &'static str, field: &Entity<Editor>| {
            div()
                .key_context(context)
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .px_2()
                .text_color(colors.text_muted)
                .child(label)
                .child(div().flex_1().child(field.clone()))
        };
        div()
            .id("rho-idea")
            .key_context("RhoIdea")
            .size_full()
            .flex()
            .flex_col()
            .child(line("RhoIdeaAddress", "to:", &self.address))
            .child(line("RhoIdeaRuntime", "on:", &self.runtime))
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
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

    /// The idea's screen. The frame holds it in history like any other;
    /// the draft itself is [`IdeaBox`]'s.
    pub(crate) fn idea_address() -> Address {
        Address::new(crate::frame::FRAME_KIND, "idea")
    }

    /// The idea's screen, while the window holds it.
    pub(crate) fn idea_view(&self) -> Option<Entity<IdeaView>> {
        self.surface(&Self::idea_address())?
            .view()
            .downcast::<IdeaView>()
            .ok()
    }

    pub(crate) fn idea_in_view(&self) -> bool {
        self.current_address() == &Self::idea_address()
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
                runtime: self.idea.runtime.clone(),
                text,
            });
        self.place.write(Write::IdeaDraft(draft));
    }

    pub(crate) fn prompt_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() || self.menu.is_some() {
            return;
        }
        if self.idea_in_view() {
            self.choose_idea_project(ProjectIntent::Address, window, cx);
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
                if let Some(view) = self.idea_view() {
                    let recipient = self.idea.recipient.clone();
                    view.read(cx).address.clone().update(cx, |address, cx| {
                        address.set_text(recipient.as_str(), window, cx)
                    });
                }
            }
        }
        self.open(Self::idea_address(), window, cx);
        self.refresh_idea_runtime(window, cx);
    }

    /// The idea's screen over the draft as it stands.
    pub(crate) fn make_idea_view(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<IdeaView> {
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
            IdeaBox::style_draft(&mut editor, cx);
            editor
        });
        let address = cx.new(|cx| {
            let mut address = Editor::single_line(window, cx);
            rho_window::editor_config::configure(&mut address, window, cx);
            address.set_use_modal_editing(false);
            address.set_text(self.idea.recipient.as_str(), window, cx);
            address
        });
        let frame_link = FrameLink::new(
            window
                .window_handle()
                .downcast::<Frame>()
                .expect("the frame owns the window"),
        );
        let address_edits = cx.subscribe(
            &address,
            move |frame, address, event: &editor::EditorEvent, cx| {
                if matches!(event, editor::EditorEvent::Edited { .. }) {
                    let recipient = address.read(cx).text(cx);
                    if frame.idea.recipient != recipient {
                        frame.idea.recipient = recipient;
                        frame.save_idea(cx);
                        frame_link.with(cx, |frame, window, cx| {
                            frame.refresh_idea_runtime(window, cx);
                        });
                    }
                }
            },
        );
        let hint = self
            .resolved_idea_project(cx)
            .and_then(|(_, project)| project.runtime);
        let runtime = cx.new(|cx| {
            let mut field = Editor::single_line(window, cx);
            rho_window::editor_config::configure(&mut field, window, cx);
            field.set_use_modal_editing(false);
            if let Some(hint) = &hint {
                field.set_placeholder_text(hint.as_str(), window, cx);
            }
            if let Some(runtime) = &self.idea.runtime {
                field.set_text(runtime.as_str(), window, cx);
            }
            field
        });
        let runtime_edits = cx.subscribe(
            &runtime,
            |frame, runtime, event: &editor::EditorEvent, cx| {
                if matches!(event, editor::EditorEvent::Edited { .. }) {
                    let text = runtime.read(cx).text(cx);
                    let override_text = (!text.is_empty()).then_some(text);
                    if frame.idea.runtime != override_text {
                        frame.idea.runtime = override_text;
                        frame.save_idea(cx);
                    }
                }
            },
        );
        cx.new(|_| IdeaView {
            editor,
            address,
            runtime,
            _edits: [address_edits, runtime_edits],
        })
    }

    /// The automatic answer is only a placeholder, not a copied setting
    /// or an explicit override. Editing the field replaces it with words.
    pub fn refresh_idea_runtime(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.idea_view() else {
            return;
        };
        let label = self
            .resolved_idea_project(cx)
            .and_then(|(_, project)| project.runtime);
        let hint = label.as_ref().map_or("", RuntimeLabel::as_str);
        view.read(cx).runtime.clone().update(cx, |field, cx| {
            if field.placeholder_text(cx).as_deref() != Some(hint) {
                field.set_placeholder_text(hint, window, cx);
            }
        });
    }

    pub(crate) fn prompt_idea_runtime(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.idea_in_view() {
            return;
        }
        let project = self
            .resolved_idea_project(cx)
            .and_then(|(_, project)| project.runtime);
        self.open_prompt(
            "on:",
            Rc::new(move |_, needle, _| {
                runtime_candidates(needle, project.as_ref().map(|label| (label, "project")))
            }),
            Rc::new(
                |frame, input, window, cx| match RuntimeLabel::parse(&input) {
                    Ok(runtime) => frame.edit_idea_runtime(runtime, window, cx),
                    Err(reason) => frame.echo(&reason, StyleClass::SystemImportant, cx),
                },
            ),
            window,
            cx,
        );
    }

    fn edit_idea_runtime(
        &mut self,
        runtime: RuntimeLabel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.idea_view() else {
            return;
        };
        let field = view.read(cx).runtime.clone();
        self.idea.runtime = Some(runtime.label());
        field.update(cx, |field, cx| field.set_text(runtime.as_str(), window, cx));
        self.save_idea(cx);
        window.focus(&field.focus_handle(cx), cx);
        cx.notify();
    }

    pub(crate) fn focus_idea_address(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.idea_view() {
            let view = view.read(cx);
            let handle = if view.address.focus_handle(cx).is_focused(window) {
                view.runtime.focus_handle(cx)
            } else if view.runtime.focus_handle(cx).is_focused(window) {
                view.editor.focus_handle(cx)
            } else {
                view.address.focus_handle(cx)
            };
            window.focus(&handle, cx);
        }
    }

    fn choose_idea_project(
        &mut self,
        intent: ProjectIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                        frame.choose_idea_to(project, intent, window, cx);
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
                                    if frame.idea_in_view()
                                        && frame.idea.recipient == before
                                        && frame.current_address() == &place
                                        && frame.minibuffer.is_none()
                                        && frame.menu.is_none()
                                    {
                                        frame.choose_idea_to(project, intent, window, cx);
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
        let Ok(name) = rho_agent_types::ProjectName::parse(name) else {
            return rows;
        };
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
                let value = format!("{}@{alias}", name.as_str());
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
                                name: name.clone(),
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

    fn choose_idea_to(
        &mut self,
        project: IdeaProject,
        intent: ProjectIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.idea_view() else {
            return;
        };
        let (address, editor) = {
            let view = view.read(cx);
            (view.address.clone(), view.editor.clone())
        };
        self.idea.project = Some(project.address);
        self.idea.recipient = project.name;
        let recipient = self.idea.recipient.clone();
        address.update(cx, |address, cx| {
            address.set_text(recipient.as_str(), window, cx)
        });
        self.save_idea(cx);
        self.refresh_idea_runtime(window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        cx.notify();
        if matches!(intent, ProjectIntent::Send) {
            self.send_idea(window, cx);
        }
    }

    /// Closes the idea's screen; the draft stays for next time.
    pub(crate) fn cancel_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close(&Self::idea_address(), window, cx);
    }

    fn clear_idea(&mut self, cx: &mut Context<Self>) {
        let text = cx.new(|cx| Buffer::local("", cx));
        self.idea._edits = IdeaBox::subscribe(&text, cx);
        self.idea.text = text;
        self.idea.project = None;
        self.idea.recipient.clear();
        self.idea.runtime = None;
        self.save_idea(cx);
    }

    /// Hands the idea in view to its project's outbox, which keeps it until
    /// the host has it, and clears the draft for the next idea.
    pub(crate) fn send_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.idea_in_view() {
            return;
        }
        let text = self.idea.text.read(cx).text();
        if text.trim().is_empty() {
            self.echo("send: nothing written", StyleClass::SystemInfo, cx);
            return;
        }
        let Some((system, project)) = self.resolved_idea_project(cx) else {
            self.choose_idea_project(ProjectIntent::Send, window, cx);
            return;
        };
        let runtime = match self
            .idea
            .runtime
            .as_deref()
            .map(RuntimeLabel::parse)
            .transpose()
        {
            Ok(runtime) => runtime,
            Err(reason) => {
                self.echo(&reason, StyleClass::SystemImportant, cx);
                return;
            }
        };
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let client_id = u64::from_le_bytes(bytes[8..].try_into().expect("UUID halves are 8 bytes"));
        system.send_idea(
            IdeaMail {
                client_id,
                project: project.address,
                text: text.clone(),
                runtime,
            },
            cx,
        );
        self.clear_idea(cx);
        self.close(&Self::idea_address(), window, cx);
        self.echo(
            &format!("queued for {}: {}", project.name, first_line(&text)),
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// Takes an idea the host refused back into the draft, to change and
    /// send again, when no other idea is being written; `false` leaves it
    /// where it was.
    pub fn write_idea_again(
        &mut self,
        idea: IdeaMail,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.idea.is_empty(cx) {
            self.echo(
                "idea: send or clear the idea being written first",
                StyleClass::SystemInfo,
                cx,
            );
            return false;
        }
        self.close(&Self::idea_address(), window, cx);
        self.idea
            .text
            .update(cx, |draft, cx| draft.set_text(idea.text, cx));
        self.idea.recipient = self
            .project_for(&idea.project, cx)
            .map(|(_, project)| project.name)
            .unwrap_or_default();
        self.idea.project = Some(idea.project);
        self.idea.runtime = idea.runtime.map(|runtime| runtime.label());
        self.save_idea(cx);
        self.prompt_idea(window, cx);
        true
    }
}

/// An idea as the echo names it: its first written line, cut short.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(60)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ProjectHost {
        projects: Vec<IdeaProject>,
        sent: Vec<IdeaMail>,
    }

    impl crate::System for ProjectHost {
        fn kinds(&self) -> &'static [&'static str] {
            &["project"]
        }

        fn open(&mut self, _: &Address, _: &mut Window, _: &mut Context<Self>) -> crate::Opening {
            crate::Opening::Gone
        }

        fn idea_project_hosts(&self, _: &App) -> Vec<(EndpointId, String)> {
            vec![(EndpointId([1; 32]), "lab".into())]
        }

        fn idea_projects(&self, _: &App) -> Vec<IdeaProject> {
            self.projects.clone()
        }

        fn idea_project(&self, address: &Address, _: &App) -> Option<IdeaProject> {
            self.projects
                .iter()
                .find(|project| &project.address == address)
                .cloned()
        }

        fn find(&self, _: &crate::Marks, _: &App) -> Vec<crate::system::Found> {
            self.projects
                .iter()
                .map(|project| crate::system::Found {
                    address: project.address.clone(),
                    title: project.name.clone(),
                    description: "on /srv/tide · one agent · opus-high".into(),
                    thread: None,
                    recency: 0,
                })
                .collect()
        }

        fn create_idea_project(
            &mut self,
            new: NewIdeaProject,
            created: ProjectCreated,
            cx: &mut Context<Self>,
        ) {
            let name = new.name.as_str();
            let project = IdeaProject {
                address: Address::new("project", name),
                name: format!("{name}@lab"),
                runtime: None,
            };
            self.projects.push(project.clone());
            created(project, cx);
        }

        fn send_idea(&mut self, mail: IdeaMail, _: &mut Context<Self>) {
            self.sent.push(mail);
        }
    }

    /// A project has no screen: choosing it in the palette filters the
    /// palette to its threads.
    #[gpui::test]
    fn choosing_a_project_in_the_palette_filters_to_its_threads(cx: &mut gpui::TestAppContext) {
        let host = cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            cx.new(|_| ProjectHost {
                projects: vec![IdeaProject {
                    address: Address::new("project", "tide"),
                    name: "tide@lab".into(),
                    runtime: None,
                }],
                sent: Vec::new(),
            })
        });
        let window = cx.add_window(|window, cx| {
            Frame::new(
                rho_db::RhoDb::in_memory(),
                vec![AnySystem::new(host.clone())],
                Vec::new(),
                Rc::new(|| {}),
                window,
                cx,
            )
        });
        window
            .update(cx, |frame, window, cx| {
                frame.palette(window, cx);
                frame
                    .minibuffer_mut()
                    .unwrap()
                    .set_input("tide@lab".into(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), crate::keys::MinibufferConfirm);
        cx.run_until_parked();
        window
            .update(cx, |frame, _, cx| {
                assert_eq!(frame.current_address(), &Frame::desk_address());
                let palette = frame.minibuffer_mut().expect("the palette stays open");
                assert_eq!(palette.input(cx), "tide@lab ");
            })
            .unwrap();
    }

    #[gpui::test]
    fn creating_a_project_keeps_the_intent_of_the_address_prompt(cx: &mut gpui::TestAppContext) {
        let host = cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            cx.new(|_| ProjectHost {
                projects: Vec::new(),
                sent: Vec::new(),
            })
        });
        let window = cx.add_window(|window, cx| {
            Frame::new(
                rho_db::RhoDb::in_memory(),
                vec![AnySystem::new(host.clone())],
                Vec::new(),
                Rc::new(|| {}),
                window,
                cx,
            )
        });
        let text = "Half a thought, not ready";
        for (sending, name) in [(false, "skiff@lab"), (true, "stern@lab")] {
            window
                .update(cx, |frame, window, cx| {
                    frame.prompt_idea(window, cx);
                    frame.idea.text.update(cx, |draft, cx| {
                        draft.edit([(0..draft.len(), text)], None, cx);
                    });
                    if sending {
                        frame.send_idea(window, cx);
                    } else {
                        frame.prompt_idea(window, cx);
                    }
                    frame
                        .minibuffer_mut()
                        .unwrap()
                        .set_input(name.into(), window, cx);
                })
                .unwrap();
            cx.run_until_parked();
            window
                .update(cx, |frame, window, cx| frame.minibuffer_confirm(window, cx))
                .unwrap();
            cx.run_until_parked();
            window
                .update(cx, |frame, window, cx| {
                    // A sent idea is the outbox's: the draft is clear for
                    // the next one.
                    let (recipient, words) = if sending { ("", "") } else { (name, text) };
                    assert_eq!(frame.idea.recipient, recipient);
                    assert_eq!(frame.idea.text.read(cx).text(), words);
                    assert_eq!(frame.idea_in_view(), !sending);
                    assert_eq!(host.read(cx).sent.len(), sending as usize);
                    if !sending {
                        frame.cancel_idea(window, cx);
                        frame.clear_idea(cx);
                    }
                })
                .unwrap();
        }
        cx.update(|cx| {
            let send = &host.read(cx).sent[0];
            assert_eq!(send.text, text);
            assert_eq!(send.project, Address::new("project", "stern"));
        });
    }

    /// `chat.Write an idea` step 3: a sent idea is the outbox's at once, so
    /// a second can be written while the first still waits for its host.
    #[gpui::test]
    fn a_second_idea_is_written_while_the_first_waits(cx: &mut gpui::TestAppContext) {
        let host = cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            cx.new(|_| ProjectHost {
                projects: vec![IdeaProject {
                    address: Address::new("project", "tide"),
                    name: "tide@lab".into(),
                    runtime: None,
                }],
                sent: Vec::new(),
            })
        });
        let db = rho_db::RhoDb::in_memory();
        let window = cx.add_window(|window, cx| {
            Frame::new(
                db.clone(),
                vec![AnySystem::new(host.clone())],
                Vec::new(),
                Rc::new(|| {}),
                window,
                cx,
            )
        });
        for words in ["Check the tide", "And the moon"] {
            window
                .update(cx, |frame, window, cx| {
                    frame.prompt_idea(window, cx);
                    assert!(frame.idea_in_view(), "{words}: the idea opens");
                    let view = frame.idea_view().unwrap();
                    assert_eq!(view.read(cx).editor.read(cx).text(cx), "", "{words}");
                    view.read(cx)
                        .address
                        .clone()
                        .update(cx, |address, cx| address.set_text("tide@lab", window, cx));
                    frame.idea.text.update(cx, |draft, cx| {
                        draft.edit([(0..0, words)], None, cx);
                    });
                })
                .unwrap();
            cx.run_until_parked();
            window
                .update(cx, |frame, window, cx| {
                    frame.send_idea(window, cx);
                    assert!(!frame.idea_in_view());
                    assert_eq!(frame.idea.text.read(cx).text(), "");
                    assert_eq!(frame.idea.recipient, "");
                })
                .unwrap();
            cx.run_until_parked();
        }
        cx.update(|cx| {
            let sent = &host.read(cx).sent;
            let words: Vec<_> = sent.iter().map(|mail| mail.text.as_str()).collect();
            assert_eq!(words, ["Check the tide", "And the moon"]);
            assert_ne!(sent[0].client_id, sent[1].client_id);
            assert!(
                sent.iter()
                    .all(|mail| mail.project == Address::new("project", "tide"))
            );
        });
        assert_eq!(
            place::idea_draft(&db),
            None,
            "nothing is left to send twice"
        );
    }

    #[gpui::test]
    fn a_refused_idea_comes_back_only_into_an_empty_draft(cx: &mut gpui::TestAppContext) {
        let host = cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            cx.new(|_| ProjectHost {
                projects: vec![IdeaProject {
                    address: Address::new("project", "tide"),
                    name: "tide@lab".into(),
                    runtime: None,
                }],
                sent: Vec::new(),
            })
        });
        let window = cx.add_window(|window, cx| {
            Frame::new(
                rho_db::RhoDb::in_memory(),
                vec![AnySystem::new(host.clone())],
                Vec::new(),
                Rc::new(|| {}),
                window,
                cx,
            )
        });
        let refused = || IdeaMail {
            client_id: 3,
            project: Address::new("project", "tide"),
            text: "Check the tide".into(),
            runtime: Some(RuntimeLabel::parse("sol-high").unwrap()),
        };
        window
            .update(cx, |frame, window, cx| {
                frame.idea.text.update(cx, |draft, cx| {
                    draft.edit([(0..0, "Another thought")], None, cx);
                });
                assert!(!frame.write_idea_again(refused(), window, cx));
                assert_eq!(frame.idea.text.read(cx).text(), "Another thought");
                frame.clear_idea(cx);
                assert!(frame.write_idea_again(refused(), window, cx));
                assert!(frame.idea_in_view());
                assert_eq!(frame.idea.text.read(cx).text(), "Check the tide");
                assert_eq!(frame.idea.recipient, "tide@lab");
                assert_eq!(frame.idea.runtime.as_deref(), Some("sol-high"));
                let view = frame.idea_view().unwrap();
                assert_eq!(view.read(cx).address.read(cx).text(cx), "tide@lab");
            })
            .unwrap();
    }

    #[test]
    fn runtime_choices_rank_the_project_without_losing_unfeatured_pairs() {
        let current = RuntimeLabel::parse("haiku-max").unwrap();
        let rows = runtime_candidates("", Some((&current, "project")));
        assert_eq!(
            (&*rows[0].value, &*rows[0].description),
            ("haiku-max", "project")
        );
        assert_eq!(rows[1].value, "opus-high");
        assert_eq!(rows.iter().filter(|row| row.value == "sol-high").count(), 1);
        assert!(
            rows.iter()
                .all(|row| RuntimeLabel::parse(&row.value).is_ok())
        );
        let filtered = runtime_candidates(" XHigH ", None);
        assert!(filtered.iter().any(|row| row.value == "astra-xhigh"));
        assert!(!filtered.iter().any(|row| row.value == "opus-high"));
    }
}
