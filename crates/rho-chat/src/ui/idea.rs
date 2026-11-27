//! The idea draft as a screen (`chat.Write an idea`): its `to:`, `in:` and
//! `on:` lines over its words, which fill the window. The `to:` line
//! completes against existing projects; making one is a separate, explicit
//! choice. The `in:` and `on:` lines show the project's mode and runtime as
//! quiet hints until the user writes on them: an untouched line sends
//! nothing, so the host keeps a standing agent's runtime as it is.
//!
//! The draft is chat's, kept beside the thread drafts ([`crate::cache`])
//! across restarts until it is sent. Sending hands it to the outbox and
//! clears it at once, so a second idea can be written while the first
//! waits for its host.

use std::cell::RefCell;
use std::rc::Rc;

use editor::{Editor, EditorEvent, EditorMode};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, KeyBinding, Subscription, WeakEntity, Window,
    actions, div,
};
use language::{Buffer, BufferEvent, Point};
use multi_buffer::MultiBuffer;
use rho_agent_types::{ProjectMode, ProjectName, RuntimeLabel};
use rho_dealer::{Address, Addressed as _};
use rho_frame::minibuffer::NEW_PROJECT;
use rho_frame::surface::PhoneSend;
use rho_frame::{Candidate, CandidateSource, Frame, Mode, Surface, keys};
use rho_window::style::StyleClass;

use super::ChatSystem;
use super::pending::PendingIdeaId;
use super::projects::{NewProject, ProjectCreated, runtime_choices};
use super::sheet::{self, Heading, token};
use crate::cache::IdeaDraft;
use crate::{ThreadId, names};

actions!(
    rho_idea,
    [
        /// `enter` on the `on:` line: choose a runtime from the host's.
        ChooseRuntime,
        /// `enter` on the `in:` line: choose `one` or `new`.
        ChooseMode,
        /// `shift-tab`: the next of the `to:`, `in:` and `on:` lines and
        /// the words.
        NextLine,
        /// `ctrl-enter`: hand the idea to its project.
        SendIdea,
    ]
);

/// The kind of chat's own screens, which no host holds.
pub(super) const KIND: &str = "chat";

/// The idea's screen. A device writes one idea at a time.
pub(super) fn address() -> Address {
    Address::new(KIND, "idea")
}

pub(super) fn bind(cx: &mut App) {
    let screen = "RhoIdea > Editor";
    cx.bind_keys([
        // Over the idea, the idea key chooses its project.
        KeyBinding::new("ctrl-t", keys::PromptIdea, Some(screen)),
        KeyBinding::new("enter", keys::PromptIdea, Some("RhoIdeaAddress > Editor")),
        KeyBinding::new("enter", ChooseRuntime, Some("RhoIdeaRuntime > Editor")),
        KeyBinding::new("enter", ChooseMode, Some("RhoIdeaMode > Editor")),
        KeyBinding::new("shift-tab", NextLine, Some(screen)),
        KeyBinding::new("ctrl-enter", SendIdea, Some(screen)),
        // The idea is a screen: `ctrl-g` closes it as `q` does, from any
        // mode, and the draft stays for next time.
        KeyBinding::new("ctrl-g", keys::Close, Some(screen)),
    ]);
}

/// A project an idea can go to, as the `to:` line names it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct IdeaProject {
    pub(super) thread: ThreadId,
    /// Its address as the window shows it: `tide`, or `tide@lab` while
    /// another host has a `tide` too.
    pub(super) name: String,
    /// Its address with its host, `tide@lab`, which always names it.
    pub(super) qualified: String,
    pub(super) runtime: Option<RuntimeLabel>,
    pub(super) mode: ProjectMode,
}

impl IdeaProject {
    /// Whether the typed address names this project, in either form.
    fn answers(&self, typed: &str) -> bool {
        self.name.eq_ignore_ascii_case(typed) || self.qualified.eq_ignore_ascii_case(typed)
    }

    /// Whether either form of its address holds the lowercase `needle`.
    pub(super) fn contains(&self, needle: &str) -> bool {
        [&self.name, &self.qualified]
            .into_iter()
            .any(|address| address.to_lowercase().contains(needle))
    }
}

/// A durable idea submission. Repeating it after a lost receipt uses the
/// same client id, so the host can answer without posting the idea twice.
pub(super) struct IdeaMail {
    pub(super) client_id: u64,
    pub(super) project: ThreadId,
    pub(super) text: String,
    pub(super) runtime: Option<RuntimeLabel>,
    pub(super) mode: Option<ProjectMode>,
}

/// How the `in:` line says a mode: `one` for the project's standing agent,
/// `new` for a fresh agent in a fresh workset.
pub(super) fn mode_word(mode: ProjectMode) -> &'static str {
    match mode {
        ProjectMode::One => "one",
        ProjectMode::PerIdea => "new",
    }
}

/// The mode an `in:` line names, or why it names none.
fn parse_mode_word(text: &str) -> Result<ProjectMode, String> {
    let text = text.trim().to_lowercase();
    ProjectMode::ALL
        .into_iter()
        .find(|mode| mode_word(*mode) == text)
        .ok_or_else(|| format!("in: `{text}` is neither one nor new"))
}

/// The idea being written. Its words outlive its screen: closing the
/// screen keeps them for next time.
pub(super) struct Idea {
    text: Entity<Buffer>,
    /// The project chosen for it, while the `to:` line still names it.
    project: Option<ThreadId>,
    /// The `to:` line as written.
    recipient: String,
    /// The `on:` line as written, when it overrides the project's runtime.
    runtime: Option<String>,
    /// The `in:` line as written, when it overrides the project's mode.
    mode: Option<String>,
    /// Its screen, while the window holds it.
    view: WeakEntity<IdeaView>,
    _edits: Subscription,
}

impl Idea {
    pub(super) fn load(saved: Option<IdeaDraft>, cx: &mut Context<ChatSystem>) -> Self {
        let saved = saved.unwrap_or_default();
        let text = cx.new(|cx| Buffer::local(saved.text, cx));
        Self {
            _edits: Self::subscribe(&text, cx),
            text,
            project: saved
                .address
                .as_deref()
                .and_then(Address::parse)
                .as_ref()
                .and_then(ThreadId::from_address),
            recipient: saved.recipient,
            runtime: saved.runtime,
            mode: saved.mode,
            view: WeakEntity::new_invalid(),
        }
    }

    fn subscribe(text: &Entity<Buffer>, cx: &mut Context<ChatSystem>) -> Subscription {
        cx.subscribe(text, |chat, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                chat.save_idea(cx);
            }
        })
    }

    fn is_empty(&self, cx: &App) -> bool {
        self.text.read(cx).is_empty()
            && self.recipient.is_empty()
            && self.runtime.is_none()
            && self.mode.is_none()
    }
}

impl ChatSystem {
    /// Every project an idea can go to. Of roots with the same address,
    /// only the newest; a contextual default knows its exact id.
    pub(super) fn idea_projects(&self) -> Vec<IdeaProject> {
        let names = self.project_names();
        let names = &names;
        self.logs
            .iter()
            .flat_map(|(&host, chat)| {
                chat.roots().iter().filter_map(move |&message| {
                    if chat.is_retired(message) {
                        return None;
                    }
                    let thread = ThreadId { host, message };
                    let project = self.idea_project_in(names, thread)?;
                    (self.project_named(&project.qualified) == Some(thread)).then_some(project)
                })
            })
            .collect()
    }

    /// The project a thread is in, or the project itself.
    pub(super) fn idea_project_of(&self, address: &Address) -> Option<IdeaProject> {
        self.idea_project_at(self.project_root(self.thread_of(address)?)?)
    }

    /// The project the `to:` line names: the one chosen for it while the
    /// line still names it, or else the project of that name. A bare name
    /// two hosts share names neither.
    fn resolved_idea_project(&self) -> Option<IdeaProject> {
        let typed = self.idea.recipient.trim();
        if let Some(project) = self
            .idea
            .project
            .and_then(|project| self.idea_project_at(project))
            && project.answers(typed)
            && !self.retired(project.thread)
        {
            return Some(project);
        }
        self.idea_projects()
            .into_iter()
            .find(|project| project.answers(typed))
    }

