//! One local idea draft: the tail of Desk's buffer, or a temporary strip
//! elsewhere. The minibuffer addresses it. The project owns delivery.

use std::rc::Rc;

use editor::{Editor, EditorMode};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Window, div};
use language::{Buffer, BufferEvent, Capability};
use multi_buffer::MultiBuffer;
use rho_dealer::Address;
use rho_window::style::StyleClass;
use theme::ActiveTheme as _;

use crate::minibuffer::{Candidate, CandidateSource};
use crate::phone::Layout;
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
    pub(crate) text: Entity<Buffer>,
    to: Addressee,
    overlay: Option<Overlay>,
    _edits: gpui::Subscription,
}

enum Addressee {
    Unaddressed,
    Last(Address),
    Fixed(Address),
}

impl Addressee {
    fn address(&self) -> Option<&Address> {
        match self {
            Self::Unaddressed => None,
            Self::Last(address) | Self::Fixed(address) => Some(address),
        }
    }
}

struct Overlay {
    editor: Entity<Editor>,
    return_focus: FocusHandle,
}

impl IdeaBox {
    pub(crate) fn new(cx: &mut Context<Frame>, db: &rho_db::RhoDb) -> Self {
        let saved = place::idea_draft(db);
        let (to, words) = saved.unzip();
        let text = cx.new(|cx| {
            let mut text = Buffer::local(words.unwrap_or_default(), cx);
            if to.is_none() {
                text.set_capability(Capability::Read, cx);
            }
            text
        });
        Self {
            _edits: Self::subscribe(&text, cx),
            text,
            to: to.map_or(Addressee::Unaddressed, Addressee::Fixed),
            overlay: None,
        }
    }

    fn subscribe(text: &Entity<Buffer>, cx: &mut Context<Frame>) -> gpui::Subscription {
        cx.subscribe(text, |frame, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                if let Addressee::Last(address) = &frame.idea.to
                    && !frame.idea.text.read(cx).is_empty()
                {
                    // From the first words, changing the last project must
                    // not silently readdress what the user wrote.
                    frame.idea.to = Addressee::Fixed(address.clone());
                }
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

    fn project_for(&self, address: &Address, cx: &App) -> Option<(AnySystem, IdeaProject)> {
        self.systems.iter().find_map(|system| {
            let project = system.idea_project(address, cx)?;
            Some((system.clone(), project))
        })
    }

    fn draft_project(&self, cx: &App) -> Option<IdeaProject> {
        match self.idea.to.address() {
            // A missing project is not a reason to silently send an
            // addressed draft somewhere else.
            Some(address) => self.project_for(address, cx).map(|(_, project)| project),
            None => self.last_idea_project(cx),
        }
    }

    pub(crate) fn idea_here(&self, cx: &App) -> bool {
        self.idea.overlay.is_some() || (self.desk_in_view() && self.desk.read(cx).on_draft(cx))
    }

    fn idea_label(&self, layout: Layout, cx: &App) -> String {
        match (self.draft_project(cx), self.idea.to.address(), layout) {
            (Some(project), _, Layout::Desktop) => {
                format!("idea to {} · ctrl-enter sends", project.name)
            }
            (Some(project), _, Layout::Phone) => format!("idea to {}", project.name),
            (None, Some(_), _) => "idea · project unavailable".into(),
            (None, None, _) => "idea · choose a project".into(),
        }
    }

    pub(crate) fn sync_desk_idea(&mut self, layout: Layout, cx: &mut Context<Self>) {
        if !matches!(self.idea.to, Addressee::Fixed(_))
            && let Some(project) = self.last_idea_project(cx)
        {
            self.address_idea(Addressee::Last(project.address), cx);
        }
        let label = format!("── {}", self.idea_label(layout, cx));
        self.desk
            .update(cx, |desk, cx| desk.set_idea_label(label, cx));
    }

    fn address_idea(&mut self, to: Addressee, cx: &mut Context<Self>) {
        let was_unaddressed = matches!(self.idea.to, Addressee::Unaddressed);
        self.idea.to = to;
        if was_unaddressed {
            // An unaddressed tail can't receive words. Start a fresh buffer
            // when it's addressed rather than changing an existing excerpt's
            // capability; the multibuffer snapshots that with the excerpt.
            self.replace_idea_text(cx);
        }
    }

    fn replace_idea_text(&mut self, cx: &mut Context<Self>) {
        let text = cx.new(|cx| Buffer::local("", cx));
        self.idea._edits = IdeaBox::subscribe(&text, cx);
        self.idea.text = text.clone();
        self.desk.update(cx, |desk, cx| desk.set_draft(text, cx));
    }

    fn save_idea(&self, cx: &App) {
        let text = self.idea.text.read(cx).text();
        let draft = (!text.is_empty()).then(|| {
            let address = self
                .idea
                .to
                .address()
                .expect("only an addressed idea can be edited")
                .clone();
            (address, text)
        });
        self.place.write(Write::IdeaDraft(draft));
    }

    fn overlay_editor(text: Entity<Buffer>, window: &mut Window, cx: &mut App) -> Entity<Editor> {
        let buffer = cx.new(|cx| MultiBuffer::singleton(text, cx));
        cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::AutoHeight {
                    min_lines: 1,
                    max_lines: Some(8),
                },
                buffer,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_use_modal_editing(false);
            editor
        })
    }

