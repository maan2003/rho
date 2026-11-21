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
use crate::{Frame, FrameLink};

/// A project address as it is written, with the stable identity of the
/// project that owns delivery. Neither its displayed name nor its host's
/// alias is used to identify it once the user has chosen it.
#[derive(Clone, Debug)]
pub struct IdeaProject {
    pub address: Address,
    pub name: String,
    /// Host-owned project settings, presented under the narrowed Desk title.
    pub details: Option<String>,
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

pub(crate) struct IdeaBox {
    pub(crate) text: Entity<Buffer>,
    project: Option<Address>,
    recipient: String,
    runtime: Option<String>,
    client_id: Option<u64>,
    overlay: Option<Overlay>,
    _edits: gpui::Subscription,
}

struct Overlay {
    editor: Entity<Editor>,
    address: Entity<Editor>,
    runtime: Entity<Editor>,
    return_focus: FocusHandle,
    _edits: [gpui::Subscription; 2],
}

impl IdeaBox {
    pub(crate) fn new(cx: &mut Context<Frame>, db: &rho_db::RhoDb) -> Self {
        let (project, recipient, runtime, client_id, words) = place::idea_draft(db)
            .map(|saved| {
                (
                    saved.address.and_then(|a| Address::parse(&a)),
                    saved.recipient,
                    saved.runtime,
                    saved.client_id,
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
            client_id,
            overlay: None,
        }
    }

    fn subscribe(text: &Entity<Buffer>, cx: &mut Context<Frame>) -> gpui::Subscription {
        cx.subscribe(text, |frame, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                if let Some(overlay) = &frame.idea.overlay {
                    overlay.editor.update(cx, Self::style_draft);
                }
                frame.idea.client_id = None;
                frame.save_idea(cx);
            }
        })
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
                runtime: self.idea.runtime.clone(),
                client_id: self.idea.client_id,
                text,
            });
        self.place.write(Write::IdeaDraft(draft));
    }

    pub(crate) fn prompt_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() || self.menu.is_some() {
            return;
        }
        if self.idea.overlay.is_some() {
            self.choose_idea_project(ProjectIntent::Address, window, cx);
            return;
        }
        if self.idea.client_id.is_some_and(|id| {
            self.systems
                .iter()
                .any(|system| system.idea_pending(id, cx))
        }) {
            self.echo("idea: waiting for the host", StyleClass::SystemInfo, cx);
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
                        frame.idea.client_id = None;
                        frame.save_idea(cx);
                        frame_link.with(cx, |frame, window, cx| {
                            frame.refresh_idea_runtime(window, cx);
                        });
                    }
                }
            },
        );
        let runtime = cx.new(|cx| {
            let mut field = Editor::single_line(window, cx);
            rho_window::editor_config::configure(&mut field, window, cx);
            field.set_use_modal_editing(false);
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
                        frame.idea.client_id = None;
                        frame.save_idea(cx);
                    }
                }
            },
        );
        self.idea.overlay = Some(Overlay {
            editor: editor.clone(),
            address,
            runtime,
            return_focus,
            _edits: [address_edits, runtime_edits],
        });
        self.refresh_idea_runtime(window, cx);
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

    /// The automatic answer is only a placeholder, not a copied setting
    /// or an explicit override. Editing the field replaces it with words.
    pub fn refresh_idea_runtime(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(overlay) = &self.idea.overlay else {
            return;
        };
        let label = self
            .resolved_idea_project(cx)
            .and_then(|(_, project)| project.runtime);
        let hint = label.as_ref().map_or("", RuntimeLabel::as_str);
        overlay.runtime.update(cx, |field, cx| {
            if field.placeholder_text(cx).as_deref() != Some(hint) {
                field.set_placeholder_text(hint, window, cx);
            }
        });
    }

    pub(crate) fn prompt_idea_runtime(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.idea_here(cx) {
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
        let Some(overlay) = &self.idea.overlay else {
            return;
        };
        self.idea.client_id = None;
        self.idea.runtime = Some(runtime.label());
        overlay
            .runtime
            .update(cx, |field, cx| field.set_text(runtime.as_str(), window, cx));
        self.save_idea(cx);
        window.focus(&overlay.runtime.focus_handle(cx), cx);
        cx.notify();
    }

    pub(crate) fn focus_idea_address(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(overlay) = &self.idea.overlay {
            let handle = if overlay.address.focus_handle(cx).is_focused(window) {
                overlay.runtime.focus_handle(cx)
            } else if overlay.runtime.focus_handle(cx).is_focused(window) {
                overlay.editor.focus_handle(cx)
            } else {
                overlay.address.focus_handle(cx)
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
                                    if frame.idea.overlay.is_some()
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
        let Some(overlay) = &self.idea.overlay else {
            return;
        };
        if self.idea.project.as_ref() != Some(&project.address) {
            self.idea.client_id = None;
        }
        self.idea.project = Some(project.address);
        self.idea.recipient = project.name;
        overlay.address.update(cx, |address, cx| {
            address.set_text(self.idea.recipient.as_str(), window, cx)
        });
        self.save_idea(cx);
        self.refresh_idea_runtime(window, cx);
        window.focus(&overlay.editor.focus_handle(cx), cx);
        cx.notify();
        if matches!(intent, ProjectIntent::Send) {
            self.send_idea(window, cx);
        }
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
        self.idea.runtime = None;
        self.idea.client_id = None;
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
        if self.idea.project.as_ref() != Some(&project.address) {
            self.idea.client_id = None;
        }
        self.idea.project = Some(project.address.clone());
        self.idea.recipient = project.name;
        let client_id = *self.idea.client_id.get_or_insert_with(|| {
            let bytes = *uuid::Uuid::new_v4().as_bytes();
            u64::from_le_bytes(bytes[8..].try_into().expect("UUID halves are 8 bytes"))
        });
        self.save_idea(cx);
        self.close_idea_overlay(window, cx);
        system.send_idea(
            IdeaMail {
                client_id,
                project: project.address,
                text,
                runtime,
            },
            cx,
        );
        self.echo(
            &format!("queued for {}", self.idea.recipient),
            StyleClass::SystemInfo,
            cx,
        );
    }

    /// Keep the submitted draft until the host answers. A refusal removes
    /// only its outbox attempt, so the words remain available to edit.
    pub fn finish_idea(&mut self, id: u64, result: Result<(), String>, cx: &mut Context<Self>) {
        if self.idea.client_id != Some(id) {
            return;
        }
        match result {
            Ok(()) => {
                let name = self.idea.recipient.clone();
                let idea: String = self
                    .idea
                    .text
                    .read(cx)
                    .text()
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .chars()
                    .take(60)
                    .collect();
                self.clear_idea(cx);
                self.echo(
                    &format!("sent to {name}: {idea}"),
                    StyleClass::SystemInfo,
                    cx,
                );
            }
            Err(reason) => self.echo(
                &format!("idea: {reason} · draft kept"),
                StyleClass::SystemImportant,
                cx,
            ),
        }
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
                .child(
                    div()
                        .key_context("RhoIdeaRuntime")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .text_color(colors.text_muted)
                        .child("on:")
                        .child(div().flex_1().child(overlay.runtime.clone())),
                )
                .child(div().flex_1().min_h_0().child(overlay.editor.clone()))
                .into_any_element(),
        )
    }
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

        fn open(
            &mut self,
            _: &Address,
            _: &mut Window,
            _: &mut Context<Self>,
        ) -> Option<crate::AnySurface> {
            None
        }

        fn idea_project_hosts(&self, _: &App) -> Vec<(EndpointId, String)> {
            vec![(EndpointId([1; 32]), "lab".into())]
        }

        fn idea_projects(&self, _: &App) -> Vec<IdeaProject> {
            self.projects.clone()
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
                details: None,
                runtime: None,
            };
            self.projects.push(project.clone());
            created(project, cx);
        }

        fn send_idea(&mut self, mail: IdeaMail, _: &mut Context<Self>) {
            self.sent.push(mail);
        }
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
                    assert_eq!(frame.idea.recipient, name);
                    assert_eq!(frame.idea.text.read(cx).text(), text);
                    assert_eq!(frame.idea_here(cx), !sending);
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