    /// Whether the user retired the project, which takes no more ideas.
    fn retired(&self, project: ThreadId) -> bool {
        self.host_chat(project.host)
            .is_some_and(|chat| chat.is_retired(project.message))
    }

    fn save_idea(&self, cx: &App) {
        let idea = &self.idea;
        let text = idea.text.read(cx).text();
        let draft = (!text.is_empty() || !idea.recipient.is_empty()).then(|| IdeaDraft {
            address: idea.project.map(|project| project.address().to_string()),
            recipient: idea.recipient.clone(),
            runtime: idea.runtime.clone(),
            mode: idea.mode.clone(),
            text,
        });
        self.cache.write_idea(draft);
    }

    fn clear_idea(&mut self, cx: &mut Context<Self>) {
        let text = cx.new(|cx| Buffer::local("", cx));
        self.idea._edits = Idea::subscribe(&text, cx);
        self.idea.text = text;
        self.idea.project = None;
        self.idea.recipient.clear();
        self.idea.runtime = None;
        self.idea.mode = None;
        self.save_idea(cx);
    }

    /// A `to:` line nobody has written names the project chosen for the
    /// draft or, while the draft is empty, the project of the screen the
    /// user is on (`here`: a thread's, or a transcript's or tool's work's),
    /// or else the last one they wrote to.
    fn address_idea(
        &mut self,
        here: Option<ThreadId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.idea.recipient.is_empty() {
            return;
        }
        let project = match self.idea.project {
            Some(project) => self.idea_project_at(project),
            None if self.idea.text.read(cx).is_empty() => here
                .and_then(|here| self.idea_project_at(here))
                .or_else(|| self.idea_project_at(self.last_project?)),
            None => None,
        }
        .filter(|project| !self.retired(project.thread));
        let Some(project) = project else {
            return;
        };
        self.idea.project = Some(project.thread);
        self.idea.recipient = project.name;
        if let Some(view) = self.idea.view.upgrade() {
            let recipient = self.idea.recipient.clone();
            view.read(cx).address.clone().update(cx, |address, cx| {
                address.set_text(recipient.as_str(), window, cx)
            });
        }
    }

    /// The idea's screen over the draft as it stands.
    pub(super) fn idea_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<IdeaView> {
        let chat = cx.entity();
        let view = cx.new(|cx| IdeaView::new(chat, self, window, cx));
        self.idea.view = view.downgrade();
        view
    }

    fn project_choices(
        &self,
        input: &str,
        default: Option<&IdeaProject>,
    ) -> Vec<(Candidate, ProjectChoice)> {
        let input = rho_agent_types::one_line(input);
        let needle = input.to_lowercase();
        let mut rows: Vec<_> = self
            .idea_projects()
            .into_iter()
            .filter(|project| {
                default.is_none_or(|default| {
                    !project.qualified.eq_ignore_ascii_case(&default.qualified)
                })
            })
            .chain(default.cloned())
            .filter(|project| project.contains(&needle))
            .collect();
        rows.sort_by(|a, b| {
            let key = |project: &IdeaProject| {
                (
                    !project.answers(&input),
                    default.is_none_or(|default| default.thread != project.thread),
                )
            };
            (key(a), &a.name).cmp(&(key(b), &b.name))
        });
        let mut rows: Vec<_> = rows
            .into_iter()
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
        if input.is_empty() || !rows.is_empty() {
            return rows;
        }
        let (name, requested_host) = names::split(&input);
        let Ok(name) = ProjectName::parse(name) else {
            return rows;
        };
        let host_hint = requested_host
            .or_else(|| default.and_then(|project| names::split(&project.qualified).1));
        let hosts = self.hosts();
        let mut new: Vec<_> = hosts
            .iter()
            .filter(|(_, alias)| host_hint.is_none_or(|host| alias.eq_ignore_ascii_case(host)))
            .map(|&(host, _)| {
                let project = NewProject {
                    host,
                    name: name.clone(),
                };
                (
                    Candidate {
                        value: self.new_project_address(&project),
                        description: NEW_PROJECT.into(),
                    },
                    ProjectChoice::New(project),
                )
            })
            .collect();
        new.sort_by(|(a, _), (b, _)| a.value.cmp(&b.value));
        rows.extend(new);
        rows
    }
}

#[derive(Clone)]
enum ProjectChoice {
    Existing(IdeaProject),
    New(NewProject),
}

/// What the idea goes on to do once its project is chosen.
#[derive(Clone, Copy)]
enum Intent {
    Address,
    Send,
}

/// One of the idea's steps, which needs the window's prompt and history.
type Step = fn(Entity<ChatSystem>, &mut Frame, &mut Window, &mut Context<Frame>);

/// Runs `step` on the frame once the current update has finished.
fn later(chat: Entity<ChatSystem>, step: Step, cx: &mut App) {
    let link = chat.read(cx).frame.clone();
    link.with(cx, move |frame, window, cx| step(chat, frame, window, cx));
}

/// The idea key, wherever the window offers it.
pub(super) fn prompt_later(chat: Entity<ChatSystem>, cx: &mut App) {
    later(chat, prompt, cx);
}

fn in_view(frame: &Frame) -> bool {
    frame.current_address() == &address()
}

/// The idea key (`chat.Write an idea` step 1): the idea's screen, addressed
/// from where the user is. Over the idea it chooses the project.
fn prompt(
    chat: Entity<ChatSystem>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    if frame.is_asking() {
        return;
    }
    if in_view(frame) {
        choose_project(chat, Intent::Address, frame, window, cx);
        return;
    }
    // The system that owns the screen in view says its project: a thread's
    // own, or the work's thread's for a transcript or one of its tools.
    let here = frame
        .project_of(frame.current_address(), cx)
        .and_then(|project| ThreadId::from_address(&project.address));
    chat.update(cx, |chat, cx| chat.address_idea(here, window, cx));
    frame.open(address(), window, cx);
}