    pub(crate) fn prompt_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() || self.menu.is_some() {
            return;
        }
        let default = if self.idea.overlay.is_some() || self.desk_in_view() {
            self.draft_project(cx)
        } else {
            self.default_idea_project(cx)
        };
        let label = match &default {
            Some(project) => format!("to ({}):", project.name),
            None => "to:".into(),
        };
        let default_name = default.as_ref().map(|project| project.name.clone());
        let complete: CandidateSource = Rc::new(move |frame, needle, cx| {
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
            // The default named in the question is the first answer when
            // it matches, not an invisible alternative to the highlight.
            projects.sort_by(|a, b| {
                let by_default = |row: &Candidate| default_name.as_ref() != Some(&row.value);
                (by_default(a), &a.value).cmp(&(by_default(b), &b.value))
            });
            projects
        });
        self.open_prompt(
            label,
            complete,
            Rc::new(move |frame, input, window, cx| {
                let input = input.trim();
                let project = if let Some(default) = default.as_ref().filter(|p| p.name == input) {
                    frame
                        .project_for(&default.address, cx)
                        .map(|(_, project)| project)
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
        // Tapping a project chooses it, just like Enter on its row.
        if let Some(prompt) = self.minibuffer_mut() {
            prompt.complete_project_address();
        }
    }

    fn edit_idea_to(&mut self, project: IdeaProject, window: &mut Window, cx: &mut Context<Self>) {
        let was_writable = self.idea.text.read(cx).capability() == Capability::ReadWrite;
        self.address_idea(Addressee::Fixed(project.address), cx);
        self.save_idea(cx);
        if self.desk_in_view() {
            self.sync_desk_idea(Layout::of(window.viewport_size().width), cx);
            if !was_writable || !self.desk.read(cx).on_draft(cx) {
                let desk = self.desk.clone();
                desk.update(cx, |desk, cx| desk.focus_draft(window, cx));
            }
        } else {
            if self.idea.overlay.is_none() {
                let return_focus = window
                    .focused(cx)
                    .unwrap_or_else(|| self.current_surface().focus_handle(cx));
                let editor = Self::overlay_editor(self.idea.text.clone(), window, cx);
                self.idea.overlay = Some(Overlay {
                    editor,
                    return_focus,
                });
            }
            let overlay = self
                .idea
                .overlay
                .as_ref()
                .expect("the idea overlay is open");
            window.focus(&overlay.editor.focus_handle(cx), cx);
        }
        cx.notify();
    }

    pub(crate) fn close_idea_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(overlay) = self.idea.overlay.take() {
            window.focus(&overlay.return_focus, cx);
            cx.notify();
        }
    }

    pub(crate) fn cancel_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.idea.overlay.is_some() {
            self.close_idea_overlay(window, cx);
        } else if self.desk_in_view() {
            vim::enter_normal_mode(&self.active_editor(cx), window, cx);
            self.desk.update(cx, |desk, cx| desk.focus_first_row(cx));
            cx.notify();
        }
    }

    fn clear_idea(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sent_to = self.idea.to.address().expect("the sent idea was addressed");
        self.idea.to = Addressee::Last(sent_to.clone());
        self.replace_idea_text(cx);
        if let Some(overlay) = &mut self.idea.overlay {
            let was_focused = overlay.editor.focus_handle(cx).is_focused(window);
            overlay.editor = Self::overlay_editor(self.idea.text.clone(), window, cx);
            if was_focused {
                window.focus(&overlay.editor.focus_handle(cx), cx);
            }
        }
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
        let Some((system, project)) = self
            .idea
            .to
            .address()
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
        if self.desk_in_view() {
            vim::enter_normal_mode(&self.active_editor(cx), window, cx);
        }
        self.close_idea_overlay(window, cx);
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
                        if frame.idea.to.address() == Some(&sent_to)
                            && frame.idea.text.read(cx).text() == sent_text
                        {
                            frame.clear_idea(window, cx);
                        }
                        frame.echo(&format!("sent to {name}"), StyleClass::SystemInfo, cx);
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

    pub(crate) fn render_idea(
        &self,
        text_style: &gpui::TextStyle,
        layout: Layout,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let editor = self.idea_editor()?;
        let label = self.idea_label(layout, cx);
        let colors = cx.theme().colors();
        Some(
            div()
                .id("rho-idea-box")
                .key_context("RhoIdea RhoIdeaOverlay")
                .flex_none()
                .w_full()
                .border_t_1()
                .border_color(colors.border_variant)
                .px_2()
                .py_1()
                .child(
                    div()
                        .id("rho-idea-address")
                        .cursor_pointer()
                        .font_family(text_style.font_family.clone())
                        .text_size(text_style.font_size)
                        .line_height(text_style.line_height)
                        .text_color(colors.text_muted)
                        .child(label)
                        .on_click(cx.listener(|frame, _, window, cx| {
                            frame.prompt_idea(window, cx);
                            cx.stop_propagation();
                        })),
                )
                .child(editor),
        )
    }
}