fn choose_project(
    chat: Entity<ChatSystem>,
    intent: Intent,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let (default, recipient) = {
        let chat = chat.read(cx);
        (chat.resolved_idea_project(), chat.idea.recipient.clone())
    };
    let prefill = default.is_none().then_some(recipient);
    let choices = Rc::new(RefCell::new(Vec::new()));
    let shown = choices.clone();
    let source = chat.clone();
    let complete: CandidateSource = Rc::new(move |_, needle, cx| {
        let rows = source.read(cx).project_choices(needle, default.as_ref());
        let candidates = rows.iter().map(|(row, _)| row.clone()).collect();
        *shown.borrow_mut() = rows;
        candidates
    });
    frame.open_prompt(
        "to:",
        complete,
        Rc::new(move |frame, input, window, cx| {
            let choice = choices
                .borrow()
                .iter()
                .find(|(row, _)| row.value == input)
                .map(|(_, choice)| choice.clone());
            match choice {
                Some(ProjectChoice::Existing(project)) => {
                    choose_to(chat.clone(), project, intent, frame, window, cx)
                }
                Some(ProjectChoice::New(project)) => {
                    let created = when_created(chat.clone(), intent, cx);
                    let runtime = chat.read(cx).idea.runtime.clone();
                    ChatSystem::prompt_project(
                        chat.clone(),
                        project,
                        Some(created),
                        runtime.as_deref(),
                        frame,
                        window,
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
    if let Some(prompt) = frame.minibuffer_mut() {
        prompt.complete_project_address();
        if let Some(prefill) = prefill {
            prompt.set_input(prefill, window, cx);
        }
    }
}

/// Once the project chosen on the `to:` line exists, the idea goes to it,
/// and is sent when sending asked for it, unless the user has moved on.
fn when_created(chat: Entity<ChatSystem>, intent: Intent, cx: &App) -> ProjectCreated {
    let before = chat.read(cx).idea.recipient.clone();
    // Asked from inside chat's own update, which holds it.
    let link = chat.read(cx).frame.clone();
    Rc::new(move |project, cx| {
        let chat = chat.clone();
        let before = before.clone();
        link.with(cx, move |frame, window, cx| {
            if in_view(frame) && chat.read(cx).idea.recipient == before && !frame.is_asking() {
                choose_to(chat, project, intent, frame, window, cx);
            }
        });
    })
}

fn choose_to(
    chat: Entity<ChatSystem>,
    project: IdeaProject,
    intent: Intent,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let Some(view) = chat.read(cx).idea.view.upgrade() else {
        return;
    };
    chat.update(cx, |chat, cx| {
        chat.idea.project = Some(project.thread);
        chat.idea.recipient = project.name.clone();
        chat.save_idea(cx);
    });
    view.update(cx, |view, cx| {
        view.address.update(cx, |address, cx| {
            address.set_text(project.name.as_str(), window, cx)
        });
        view.refresh_hints(window, cx);
        window.focus(&view.editor.focus_handle(cx), cx);
    });
    if matches!(intent, Intent::Send) {
        send(chat, frame, window, cx);
    }
}

/// Hands the idea in view to its project's outbox, which keeps it until
/// the host has it, and clears the draft for the next idea.
fn send(chat: Entity<ChatSystem>, frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>) {
    if !in_view(frame) {
        return;
    }
    let (text, project, runtime, mode) = {
        let chat = chat.read(cx);
        (
            chat.idea.text.read(cx).text(),
            chat.resolved_idea_project(),
            chat.idea.runtime.clone(),
            chat.idea.mode.clone(),
        )
    };
    if text.trim().is_empty() {
        frame.echo("send: nothing written", StyleClass::SystemInfo, cx);
        return;
    }
    let Some(project) = project else {
        choose_project(chat, Intent::Send, frame, window, cx);
        return;
    };
    let runtime = match runtime.as_deref() {
        Some(text) => {
            let Some(catalogue) = chat.read(cx).runtime_catalogue(project.thread.host) else {
                frame.echo(
                    "the project's host is not attached",
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            };
            match catalogue.label(text) {
                Ok(runtime) => Some(runtime),
                Err(reason) => {
                    frame.echo(&reason, StyleClass::SystemImportant, cx);
                    return;
                }
            }
        }
        None => None,
    };
    let mode = match mode.as_deref().map(parse_mode_word).transpose() {
        Ok(mode) => mode,
        Err(reason) => return frame.echo(&reason, StyleClass::SystemImportant, cx),
    };
    let echo = format!("queued for {}: {}", project.name, first_line(&text));
    let client_id = rand::random();
    chat.update(cx, |chat, cx| {
        chat.send_idea(
            IdeaMail {
                client_id,
                project: project.thread,
                text,
                runtime,
                mode,
            },
            cx,
        );
        chat.clear_idea(cx);
    });
    frame.close(&address(), window, cx);
    // The sent idea is a place now, the one just left (`chat.Write an
    // idea`); it becomes its thread where it stands once the host has it.
    frame.remember(PendingIdeaId(client_id).address());
    frame.echo(&echo, StyleClass::SystemInfo, cx);
}

/// Takes an idea the host refused back into the draft, to change and send
/// again, when no other idea is being written; `false` leaves it where it
/// was.
pub(super) fn write_again(
    chat: Entity<ChatSystem>,
    idea: IdeaMail,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) -> bool {
    if !chat.read(cx).idea.is_empty(cx) {
        frame.echo(
            "idea: send or clear the idea being written first",
            StyleClass::SystemInfo,
            cx,
        );
        return false;
    }
    frame.close(&address(), window, cx);
    chat.update(cx, |chat, cx| {
        chat.idea
            .text
            .update(cx, |draft, cx| draft.set_text(idea.text, cx));
        chat.idea.recipient = chat
            .idea_project_at(idea.project)
            .map(|project| project.name)
            .unwrap_or_default();
        chat.idea.project = Some(idea.project);
        chat.idea.runtime = idea.runtime.map(|runtime| runtime.label());
        chat.idea.mode = idea.mode.map(|mode| mode_word(mode).to_owned());
        chat.save_idea(cx);
    });
    prompt(chat, frame, window, cx);
    true
}

fn choose_runtime(
    chat: Entity<ChatSystem>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    if !in_view(frame) {
        return;
    }
    let (catalogue, project) = {
        let chat = chat.read(cx);
        let Some(resolved) = chat.resolved_idea_project() else {
            frame.echo("to: choose a project first", StyleClass::SystemInfo, cx);
            return;
        };
        let Some(catalogue) = chat.runtime_catalogue(resolved.thread.host) else {
            frame.echo(
                "the project's host is not attached",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        (catalogue, resolved.runtime)
    };
    let choices = catalogue.clone();
    frame.open_prompt(
        "on:",
        Rc::new(move |_, needle, _| {
            runtime_choices(
                &choices,
                needle,
                project.as_ref().map(|label| (label, "project")),
            )
        }),
        Rc::new(
            move |frame, input, window, cx| match catalogue.label(&input) {
                Ok(runtime) => set_runtime(&chat, runtime, window, cx),
                Err(reason) => frame.echo(&reason, StyleClass::SystemImportant, cx),
            },
        ),
        window,
        cx,
    );
}

fn set_runtime(
    chat: &Entity<ChatSystem>,
    runtime: RuntimeLabel,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(view) = chat.read(cx).idea.view.upgrade() else {
        return;
    };
    chat.update(cx, |chat, cx| {
        chat.idea.runtime = Some(runtime.label());
        chat.save_idea(cx);
    });
    let field = view.read(cx).runtime.clone();
    field.update(cx, |field, cx| field.set_text(runtime.as_str(), window, cx));
    window.focus(&field.focus_handle(cx), cx);
}

/// `enter` on the `in:` line: `one` or `new`, the project's own mode first.
fn choose_mode(
    chat: Entity<ChatSystem>,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    if !in_view(frame) {
        return;
    }
    let Some(project) = chat.read(cx).resolved_idea_project() else {
        frame.echo("to: choose a project first", StyleClass::SystemInfo, cx);
        return;
    };
    frame.open_prompt(
        "in:",
        Rc::new(move |_, needle, _| {
            let mut modes = ProjectMode::ALL;
            modes.sort_by_key(|mode| *mode != project.mode);
            modes
                .into_iter()
                .filter(|mode| mode_word(*mode).starts_with(needle.trim()))
                .map(|mode| Candidate {
                    value: mode_word(mode).to_owned(),
                    description: match mode {
                        ProjectMode::One => "the project's standing agent",
                        ProjectMode::PerIdea => "a fresh agent in its own workset",
                    }
                    .to_owned(),
                })
                .collect()
        }),
        Rc::new(
            move |frame, input, window, cx| match parse_mode_word(&input) {
                Ok(mode) => set_mode(&chat, mode, window, cx),
                Err(reason) => frame.echo(&reason, StyleClass::SystemImportant, cx),
            },
        ),
        window,
        cx,
    );
}

fn set_mode(chat: &Entity<ChatSystem>, mode: ProjectMode, window: &mut Window, cx: &mut App) {
    let Some(view) = chat.read(cx).idea.view.upgrade() else {
        return;
    };
    chat.update(cx, |chat, cx| {
        chat.idea.mode = Some(mode_word(mode).to_owned());
        chat.save_idea(cx);
    });
    let field = view.read(cx).mode.clone();
    field.update(cx, |field, cx| field.set_text(mode_word(mode), window, cx));
    window.focus(&field.focus_handle(cx), cx);
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

/// The idea draft as a screen: its `to:`, `in:` and `on:` lines over its
/// words. It is a screen like any other, so the window's keys act on it
/// and never on the one before.
pub(super) struct IdeaView {
    chat: Entity<ChatSystem>,
    editor: Entity<Editor>,
    address: Entity<Editor>,
    mode: Entity<Editor>,
    runtime: Entity<Editor>,
    _subscriptions: [Subscription; 5],
}

impl IdeaView {
    fn new(
        chat: Entity<ChatSystem>,
        system: &ChatSystem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let idea = &system.idea;
        let buffer = cx.new(|cx| MultiBuffer::singleton(idea.text.clone(), cx));
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
            Self::style_words(&mut editor, cx);
            editor
        });
        let line = |text: Option<&str>, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut line = Editor::single_line(window, cx);
                rho_window::editor_config::configure(&mut line, window, cx);
                line.set_use_modal_editing(false);
                if let Some(text) = text {
                    line.set_text(text, window, cx);
                }
                line
            })
        };
        let address = line(Some(&idea.recipient), window, cx);
        let mode = line(idea.mode.as_deref(), window, cx);
        let runtime = line(idea.runtime.as_deref(), window, cx);
        let project = system.resolved_idea_project();
        if let Some(hint) = project
            .as_ref()
            .and_then(|project| project.runtime.as_ref())
        {
            runtime.update(cx, |field, cx| {
                field.set_placeholder_text(hint.as_str(), window, cx)
            });
        }
        if let Some(project) = &project {
            mode.update(cx, |field, cx| {
                field.set_placeholder_text(mode_word(project.mode), window, cx)
            });
        }
        let words = cx.subscribe(&idea.text, |view, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                view.editor.update(cx, Self::style_words);
            }
        });
        let address_edits = cx.subscribe_in(
            &address,
            window,
            |view, address, event: &EditorEvent, window, cx| {
                if matches!(event, EditorEvent::Edited { .. }) {
                    let recipient = address.read(cx).text(cx);
                    view.chat.update(cx, |chat, cx| {
                        if chat.idea.recipient != recipient {
                            chat.idea.recipient = recipient;
                            chat.save_idea(cx);
                        }
                    });
                    view.refresh_hints(window, cx);
                }
            },
        );
        let runtime_edits = cx.subscribe(&runtime, |view, runtime, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Edited { .. }) {
                let text = runtime.read(cx).text(cx);
                let written = (!text.is_empty()).then_some(text);
                view.chat.update(cx, |chat, cx| {
                    if chat.idea.runtime != written {
                        chat.idea.runtime = written;
                        chat.save_idea(cx);
                    }
                });
            }
        });
        let mode_edits = cx.subscribe(&mode, |view, mode, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Edited { .. }) {
                let text = mode.read(cx).text(cx);
                let written = (!text.is_empty()).then_some(text);
                view.chat.update(cx, |chat, cx| {
                    if chat.idea.mode != written {
                        chat.idea.mode = written;
                        chat.save_idea(cx);
                    }
                });
            }
        });
        // What the projects say moves under the hints.
        let projects = cx.observe_in(&chat, window, |view, _, window, cx| {
            view.refresh_hints(window, cx)
        });
        Self {
            chat,
            editor,
            address,
            mode,
            runtime,
            _subscriptions: [words, address_edits, mode_edits, runtime_edits, projects],
        }
    }

    fn style_words(editor: &mut Editor, cx: &mut Context<Editor>) {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let buffer = snapshot.as_singleton().expect("the idea has one buffer");
        let range = snapshot
            .buffer_anchor_range_to_anchor_range(
                buffer.anchor_before(Point::zero())..buffer.anchor_after(buffer.max_point()),
            )
            .expect("the idea excerpt contains its buffer");
        super::thread::style_draft_in(editor, range, cx);
    }

    /// The `in:` and `on:` lines' hints are the project's mode and runtime:
    /// placeholders, not copied settings, so they follow the `to:` line
    /// until the user writes on a line, which replaces its hint with words.
    fn refresh_hints(&self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.chat.read(cx).resolved_idea_project();
        let runtime = project
            .as_ref()
            .and_then(|project| project.runtime.as_ref())
            .map_or("", RuntimeLabel::as_str);
        let mode = project
            .as_ref()
            .map_or("", |project| mode_word(project.mode));
        for (field, hint) in [(&self.runtime, runtime), (&self.mode, mode)] {
            field.update(cx, |field, cx| {
                if field.placeholder_text(cx).as_deref() != Some(hint) {
                    field.set_placeholder_text(hint, window, cx);
                }
            });
        }
    }

    fn next_line(&mut self, _: &NextLine, window: &mut Window, cx: &mut Context<Self>) {
        let handle = if self.address.focus_handle(cx).is_focused(window) {
            self.mode.focus_handle(cx)
        } else if self.mode.focus_handle(cx).is_focused(window) {
            self.runtime.focus_handle(cx)
        } else if self.runtime.focus_handle(cx).is_focused(window) {
            self.editor.focus_handle(cx)
        } else {
            self.address.focus_handle(cx)
        };
        window.focus(&handle, cx);
    }
}

impl Surface for IdeaView {
    fn title(&self, _cx: &App) -> String {
        "idea".to_owned()
    }

    fn draws_top_left(&self, _cx: &App) -> bool {
        true
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    /// The `to:`, `in:` and `on:` lines are plain lines that always type;
    /// the words are a modal buffer, whose mode is vim's to say.
    fn mode(&self, window: &Window, cx: &App) -> Option<Mode> {
        [&self.address, &self.mode, &self.runtime]
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
                let start = editor
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .anchor_before(Point::zero());
                rho_window::draft::focus_writing_at(&editor, start, start, window, cx);
            }
        });
    }

    fn composes(&self, _cx: &App) -> bool {
        true
    }

    fn phone_send(&mut self, _cx: &mut Context<Self>) -> Option<PhoneSend> {
        let chat = self.chat.clone();
        Some(Rc::new(move |_, cx| later(chat.clone(), send, cx)))
    }
}

impl Focusable for IdeaView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for IdeaView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self
            .editor
            .update(cx, |editor, cx| editor.style(cx).text.clone());
        // The header lines start on the words' left edge, after the half
        // column where the words' bar is drawn.
        let gutter = sheet::column_width(&text, window) * 0.5;
        let line = |context: &'static str, label: &'static str, field: &Entity<Editor>| {
            div()
                .key_context(context)
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .pl(gutter)
                .text_color(token::quiet())
                .child(label)
                .child(div().flex_1().child(field.clone()))
        };
        // The words wrap at the prose measure, on the sheet's left edge.
        let words = sheet::column_width(&text, window) * (sheet::PROSE_COLUMNS as f32 + 1.5);
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(|view, _: &SendIdea, _, cx| later(view.chat.clone(), send, cx)))
            .on_action(cx.listener(|view, _: &ChooseRuntime, _, cx| {
                later(view.chat.clone(), choose_runtime, cx)
            }))
            .on_action(
                cx.listener(|view, _: &ChooseMode, _, cx| {
                    later(view.chat.clone(), choose_mode, cx)
                }),
            )
            .on_action(cx.listener(Self::next_line))
            .child(line("RhoIdeaAddress", "to:", &self.address))
            .child(line("RhoIdeaMode", "in:", &self.mode))
            .child(line("RhoIdeaRuntime", "on:", &self.runtime))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .max_w(words)
                    .pt(text.line_height_in_pixels(window.rem_size()) * 0.5)
                    .child(self.editor.clone()),
            );
        div()
            .id("rho-idea")
            .key_context("RhoIdea")
            .size_full()
            .child(sheet::page(&Heading::new("idea"), &text, body, window, cx))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use rho_agent_hosts::{HostId, Link};
    use rho_agent_types::{
        Address as Party, ChatEvent, ChatRecord, ChatSeq, ContentPart, EndpointId, MessageId,
        ProjectConfig, ProjectId, ProjectMode, ProjectPlace, RuntimeCatalogue, RuntimeChoice,
        RuntimeKind, UnixMs, Voice,
    };
    use rho_frame::System as _;

    use super::*;
    use crate::HostChat;
    use crate::ui::search_bench::fixture_on;

    const LAB: EndpointId = EndpointId([3; 32]);

    fn posted(seq: u64, parent: Option<u64>, text: &str) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq),
            event: ChatEvent::Posted {
                parent: parent.map(|parent| MessageId(ChatSeq(parent))),
                retry_of: None,
                thread_reply: None,
                from: Party::User,
                voice: Voice::Message,
                content: vec![ContentPart::Text { text: text.into() }],
            },
        }
    }

    fn thread(seq: u64) -> ThreadId {
        ThreadId {
            host: LAB,
            message: MessageId(ChatSeq(seq)),
        }
    }

    /// Two projects on `lab`, `tide` (1) on `haiku-max` and `reef` (2) on
    /// `opus-high`, and a thread (3) in `tide`.
    fn lab() -> HostChat {
        let mut log = HostChat::default();
        for record in [
            posted(1, None, "tide"),
            posted(2, None, "reef"),
            posted(3, Some(1), "Check the clock"),
        ] {
            log.tell(&record);
        }
        log
    }

    fn attach_with(
        chat: &Entity<ChatSystem>,
        projects: &[(u64, &str)],
        runtimes: RuntimeCatalogue,
        cx: &mut App,
    ) {
        chat.update(cx, |chat, cx| {
            chat.streams(HostId(0), "lab", Some(LAB), &Link::detached(), cx);
            chat.runtimes.insert(LAB, runtimes);
            let log = chat.logs.entry(LAB).or_default();
            let mut seq = log.head.0;
            for &(root, runtime) in projects {
                seq += 1;
                log.tell(&ChatRecord {
                    seq: ChatSeq(seq),
                    at: UnixMs(seq),
                    event: ChatEvent::Project {
                        project: ProjectId(MessageId(ChatSeq(root))),
                        config: ProjectConfig {
                            place: ProjectPlace::parse("/srv/lab").unwrap(),
                            mode: ProjectMode::One,
                            runtime: RuntimeLabel::parse(runtime).unwrap(),
                        },
                    },
                });
            }
        });
    }

    fn attach(chat: &Entity<ChatSystem>, projects: &[(u64, &str)], cx: &mut App) {
        attach_with(chat, projects, RuntimeCatalogue::host(), cx);
    }

    /// The second host's identity, which the user calls `box`.
    pub(in crate::ui) const BOX: EndpointId = EndpointId([4; 32]);

    /// Attaches `box`, with a `tide` (1) of its own and a `skiff` (2) no
    /// other host has, and a thread (3) in its `tide`.
    pub(in crate::ui) fn attach_box(chat: &Entity<ChatSystem>, cx: &mut App) {
        chat.update(cx, |chat, cx| {
            chat.streams(HostId(1), "box", Some(BOX), &Link::detached(), cx);
            chat.runtimes.insert(BOX, RuntimeCatalogue::host());
            let log = chat.logs.entry(BOX).or_default();
            for record in [
                posted(1, None, "tide"),
                posted(2, None, "skiff"),
                posted(3, Some(1), "Tune the tide"),
            ] {
                log.tell(&record);
            }
            for (seq, root) in [(4, 1), (5, 2)] {
                log.tell(&ChatRecord {
                    seq: ChatSeq(seq),
                    at: UnixMs(seq),
                    event: ChatEvent::Project {
                        project: ProjectId(MessageId(ChatSeq(root))),
                        config: ProjectConfig {
                            place: ProjectPlace::parse("/srv/box").unwrap(),
                            mode: ProjectMode::One,
                            runtime: RuntimeLabel::default(),
                        },
                    },
                });
            }
        });
    }

    pub(in crate::ui) fn init(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::WindowHandle<Frame>, Entity<ChatSystem>, rho_db::RhoDb) {
        let db = rho_db::RhoDb::in_memory();
        let (window, chat) = fixture_on(cx, lab(), db.clone());
        cx.update(|cx| attach(&chat, &[(1, "haiku-max"), (2, "opus-high")], cx));
        (window, chat, db)
    }

    fn view(chat: &Entity<ChatSystem>, cx: &App) -> Entity<IdeaView> {
        chat.read(cx).idea.view.upgrade().expect("the idea is open")
    }

    fn line(editor: &Entity<Editor>, cx: &App) -> String {
        editor.read(cx).text(cx)
    }

    /// What the `on:` line says while nothing is written on it.
    fn hint(view: &Entity<IdeaView>, cx: &mut App) -> Option<String> {
        let runtime = view.read(cx).runtime.clone();
        runtime.update(cx, |line, cx| {
            line.placeholder_text(cx).map(|hint| hint.to_string())
        })
    }

    fn write(chat: &Entity<ChatSystem>, words: &str, cx: &mut App) {
        let text = chat.read(cx).idea.text.clone();
        text.update(cx, |draft, cx| {
            draft.edit([(0..draft.len(), words)], None, cx)
        });
    }

    /// `chat.Write an idea` step 1 from a thread: the idea key reaches chat
    /// from the window, and the idea is addressed to the thread's project
    /// with that project's runtime as the hint.
    #[gpui::test]
    fn the_idea_key_addresses_the_idea_to_the_project_in_view(cx: &mut gpui::TestAppContext) {
        let (window, chat, _db) = init(cx);
        window
            .update(cx, |frame, window, cx| {
                frame.open(thread(3).address(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        window
            .update(cx, |frame, _, cx| {
                assert_eq!(frame.current_address(), &address());
                let view = view(&chat, cx);
                assert_eq!(line(&view.read(cx).address, cx), "tide");
                assert_eq!(line(&view.read(cx).runtime, cx), "");
                assert_eq!(hint(&view, cx).as_deref(), Some("haiku-max"));
            })
            .unwrap();

        // Another project on the `to:` line brings its own runtime hint.
        window
            .update(cx, |_, window, cx| {
                let address = view(&chat, cx).read(cx).address.clone();
                address.update(cx, |address, cx| address.set_text("reef@lab", window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let view = view(&chat, cx);
            assert_eq!(chat.read(cx).idea.recipient, "reef@lab");
            assert_eq!(hint(&view, cx).as_deref(), Some("opus-high"));
        });
    }

    /// `chat.Write an idea`: the `in:` line says the project's mode while
    /// nothing is written on it, and follows `to:`; nothing written sends
    /// no mode, so the project decides. A written word stays across `to:`
    /// changes and goes with the idea; a word that is no mode sends nothing.
    #[gpui::test]
    fn the_in_line_hints_the_projects_mode_and_sends_what_is_written(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, chat, _db) = init(cx);
        // `reef` starts a fresh agent for each idea.
        cx.update(|cx| {
            chat.update(cx, |chat, _| {
                let log = chat.logs.get_mut(&LAB).unwrap();
                let seq = ChatSeq(log.head.0 + 1);
                log.tell(&ChatRecord {
                    seq,
                    at: UnixMs(seq.0),
                    event: ChatEvent::Project {
                        project: ProjectId(MessageId(ChatSeq(2))),
                        config: ProjectConfig {
                            place: ProjectPlace::parse("/srv/lab").unwrap(),
                            mode: ProjectMode::PerIdea,
                            runtime: RuntimeLabel::parse("opus-high").unwrap(),
                        },
                    },
                });
            })
        });
        let mode_hint = |view: &Entity<IdeaView>, cx: &mut App| {
            let mode = view.read(cx).mode.clone();
            mode.update(cx, |line, cx| {
                line.placeholder_text(cx).map(|hint| hint.to_string())
            })
        };
        window
            .update(cx, |frame, window, cx| {
                frame.open(thread(3).address(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        cx.update(|cx| {
            let view = view(&chat, cx);
            assert_eq!(line(&view.read(cx).mode, cx), "");
            assert_eq!(mode_hint(&view, cx).as_deref(), Some("one"));
        });
        let set = |field: fn(&IdeaView) -> &Entity<Editor>,
                   text: &'static str,
                   cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |_, window, cx| {
                    let editor = field(view(&chat, cx).read(cx)).clone();
                    editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
                })
                .unwrap();
        };
        set(|view| &view.address, "reef@lab", cx);
        cx.run_until_parked();
        cx.update(|cx| {
            let view = view(&chat, cx);
            assert_eq!(line(&view.read(cx).mode, cx), "", "a hint is never text");
            assert_eq!(mode_hint(&view, cx).as_deref(), Some("new"));
        });

        // Written, the word stays when `to:` moves on.
        set(|view| &view.mode, "one", cx);
        set(|view| &view.address, "tide", cx);
        set(|view| &view.address, "reef@lab", cx);
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(line(&view(&chat, cx).read(cx).mode, cx), "one");
            write(&chat, "Keep one agent on it", cx);
        });
        cx.dispatch_action(window.into(), SendIdea);
        cx.run_until_parked();

        // Unwritten, the project's own mode holds on the host.
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        set(|view| &view.address, "reef@lab", cx);
        cx.update(|cx| write(&chat, "As the project does", cx));
        cx.dispatch_action(window.into(), SendIdea);
        cx.run_until_parked();

        // A word that is no mode is refused before anything is sent.
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        set(|view| &view.address, "reef@lab", cx);
        set(|view| &view.mode, "maybe", cx);
        cx.update(|cx| write(&chat, "Somewhere", cx));
        cx.dispatch_action(window.into(), SendIdea);
        cx.run_until_parked();

        cx.update(|cx| {
            let chat = chat.read(cx);
            let sent: Vec<_> = chat
                .outbox
                .ideas()
                .map(|(_, _, outgoing)| {
                    (
                        rho_agent_types::content_text(&outgoing.content),
                        outgoing.mode,
                    )
                })
                .collect();
            assert_eq!(
                sent,
                [
                    ("Keep one agent on it".to_owned(), Some(ProjectMode::One)),
                    ("As the project does".to_owned(), None),
                ]
            );
            assert_eq!(chat.idea.text.read(cx).text(), "Somewhere", "kept to fix");
        });
    }

    /// The idea is a screen of its own, so the window's keys act on it and
    /// never on the thread it came from; closing it keeps its words.
    #[gpui::test]
    fn the_idea_screen_takes_the_window_keys_from_the_screen_under_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, chat, _db) = init(cx);
        let under = thread(3).address();
        window
            .update(cx, |frame, window, cx| {
                frame.open(under.clone(), window, cx);
                assert_eq!(frame.card_target(cx), Some(under.clone()));
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        cx.update(|cx| write(&chat, "Half a thought", cx));
        window
            .update(cx, |frame, _, cx| {
                assert_eq!(frame.current_address(), &address());
                // `space` over the idea is about no card: never the
                // thread's done.
                assert_eq!(frame.card_target(cx), None);
            })
            .unwrap();

        // The mode word is the editor's on the words; the lines type.
        window
            .update(cx, |frame, window, cx| {
                assert_eq!(frame.current_surface().mode(window, cx), None);
                let address = view(&chat, cx).read(cx).address.focus_handle(cx);
                window.focus(&address, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |frame, window, cx| {
                assert_eq!(frame.current_surface().mode(window, cx), Some(Mode::Insert));
            })
            .unwrap();

        // `shift-tab` walks the lines, `to:`, `in:`, `on:`, and the words.
        cx.dispatch_action(window.into(), NextLine);
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                let view = view(&chat, cx);
                assert!(view.read(cx).mode.focus_handle(cx).is_focused(window));
            })
            .unwrap();
        cx.dispatch_action(window.into(), NextLine);
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                let view = view(&chat, cx);
                assert!(view.read(cx).runtime.focus_handle(cx).is_focused(window));
            })
            .unwrap();

        // The recent places flip between the idea and what was under it
        // like any screen.
        window
            .update(cx, |frame, window, cx| {
                frame.previous_place(window, cx);
                assert_eq!(frame.current_address(), &under);
                frame.previous_place(window, cx);
                assert_eq!(frame.current_address(), &address());
            })
            .unwrap();
        cx.run_until_parked();

        // `q` closes the idea, not the thread under it, and keeps its words.
        cx.dispatch_action(window.into(), keys::Close);
        cx.run_until_parked();
        window
            .update(cx, |frame, _, cx| {
                assert_eq!(frame.current_address(), &under);
                assert!(frame.surface(&under).is_some());
                assert!(chat.read(cx).idea.view.upgrade().is_none());
                assert_eq!(chat.read(cx).idea.text.read(cx).text(), "Half a thought");
            })
            .unwrap();
    }

    /// Over the idea, the idea key asks `to:` instead of opening it again.
    #[gpui::test]
    fn the_idea_key_over_the_idea_chooses_its_project(cx: &mut gpui::TestAppContext) {
        let (window, chat, _db) = init(cx);
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        window
            .update(cx, |frame, _, _| {
                assert_eq!(frame.current_address(), &address());
                assert!(!frame.is_asking());
            })
            .unwrap();
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        window
            .update(cx, |frame, window, cx| {
                let prompt = frame.minibuffer_mut().expect("the idea key asks to:");
                assert_eq!(prompt.prompt(), "to:");
                prompt.set_input("reef".into(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), keys::MinibufferConfirm);
        cx.run_until_parked();
        cx.update(|cx| {
            let chat = chat.read(cx);
            assert_eq!(chat.idea.recipient, "reef");
            assert_eq!(chat.idea.project, Some(thread(2)));
        });
    }

    /// Step 3: a sent idea is the outbox's at once, so a second can be
    /// written while the first still waits for its host, and nothing is
    /// left on the device to send twice.
    #[gpui::test]
    fn a_second_idea_is_written_while_the_first_waits(cx: &mut gpui::TestAppContext) {
        let (window, chat, db) = init(cx);
        for words in ["Check the tide", "And the moon"] {
            cx.dispatch_action(window.into(), keys::PromptIdea);
            cx.run_until_parked();
            window
                .update(cx, |frame, window, cx| {
                    assert_eq!(frame.current_address(), &address(), "{words}");
                    let view = view(&chat, cx);
                    assert_eq!(line(&view.read(cx).editor, cx), "", "{words}");
                    view.read(cx)
                        .address
                        .clone()
                        .update(cx, |address, cx| address.set_text("reef@lab", window, cx));
                    write(&chat, words, cx);
                })
                .unwrap();
            cx.run_until_parked();
            cx.dispatch_action(window.into(), SendIdea);
            cx.run_until_parked();
            window
                .update(cx, |frame, _, cx| {
                    assert_eq!(frame.current_address(), &Frame::desk_address());
                    let chat = chat.read(cx);
                    assert_eq!(chat.idea.text.read(cx).text(), "");
                    assert_eq!(chat.idea.recipient, "");
                })
                .unwrap();
        }
        cx.update(|cx| {
            let chat = chat.read(cx);
            let sent: Vec<_> = chat.outbox.ideas().collect();
            let words: Vec<_> = sent
                .iter()
                .map(|(_, _, outgoing)| rho_agent_types::content_text(&outgoing.content))
                .collect();
            assert_eq!(words, ["Check the tide", "And the moon"]);
            assert_ne!(sent[0].1, sent[1].1);
            assert!(sent.iter().all(|(host, _, outgoing)| *host == LAB
                && outgoing.parent == Some(MessageId(ChatSeq(2)))));
            chat.cache.flush();
        });
        assert_eq!(crate::cache::Cache::open_on(db).unwrap().load().idea, None);
    }

    /// `chat.Write an idea`: an unsent idea is still on this device after a
    /// restart, with its `to:` and `on:` lines.
    #[gpui::test]
    fn an_unsent_idea_comes_back_after_a_restart(cx: &mut gpui::TestAppContext) {
        let (window, chat, db) = init(cx);
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                let view = view(&chat, cx);
                let (address, runtime) = {
                    let view = view.read(cx);
                    (view.address.clone(), view.runtime.clone())
                };
                address.update(cx, |line, cx| line.set_text("reef@lab", window, cx));
                runtime.update(cx, |line, cx| line.set_text("sol-high", window, cx));
                write(&chat, "First line\nsecond line", cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| chat.read(cx).cache.flush());
        let (window, chat) = fixture_on(cx, lab(), db);
        cx.update(|cx| attach(&chat, &[(1, "haiku-max"), (2, "opus-high")], cx));
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        cx.update(|cx| {
            let view = view(&chat, cx);
            let view = view.read(cx);
            assert_eq!(line(&view.editor, cx), "First line\nsecond line");
            assert_eq!(line(&view.address, cx), "reef@lab");
            assert_eq!(line(&view.runtime, cx), "sol-high");
            // Written, not chosen: the line resolves by name.
            let chat = chat.read(cx);
            assert_eq!(chat.idea.project, None);
            assert_eq!(
                chat.resolved_idea_project().map(|project| project.thread),
                Some(thread(2))
            );
        });
    }

    #[gpui::test]
    fn a_refused_idea_comes_back_only_into_an_empty_draft(cx: &mut gpui::TestAppContext) {
        let (window, chat, _db) = init(cx);
        let refused = || IdeaMail {
            client_id: 3,
            project: thread(1),
            text: "Check the tide".into(),
            runtime: Some(RuntimeLabel::parse("sol-high").unwrap()),
            mode: Some(ProjectMode::PerIdea),
        };
        cx.update(|cx| write(&chat, "Another thought", cx));
        window
            .update(cx, |frame, window, cx| {
                assert!(!write_again(chat.clone(), refused(), frame, window, cx));
                assert_eq!(chat.read(cx).idea.text.read(cx).text(), "Another thought");
                chat.update(cx, |chat, cx| chat.clear_idea(cx));
                assert!(write_again(chat.clone(), refused(), frame, window, cx));
                assert_eq!(frame.current_address(), &address());
                let idea = &chat.read(cx).idea;
                assert_eq!(idea.text.read(cx).text(), "Check the tide");
                assert_eq!(idea.recipient, "tide");
                assert_eq!(idea.runtime.as_deref(), Some("sol-high"));
                let view = view(&chat, cx);
                assert_eq!(line(&view.read(cx).address, cx), "tide");
                assert_eq!(line(&view.read(cx).runtime, cx), "sol-high");
            })
            .unwrap();
    }

    /// A name no project has offers to make it. Making it asks where the
    /// project works, and the idea then goes to it as the `to:` prompt
    /// meant: addressed, or sent when sending asked for the project. With
    /// one host the new project needs no host, typed or not.
    #[gpui::test]
    fn creating_a_project_keeps_the_intent_of_the_address_prompt(cx: &mut gpui::TestAppContext) {
        let (window, chat, _db) = init(cx);
        let text = "Half a thought, not ready";
        for (intent, name, typed) in [
            (Intent::Address, "skiff", "skiff@lab"),
            (Intent::Send, "stern", "stern"),
        ] {
            let new = name.to_owned();
            cx.dispatch_action(window.into(), keys::PromptIdea);
            cx.run_until_parked();
            cx.update(|cx| write(&chat, text, cx));
            window
                .update(cx, |frame, window, cx| {
                    match intent {
                        Intent::Address => prompt(chat.clone(), frame, window, cx),
                        Intent::Send => send(chat.clone(), frame, window, cx),
                    }
                    let prompt = frame.minibuffer_mut().expect("to: is asked");
                    assert_eq!(prompt.prompt(), "to:");
                    prompt.set_input(typed.into(), window, cx);
                })
                .unwrap();
            cx.run_until_parked();
            window
                .update(cx, |frame, _, _| {
                    let row = frame.minibuffer_mut().unwrap().selected_candidate();
                    assert_eq!(
                        row.map(|(row, _)| (row.value, row.description)),
                        Some((new.clone(), NEW_PROJECT.to_owned()))
                    );
                })
                .unwrap();
            cx.dispatch_action(window.into(), keys::MinibufferConfirm);
            cx.run_until_parked();
            window
                .update(cx, |frame, _, cx| {
                    let prompt = frame.minibuffer_mut().expect("making it asks where");
                    assert_eq!(prompt.prompt(), format!("{new} works on:"));
                    assert_eq!(chat.read(cx).idea.text.read(cx).text(), text);
                    assert!(chat.read(cx).outbox.ideas().next().is_none());
                })
                .unwrap();
            cx.dispatch_action(window.into(), keys::MinibufferCancel);
            cx.run_until_parked();

            // The host makes the project; what was waiting for it runs.
            let root = cx.update(|cx| {
                let root = chat.read(cx).host_chat(LAB).unwrap().head.0 + 1;
                let created = when_created(chat.clone(), intent, cx);
                chat.update(cx, |chat, _| {
                    chat.logs
                        .get_mut(&LAB)
                        .unwrap()
                        .tell(&posted(root, None, name));
                    chat.creating_projects.push((thread(root), Some(created)));
                });
                attach(&chat, &[(root, "haiku-max")], cx);
                chat.update(cx, |chat, cx| chat.finish_project_creation(cx));
                root
            });
            cx.run_until_parked();
            window
                .update(cx, |frame, _, cx| {
                    let chat = chat.read(cx);
                    match intent {
                        Intent::Address => {
                            assert_eq!(frame.current_address(), &address());
                            assert_eq!(chat.idea.recipient, new);
                            assert_eq!(chat.idea.text.read(cx).text(), text);
                            assert!(chat.outbox.ideas().next().is_none());
                        }
                        Intent::Send => {
                            assert_ne!(frame.current_address(), &address());
                            assert_eq!(chat.idea.text.read(cx).text(), "");
                            let (_, _, sent) = chat.outbox.ideas().next().expect("sent");
                            assert_eq!(sent.parent, Some(MessageId(ChatSeq(root))));
                        }
                    }
                })
                .unwrap();
            if let Intent::Address = intent {
                window
                    .update(cx, |frame, window, cx| {
                        frame.close(&address(), window, cx);
                        chat.update(cx, |chat, cx| chat.clear_idea(cx));
                    })
                    .unwrap();
            }
        }
    }

    fn on_box(seq: u64) -> ThreadId {
        ThreadId {
            host: BOX,
            message: MessageId(ChatSeq(seq)),
        }
    }

    /// `chat.Project`: a project shows its bare name until a second host
    /// has a project of the same name; then both name their host on the
    /// Desk, in the thread, and in `Places`, while names one host alone
    /// has stay bare.
    #[gpui::test]
    fn only_a_name_two_hosts_share_shows_its_host(cx: &mut gpui::TestAppContext) {
        let (_window, chat, _db) = init(cx);
        let tide_thread = thread(3);
        cx.update(|cx| {
            let chat = chat.read(cx);
            assert_eq!(
                chat.thread_name(tide_thread, cx),
                Some(("Check the clock".into(), "tide".into()))
            );
            assert_eq!(chat.project_of(tide_thread, cx), "tide");
            let desk = chat.project(&tide_thread.address(), cx).unwrap();
            assert_eq!(desk.name, "tide");
        });
        cx.update(|cx| attach_box(&chat, cx));
        cx.update(|cx| {
            let chat = chat.read(cx);
            let mut shown: Vec<_> = chat
                .idea_projects()
                .into_iter()
                .map(|project| project.name)
                .collect();
            shown.sort();
            assert_eq!(shown, ["reef", "skiff", "tide@box", "tide@lab"]);
            // The Desk: a card's context and a row's project.
            assert_eq!(chat.thread_name(tide_thread, cx).unwrap().1, "tide@lab");
            assert_eq!(chat.thread_name(on_box(3), cx).unwrap().1, "tide@box");
            let desk = chat.project(&tide_thread.address(), cx).unwrap();
            assert_eq!(desk.name, "tide@lab");
            // The thread's status line.
            assert_eq!(chat.project_of(on_box(3), cx), "tide@box");
            // `Places`: what it shows, and what finds it unshown.
            let found = chat.find(&rho_dealer::Marks::default(), cx);
            let row = |title: &str| {
                found
                    .iter()
                    .find(|row| row.title == title)
                    .unwrap_or_else(|| panic!("{title} in {found:?}"))
            };
            assert!(row("tide@lab").also.is_empty());
            assert_eq!(row("reef").also, ["reef@lab"]);
            assert_eq!(row("skiff").also, ["skiff@box"]);
            assert_eq!(row("Check the clock").description, "tide@lab");
            assert!(row("Check the clock").also.is_empty());
            // Typed addresses: a bare shared name names neither.
            assert_eq!(chat.project_named("tide"), None);
            assert_eq!(chat.project_named("tide@box"), Some(on_box(1)));
            assert_eq!(chat.project_named("tide@lab"), Some(thread(1)));
            assert_eq!(chat.project_named("reef"), Some(thread(2)));
            assert_eq!(chat.project_named("reef@lab"), Some(thread(2)));
            assert_eq!(chat.project_named("reef@box"), None);
            assert_eq!(chat.project_named("skiff"), Some(on_box(2)));
        });
    }

    /// `retire project`: a retired project leaves `to:` completion and
    /// takes no idea, and its threads stay findable.
    #[gpui::test]
    fn a_retired_project_takes_no_more_ideas(cx: &mut gpui::TestAppContext) {
        let (_window, chat, _db) = init(cx);
        cx.update(|cx| {
            chat.update(cx, |chat, _| {
                let log = chat.logs.get_mut(&LAB).unwrap();
                let seq = ChatSeq(log.head.0 + 1);
                log.tell(&ChatRecord {
                    seq,
                    at: UnixMs(seq.0),
                    event: ChatEvent::Retired {
                        project: ProjectId(MessageId(ChatSeq(1))),
                    },
                });
            });
            let chat = chat.read(cx);
            let shown: Vec<_> = chat
                .idea_projects()
                .into_iter()
                .map(|project| project.name)
                .collect();
            assert_eq!(shown, ["reef"]);
            let found = chat.find(&rho_dealer::Marks::default(), cx);
            assert!(
                found.iter().any(|row| row.title == "Check the clock"),
                "its thread is still found"
            );
        });
    }

    /// `chat.Write an idea`: with two tides, `to: tide` names neither, so
    /// sending asks `to:` with both; `tide@lab`, and a bare name one host
    /// alone has, send at once.
    #[gpui::test]
    fn a_bare_shared_name_asks_which_host(cx: &mut gpui::TestAppContext) {
        let (window, chat, _db) = init(cx);
        cx.update(|cx| attach_box(&chat, cx));
        let write_to = |to: &'static str, words: &'static str, cx: &mut gpui::TestAppContext| {
            cx.dispatch_action(window.into(), keys::PromptIdea);
            cx.run_until_parked();
            window
                .update(cx, |_, window, cx| {
                    let view = view(&chat, cx);
                    let address = view.read(cx).address.clone();
                    address.update(cx, |line, cx| line.set_text(to, window, cx));
                    write(&chat, words, cx);
                })
                .unwrap();
            cx.run_until_parked();
            cx.dispatch_action(window.into(), SendIdea);
            cx.run_until_parked();
        };
        write_to("tide", "Which tide?", cx);
        window
            .update(cx, |frame, _, cx| {
                assert!(chat.read(cx).outbox.ideas().next().is_none());
                let prompt = frame.minibuffer_mut().expect("to: is asked");
                assert_eq!(prompt.prompt(), "to:");
                let first = prompt.selected_candidate().unwrap().0;
                prompt.select_by_delta(1);
                let second = prompt.selected_candidate().unwrap().0;
                prompt.select_by_delta(1);
                let third = prompt.selected_candidate().unwrap().0;
                assert_eq!(
                    [first.value.as_str(), second.value.as_str()],
                    ["tide@box", "tide@lab"]
                );
                assert_eq!(first.description, "project");
                assert_eq!(third, first, "no new project, and nothing else");
                prompt.select_by_delta(1);
            })
            .unwrap();
        cx.dispatch_action(window.into(), keys::MinibufferConfirm);
        cx.run_until_parked();
        write_to("tide@lab", "This tide.", cx);
        write_to("skiff", "The only skiff.", cx);
        cx.update(|cx| {
            let sent: Vec<_> = chat
                .read(cx)
                .outbox
                .ideas()
                .map(|(host, _, outgoing)| {
                    (
                        host,
                        outgoing.parent.unwrap(),
                        rho_agent_types::content_text(&outgoing.content),
                    )
                })
                .collect();
            assert_eq!(
                sent,
                [
                    (LAB, MessageId(ChatSeq(1)), "Which tide?".to_owned()),
                    (LAB, MessageId(ChatSeq(1)), "This tide.".to_owned()),
                    (BOX, MessageId(ChatSeq(2)), "The only skiff.".to_owned()),
                ]
            );
        });
    }

    /// `chat.Start a project`: the `to:` line takes `reef@lab` for the one
    /// `reef` rather than offering to make another. With two hosts a new
    /// name offers one row per host, and the address names the host while
    /// the project is asked about.
    #[gpui::test]
    fn a_new_project_names_its_host_only_among_two(cx: &mut gpui::TestAppContext) {
        let (window, chat, _db) = init(cx);
        let offered = |chat: &Entity<ChatSystem>, input: &str, cx: &App| {
            chat.read(cx)
                .project_choices(input, None)
                .into_iter()
                .map(|(row, choice)| {
                    let new = matches!(choice, ProjectChoice::New(_));
                    (row.value, new)
                })
                .collect::<Vec<_>>()
        };
        cx.update(|cx| {
            assert_eq!(offered(&chat, "reef@lab", cx), [("reef".into(), false)]);
            assert_eq!(offered(&chat, "kelp", cx), [("kelp".into(), true)]);
            assert_eq!(offered(&chat, "kelp@lab", cx), [("kelp".into(), true)]);
        });
        cx.update(|cx| attach_box(&chat, cx));
        cx.update(|cx| {
            assert_eq!(
                offered(&chat, "kelp", cx),
                [("kelp@box".into(), true), ("kelp@lab".into(), true)]
            );
            assert_eq!(offered(&chat, "kelp@box", cx), [("kelp@box".into(), true)]);
            assert_eq!(offered(&chat, "reef@lab", cx), [("reef".into(), false)]);
            assert_eq!(offered(&chat, "reef@box", cx), [("reef@box".into(), true)]);
            assert_eq!(offered(&chat, "tide@box", cx), [("tide@box".into(), false)]);
        });
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        window
            .update(cx, |frame, window, cx| {
                let prompt = frame.minibuffer_mut().expect("to: is asked");
                prompt.set_input("kelp@box".into(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), keys::MinibufferConfirm);
        cx.run_until_parked();
        window
            .update(cx, |frame, _, _| {
                let prompt = frame.minibuffer_mut().expect("making it asks where");
                assert_eq!(prompt.prompt(), "kelp@box works on:");
            })
            .unwrap();
    }

    #[gpui::test]
    fn the_on_line_offers_and_accepts_only_the_hosts_runtimes(cx: &mut gpui::TestAppContext) {
        let mut catalogue = RuntimeCatalogue::host();
        catalogue
            .choices
            .retain(|choice| choice.label.as_str() != "sol-low");
        let packed = senax_encoder::pack(&"nova-high".to_owned()).unwrap();
        let host_only = senax_encoder::unpack::<RuntimeLabel>(&mut packed.as_ref()).unwrap();
        catalogue.choices.push(RuntimeChoice {
            label: host_only.clone(),
            kind: RuntimeKind::Rho,
            usual: false,
        });
        let db = rho_db::RhoDb::in_memory();
        let (window, chat) = fixture_on(cx, lab(), db);
        cx.update(|cx| attach_with(&chat, &[(1, "haiku-max")], catalogue, cx));

        window
            .update(cx, |frame, window, cx| {
                frame.open(thread(3).address(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), keys::PromptIdea);
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                let runtime = view(&chat, cx).read(cx).runtime.focus_handle(cx);
                window.focus(&runtime, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), ChooseRuntime);
        cx.run_until_parked();
        window
            .update(cx, |frame, window, cx| {
                let prompt = frame.minibuffer_mut().expect("on: is asked");
                assert_eq!(prompt.prompt(), "on:");
                assert_eq!(
                    prompt
                        .selected_candidate()
                        .map(|(candidate, _)| (candidate.value, candidate.description)),
                    Some(("haiku-max".into(), "project".into()))
                );
                prompt.set_input("sol-low".into(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |frame, window, cx| {
                let prompt = frame.minibuffer_mut().expect("on: is asked");
                assert_eq!(prompt.selected_candidate(), None);
                prompt.set_input("nova-".into(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |frame, _, _| {
                let prompt = frame.minibuffer_mut().expect("on: is asked");
                assert_eq!(
                    prompt
                        .selected_candidate()
                        .map(|(candidate, _)| candidate.value),
                    Some("nova-high".into())
                );
            })
            .unwrap();
        cx.dispatch_action(window.into(), keys::MinibufferConfirm);
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(chat.read(cx).idea.runtime.as_deref(), Some("nova-high"));
        });

        // A runtime absent from the host's catalogue is refused on send.
        window
            .update(cx, |_, window, cx| {
                let runtime = view(&chat, cx).read(cx).runtime.clone();
                runtime.update(cx, |line, cx| line.set_text("sol-low", window, cx));
                write(&chat, "Test host-only catalogue", cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), SendIdea);
        cx.run_until_parked();
        window
            .update(cx, |frame, _, cx| {
                assert_eq!(frame.current_address(), &address());
                assert!(chat.read(cx).outbox.ideas().next().is_none());
            })
            .unwrap();

        // The host-only runtime is accepted on send.
        window
            .update(cx, |_, window, cx| {
                let runtime = view(&chat, cx).read(cx).runtime.clone();
                runtime.update(cx, |line, cx| line.set_text("nova-high", window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(window.into(), SendIdea);
        cx.run_until_parked();
        cx.update(|cx| {
            let (_, _, sent) = chat.read(cx).outbox.ideas().next().expect("idea queued");
            assert_eq!(sent.runtime, Some(host_only));
        });
    }
}
