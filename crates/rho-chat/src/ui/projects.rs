//! Project setup uses the same place and mode prompts from the idea's
//! `to:` chooser and `Commands`. These prompts write to the host; only
//! its mail session can change what every Desk shows.

use std::rc::Rc;

use gpui::{App, Context, Entity, Window};
use rho_agent_types::{
    EndpointId, ProjectChange, ProjectConfig, ProjectId, ProjectMode, ProjectName, ProjectPlace,
    RuntimeCatalogue, RuntimeLabel,
};
use rho_frame::{Candidate, Command, Frame, Point, Spelling};
use rho_window::style::StyleClass;

use super::ChatSystem;
use super::idea::IdeaProject;
use crate::protocol::{ChangeProject, CreateProject};
use crate::{ThreadId, names};

pub static NEW_PROJECT: Spelling = Spelling {
    name: "new project",
    aliases: &[("create project", 9), ("add project", 8), ("project", 5)],
    argument: Some("name or name@host"),
};

pub static SET_PROJECT_PLACE: Spelling = Spelling {
    name: "set project place",
    aliases: &[
        ("works on", 8),
        ("project directory", 7),
        ("project folder", 7),
        ("repo", 4),
    ],
    argument: Some("directory"),
};

pub static SET_PROJECT_MODE: Spelling = Spelling {
    name: "set project mode",
    aliases: &[
        ("agents per idea", 7),
        ("one agent", 6),
        ("project agents", 6),
        ("mode", 6),
    ],
    argument: Some("one or per idea"),
};

pub static SET_PROJECT_RUNTIME: Spelling = Spelling {
    name: "set project runtime",
    aliases: &[
        ("project model", 8),
        ("model", 5),
        ("runtime", 5),
        ("default model", 6),
    ],
    argument: Some("runtime"),
};

pub static RENAME_PROJECT: Spelling = Spelling {
    name: "rename project",
    aliases: &[("project name", 7), ("rename", 4)],
    argument: Some("new name"),
};

pub static RETIRE_PROJECT: Spelling = Spelling {
    name: "retire project",
    aliases: &[
        ("archive project", 8),
        ("close project", 7),
        ("remove project", 6),
        ("delete project", 5),
    ],
    argument: Some("project"),
};

pub type KnownPlaces = Rc<dyn Fn(EndpointId, &App) -> Vec<Candidate>>;

/// A validated project name on the host that will own it.
#[derive(Clone)]
pub(super) struct NewProject {
    pub(super) host: EndpointId,
    pub(super) name: ProjectName,
}

/// What waits for a project being made: the idea that named it.
pub(super) type ProjectCreated = Rc<dyn Fn(IdeaProject, &mut App)>;

impl ChatSystem {
    pub fn set_known_places(&mut self, places: KnownPlaces) {
        self.known_places = places;
    }

    /// What a project being made is called: its name alone while the
    /// window holds one host, else with the host, which the user chose.
    pub(super) fn new_project_address(&self, project: &NewProject) -> String {
        let name = project.name.as_str();
        if self.hosts().len() == 1 {
            name.to_owned()
        } else {
            names::qualified(name, &self.host_name(project.host))
        }
    }

    /// A project's settings, as the host's log says them.
    pub(super) fn project_config(&self, thread: ThreadId) -> Option<ProjectConfig> {
        Some(self.host_chat(thread.host)?.project(thread.message)?.config)
    }

    pub(super) fn project_details(&self, project: ThreadId) -> Option<String> {
        let config = self.project_config(project)?;
        let mode = mode_label(config.mode);
        Some(format!(
            "on {} · {mode} · {}",
            config.place.label(),
            config.runtime
        ))
    }

    /// The runtimes the host offers, once its session has said them.
    pub fn runtime_catalogue(&self, host: EndpointId) -> Option<RuntimeCatalogue> {
        self.runtimes.get(&host).cloned()
    }

    /// The project commands work from anywhere: on the current thread's
    /// project, or on the one the user names. Each takes its new value on
    /// the line (`set project runtime sol-high`), or asks for it.
    pub(super) fn project_commands(&self, point: &Point, cx: &Context<Self>) -> Vec<Command> {
        let this = cx.entity();
        let here = self
            .thread_of(&point.surface)
            .and_then(|thread| self.project_root(thread));
        let here_config = here.and_then(|project| {
            Some((
                self.project_address(project)?,
                self.project_config(project)?,
            ))
        });
        let target = |value: &dyn Fn(&ProjectConfig) -> String| match &here_config {
            Some((address, config)) => format!("{address} · {}", value(config)),
            None => "pick project…".to_owned(),
        };
        let create = this.clone();
        let place = this.clone();
        let mode = this.clone();
        let rename = this.clone();
        let retire = this.clone();
        let here_name = here_config.as_ref().map(|(address, _)| address.clone());
        vec![
            Command::new(&NEW_PROJECT, move |frame, named, window, cx| match named {
                Some(named) => Self::new_project_named(create.clone(), &named, frame, window, cx),
                None => Self::prompt_project_address(create.clone(), frame, window, cx),
            }),
            Command::new(&SET_PROJECT_PLACE, move |frame, typed, window, cx| {
                let this = place.clone();
                Self::for_project(
                    place.clone(),
                    here,
                    Rc::new(move |frame, project, address, window, cx| {
                        let change = this.clone();
                        let accept: PlaceAnswer = Rc::new(move |_, place, _, cx| {
                            change.update(cx, |mail, cx| {
                                mail.change_project(project, ProjectChange::Place(place), None, cx)
                            });
                        });
                        match &typed {
                            Some(typed) => match ProjectPlace::parse(typed) {
                                Ok(place) => accept(frame, place, window, cx),
                                Err(reason) => echo_invalid(frame, &SET_PROJECT_PLACE, reason, cx),
                            },
                            None => {
                                let current = this
                                    .read(cx)
                                    .project_config(project)
                                    .map(|config| config.place);
                                Self::prompt_place(
                                    &this,
                                    project.host,
                                    &address,
                                    current.as_ref(),
                                    accept,
                                    frame,
                                    window,
                                    cx,
                                )
                            }
                        }
                    }),
                    frame,
                    window,
                    cx,
                );
            })
            .target(target(&|config| config.place.label().to_owned())),
            Command::new(&SET_PROJECT_MODE, move |frame, typed, window, cx| {
                let this = mode.clone();
                Self::for_project(
                    mode.clone(),
                    here,
                    Rc::new(move |frame, project, address, window, cx| {
                        let Some(config) = this.read(cx).project_config(project) else {
                            return no_project(frame, &address, cx);
                        };
                        let change = this.clone();
                        let accept: ModeAnswer = Rc::new(move |_, mode, _, cx| {
                            change.update(cx, |mail, cx| {
                                mail.change_project(project, ProjectChange::Mode(mode), None, cx)
                            });
                        });
                        match &typed {
                            Some(typed) => match parse_mode(typed) {
                                Some(mode) => accept(frame, mode, window, cx),
                                None => echo_invalid(
                                    frame,
                                    &SET_PROJECT_MODE,
                                    "choose one or per idea",
                                    cx,
                                ),
                            },
                            None => prompt_mode(&address, config.mode, accept, frame, window, cx),
                        }
                    }),
                    frame,
                    window,
                    cx,
                );
            })
            .target(target(&|config| mode_label(config.mode).to_owned())),
            Command::new(&SET_PROJECT_RUNTIME, move |frame, typed, window, cx| {
                let runtime = this.clone();
                Self::for_project(
                    this.clone(),
                    here,
                    Rc::new(move |frame, project, address, window, cx| {
                        let Some(config) = runtime.read(cx).project_config(project) else {
                            return no_project(frame, &address, cx);
                        };
                        let Some(catalogue) = runtime.read(cx).runtime_catalogue(project.host)
                        else {
                            return host_away(frame, cx);
                        };
                        let change = runtime.clone();
                        let accept: RuntimeAnswer = Rc::new(move |_, runtime, _, cx| {
                            change.update(cx, |mail, cx| {
                                mail.change_project(
                                    project,
                                    ProjectChange::Runtime(runtime),
                                    None,
                                    cx,
                                )
                            });
                        });
                        let current = config.runtime;
                        match &typed {
                            Some(typed) => match catalogue.label(typed.trim()) {
                                Ok(runtime) => accept(frame, runtime, window, cx),
                                Err(reason) => {
                                    echo_invalid(frame, &SET_PROJECT_RUNTIME, &reason, cx)
                                }
                            },
                            None => prompt_runtime(
                                format!("{address} runtime ({current}):"),
                                catalogue,
                                current,
                                "current",
                                accept,
                                frame,
                                window,
                                cx,
                            ),
                        }
                    }),
                    frame,
                    window,
                    cx,
                );
            })
            .target(target(&|config| config.runtime.to_string())),
            Command::new(&RENAME_PROJECT, move |frame, typed, window, cx| {
                let this = rename.clone();
                Self::for_project(
                    rename.clone(),
                    here,
                    Rc::new(move |frame, project, address, window, cx| {
                        let change = this.clone();
                        let was = address.clone();
                        let accept = Rc::new(
                            move |frame: &mut Frame, input: String, cx: &mut Context<Frame>| {
                                match ProjectName::parse(&input) {
                                    Ok(name) => change.update(cx, |mail, cx| {
                                        let done = format!("renamed: {was} → {}", name.as_str());
                                        mail.change_project(
                                            project,
                                            ProjectChange::Name(name),
                                            Some(done),
                                            cx,
                                        )
                                    }),
                                    Err(reason) => echo_invalid(frame, &RENAME_PROJECT, reason, cx),
                                }
                            },
                        );
                        match &typed {
                            Some(typed) => accept(frame, typed.clone(), cx),
                            None => {
                                let current = address.clone();
                                frame.open_prompt(
                                    format!("rename {address} to:"),
                                    Rc::new(|_, _, _| Vec::new()),
                                    Rc::new(move |frame, input, _, cx| accept(frame, input, cx)),
                                    window,
                                    cx,
                                );
                                if let Some(prompt) = frame.minibuffer_mut() {
                                    let (name, _) = names::split(&current);
                                    prompt.set_input(name.to_owned(), window, cx);
                                }
                            }
                        }
                    }),
                    frame,
                    window,
                    cx,
                );
            })
            .target(
                here_name
                    .clone()
                    .unwrap_or_else(|| "pick project…".to_owned()),
            ),
            // Always asked, with the project in view first, so a project
            // is never retired by the screen alone.
            Command::new(&RETIRE_PROJECT, move |frame, typed, window, cx| {
                let this = retire.clone();
                let done = Rc::new(
                    move |frame: &mut Frame, input: &str, cx: &mut Context<Frame>| {
                        let found = {
                            let mail = this.read(cx);
                            mail.project_named(input)
                                .and_then(|project| Some((project, mail.project_address(project)?)))
                        };
                        match found {
                            Some((project, address)) => this.update(cx, |mail, cx| {
                                mail.change_project(
                                    project,
                                    ProjectChange::Retire,
                                    Some(format!("retired: {address}")),
                                    cx,
                                )
                            }),
                            None => frame.echo(
                                "retire project: no such project",
                                StyleClass::SystemInfo,
                                cx,
                            ),
                        }
                    },
                );
                if let Some(typed) = &typed {
                    return done(frame, typed.trim(), cx);
                }
                let names = retire.clone();
                let first = here.and_then(|project| retire.read(cx).project_address(project));
                frame.open_prompt(
                    "retire project:",
                    Rc::new(move |_, needle, cx| {
                        let mail = names.read(cx);
                        let needle = needle.trim().to_lowercase();
                        let mut projects: Vec<_> = mail
                            .idea_projects()
                            .into_iter()
                            .filter(|project| project.contains(&needle))
                            .collect();
                        projects.sort_by_key(|project| first.as_ref() != Some(&project.name));
                        projects
                            .into_iter()
                            .map(|project| Candidate {
                                description: mail
                                    .project_details(project.thread)
                                    .unwrap_or_default(),
                                value: project.name,
                            })
                            .collect()
                    }),
                    Rc::new(move |frame, input, _, cx| done(frame, &input, cx)),
                    window,
                    cx,
                );
            })
            .target(here_name.unwrap_or_else(|| "pick project…".to_owned())),
        ]
    }

    /// Runs `then` on the current thread's project, or asks `project:`
    /// first when the point is in no project.
    fn for_project(
        this: Entity<Self>,
        here: Option<ThreadId>,
        then: ProjectAnswer,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        if let Some(project) = here
            && let Some(address) = this.read(cx).project_address(project)
        {
            return then(frame, project, address, window, cx);
        }
        let names = this.clone();
        frame.open_prompt(
            "project:",
            Rc::new(move |_, needle, cx| {
                let mail = names.read(cx);
                let needle = needle.trim().to_lowercase();
                mail.idea_projects()
                    .into_iter()
                    .filter(|project| project.contains(&needle))
                    .map(|project| Candidate {
                        description: mail.project_details(project.thread).unwrap_or_default(),
                        value: project.name,
                    })
                    .collect()
            }),
            Rc::new(move |frame, input, window, cx| {
                let mail = this.read(cx);
                match mail
                    .project_named(&input)
                    .and_then(|project| Some((project, mail.project_address(project)?)))
                {
                    Some((project, address)) => then(frame, project, address, window, cx),
                    None => frame.echo("project: no such project", StyleClass::SystemInfo, cx),
                }
            }),
            window,
            cx,
        );
    }

    fn prompt_project_address(
        this: Entity<Self>,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        let choices = this.clone();
        frame.open_prompt(
            "new project:",
            Rc::new(move |_, input, cx| {
                let (name, alias) = names::split(input);
                let alias = alias.unwrap_or_default().to_lowercase();
                let Ok(name) = ProjectName::parse(name) else {
                    return Vec::new();
                };
                let mail = choices.read(cx);
                mail.hosts()
                    .into_iter()
                    .filter(|(_, host)| host.to_lowercase().starts_with(&alias))
                    .map(|(host, _)| Candidate {
                        value: mail.new_project_address(&NewProject {
                            host,
                            name: name.clone(),
                        }),
                        description: String::new(),
                    })
                    .collect()
            }),
            Rc::new(move |frame, input, window, cx| {
                Self::new_project_named(this.clone(), &input, frame, window, cx)
            }),
            window,
            cx,
        );
    }

    /// Makes the project `input` names, `name` or `name@host`, asking
    /// where it works and how.
    fn new_project_named(
        this: Entity<Self>,
        input: &str,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        let hosts = this.read(cx).hosts();
        let (name, alias) = names::split(input.trim());
        // With one host, the name alone says where.
        let host = match (alias, hosts.as_slice()) {
            (None, [(host, _)]) => Some(*host),
            (None, _) => {
                frame.echo("new project: enter name@host", StyleClass::SystemInfo, cx);
                return;
            }
            (Some(alias), _) => hosts
                .iter()
                .find(|(_, host)| host.eq_ignore_ascii_case(alias))
                .map(|(host, _)| *host),
        };
        let Some(host) = host else {
            frame.echo("new project: no such host", StyleClass::SystemInfo, cx);
            return;
        };
        let name = match ProjectName::parse(name) {
            Ok(name) => name,
            Err(reason) => return echo_invalid(frame, &NEW_PROJECT, reason, cx),
        };
        Self::prompt_project(
            this.clone(),
            NewProject { host, name },
            None,
            None,
            frame,
            window,
            cx,
        );
    }

    /// Asks where, how and on what the new project works. `runtime` is
    /// the `on:` line of the idea that named it, which `runtime:` offers
    /// first when the host knows it.
    pub(super) fn prompt_project(
        this: Entity<Self>,
        project: NewProject,
        created: Option<ProjectCreated>,
        runtime: Option<&str>,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        let address = this.read(cx).new_project_address(&project);
        let Some(catalogue) = this.read(cx).runtime_catalogue(project.host) else {
            return host_away(frame, cx);
        };
        let written = runtime.and_then(|runtime| catalogue.label(runtime).ok());
        let description = if written.is_some() { "on:" } else { "default" };
        let Some(default) = written.or_else(|| catalogue.default_runtime().cloned()) else {
            return frame.echo(
                "new project: the host offers no runtime",
                StyleClass::SystemInfo,
                cx,
            );
        };
        let created = Rc::new(created);
        Self::prompt_place(
            &this.clone(),
            project.host,
            &address.clone(),
            None,
            Rc::new(move |frame, place, window, cx| {
                let this = this.clone();
                let project = project.clone();
                let created = created.clone();
                let prompt = format!("{address} runtime:");
                let catalogue = catalogue.clone();
                let default = default.clone();
                prompt_mode(
                    &address,
                    ProjectMode::One,
                    Rc::new(move |frame, mode, window, cx| {
                        let this = this.clone();
                        let project = project.clone();
                        let created = created.clone();
                        let place = place.clone();
                        prompt_runtime(
                            prompt.clone(),
                            catalogue.clone(),
                            default.clone(),
                            description,
                            Rc::new(move |_, runtime, _, cx| {
                                this.update(cx, |mail, cx| {
                                    mail.create_project(
                                        project.clone(),
                                        ProjectConfig {
                                            place: place.clone(),
                                            mode,
                                            runtime,
                                        },
                                        created.as_ref().clone(),
                                        cx,
                                    )
                                });
                            }),
                            frame,
                            window,
                            cx,
                        );
                    }),
                    frame,
                    window,
                    cx,
                );
            }),
            frame,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn prompt_place(
        this: &Entity<Self>,
        host: EndpointId,
        address: &str,
        current: Option<&ProjectPlace>,
        accept: PlaceAnswer,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        let places = this.read(cx).known_places.clone();
        frame.open_prompt(
            format!("{address} works on:"),
            Rc::new(move |_, needle, cx| {
                let needle = needle.trim();
                places(host, cx)
                    .into_iter()
                    .filter(|candidate| candidate.value.contains(needle))
                    .collect()
            }),
            Rc::new(
                move |frame, input, window, cx| match ProjectPlace::parse(&input) {
                    Ok(place) => accept(frame, place, window, cx),
                    Err(reason) => frame.echo(reason, StyleClass::SystemInfo, cx),
                },
            ),
            window,
            cx,
        );
        if let Some(prompt) = frame.minibuffer_mut() {
            prompt.complete_only_when_chosen();
            if let Some(current) = current {
                prompt.set_input(current.as_str().to_owned(), window, cx);
            }
        }
    }

    fn create_project(
        &self,
        project: NewProject,
        config: ProjectConfig,
        created: Option<ProjectCreated>,
        cx: &mut Context<Self>,
    ) {
        self.call(
            project.host,
            CreateProject {
                name: project.name,
                config,
            },
            cx,
            move |mail, result, cx| match result {
                Ok(ProjectId(message)) => {
                    mail.creating_projects.push((
                        ThreadId {
                            host: project.host,
                            message,
                        },
                        created,
                    ));
                    mail.finish_project_creation(cx);
                }
                Err(error) => mail.frame.echo(
                    format!("new project: {error:#}"),
                    StyleClass::SystemImportant,
                    cx,
                ),
            },
        );
    }

    /// Asks the project's host for `change`; the echo says `done` once
    /// the host has made it, or why it would not.
    fn change_project(
        &self,
        project: ThreadId,
        change: ProjectChange,
        done: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.call(
            project.host,
            ChangeProject {
                project: ProjectId(project.message),
                change,
            },
            cx,
            move |mail, result, cx| match result {
                Ok(()) => {
                    if let Some(done) = done {
                        mail.frame.echo(done, StyleClass::SystemInfo, cx);
                    }
                }
                Err(error) => mail.frame.echo(
                    format!("project: {error:#}"),
                    StyleClass::SystemImportant,
                    cx,
                ),
            },
        );
    }
}

type PlaceAnswer = Rc<dyn Fn(&mut Frame, ProjectPlace, &mut Window, &mut Context<Frame>)>;
type ModeAnswer = Rc<dyn Fn(&mut Frame, ProjectMode, &mut Window, &mut Context<Frame>)>;

fn mode_label(mode: ProjectMode) -> &'static str {
    match mode {
        ProjectMode::One => "one agent",
        ProjectMode::PerIdea => "per idea",
    }
}

/// A mode typed on the line, by its label or the start of it.
fn parse_mode(typed: &str) -> Option<ProjectMode> {
    let typed = typed.trim();
    (!typed.is_empty())
        .then(|| {
            ProjectMode::ALL
                .into_iter()
                .find(|mode| mode.label().starts_with(typed))
        })
        .flatten()
}

/// The echo for a value on the line the command cannot take.
fn echo_invalid(frame: &mut Frame, command: &Spelling, reason: &str, cx: &mut Context<Frame>) {
    frame.echo(
        &format!("{}: {reason}", command.name),
        StyleClass::SystemInfo,
        cx,
    );
}

fn prompt_mode(
    address: &str,
    current: ProjectMode,
    accept: ModeAnswer,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    frame.open_prompt(
        format!("{address} agents:"),
        Rc::new(move |_, needle, _| {
            let mut modes = ProjectMode::ALL;
            modes.sort_by_key(|mode| *mode != current);
            modes
                .into_iter()
                .filter(|mode| mode.label().starts_with(needle.trim()))
                .map(|mode| Candidate {
                    value: mode.label().into(),
                    description: match mode {
                        ProjectMode::One => "every idea goes to the same agent",
                        ProjectMode::PerIdea => "each idea gets a fresh agent in its own workset",
                    }
                    .into(),
                })
                .collect()
        }),
        Rc::new(move |frame, input, window, cx| {
            if let Some(mode) = ProjectMode::ALL.into_iter().find(|m| m.label() == input) {
                accept(frame, mode, window, cx);
            }
        }),
        window,
        cx,
    );
    if let Some(prompt) = frame.minibuffer_mut() {
        prompt.validate(|input| {
            ProjectMode::ALL
                .into_iter()
                .all(|mode| !mode.label().starts_with(input.trim()))
                .then_some("choose one or per idea")
        });
    }
}

type ProjectAnswer = Rc<dyn Fn(&mut Frame, ThreadId, String, &mut Window, &mut Context<Frame>)>;

fn no_project(frame: &mut Frame, address: &str, cx: &mut Context<Frame>) {
    frame.echo(
        &format!("{address}: no such project on its host"),
        StyleClass::SystemInfo,
        cx,
    );
}

fn host_away(frame: &mut Frame, cx: &mut Context<Frame>) {
    frame.echo(
        "the project's host is not attached",
        StyleClass::SystemInfo,
        cx,
    );
}

type RuntimeAnswer = Rc<dyn Fn(&mut Frame, RuntimeLabel, &mut Window, &mut Context<Frame>)>;

/// Offers the host's runtimes, `first` first, and accepts only what the
/// host offers.
fn prompt_runtime(
    title: String,
    catalogue: RuntimeCatalogue,
    first: RuntimeLabel,
    description: &'static str,
    accept: RuntimeAnswer,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let choices = catalogue.clone();
    frame.open_prompt(
        title,
        Rc::new(move |_, needle, _| runtime_choices(&choices, needle, Some((&first, description)))),
        Rc::new(
            move |frame, input, window, cx| match catalogue.label(&input) {
                Ok(runtime) => accept(frame, runtime, window, cx),
                Err(reason) => frame.echo(&reason, StyleClass::SystemInfo, cx),
            },
        ),
        window,
        cx,
    );
}

/// The host's runtimes that start with what was typed: `first` described,
/// then the usual ones, then every other.
pub(super) fn runtime_choices(
    catalogue: &RuntimeCatalogue,
    needle: &str,
    first: Option<(&RuntimeLabel, &str)>,
) -> Vec<Candidate> {
    let needle = needle.trim().to_ascii_lowercase();
    let mut choices: Vec<_> = catalogue.choices.iter().collect();
    let first_label = first.map(|(label, _)| label);
    choices.sort_by_key(|choice| (first_label != Some(&choice.label), !choice.usual));
    choices
        .into_iter()
        .filter(|choice| choice.label.as_str().starts_with(&needle))
        .map(|choice| Candidate {
            value: choice.label.to_string(),
            description: first
                .filter(|(label, _)| **label == choice.label)
                .map_or("", |(_, description)| description)
                .to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{ChatSeq, MessageId};
    use rho_dealer::Addressed as _;

    use super::*;

    #[gpui::test]
    fn project_place_uses_the_typed_directory_unless_a_completion_is_chosen(
        cx: &mut gpui::TestAppContext,
    ) {
        let (window, mail, _db) =
            super::super::search_bench::fixture(cx, crate::HostChat::default());
        let endpoint = EndpointId([3; 32]);
        cx.update(|cx| {
            mail.update(cx, |mail, _| {
                mail.set_known_places(Rc::new(move |host, _| {
                    assert_eq!(host, endpoint);
                    ["/srv/bench work/repo", "/srv/bench work/tools"]
                        .into_iter()
                        .map(|value| Candidate {
                            value: value.to_owned(),
                            description: "directory on the host".to_owned(),
                        })
                        .collect()
                }))
            });
        });
        let picked = Rc::new(std::cell::RefCell::new(None));
        for (choose, expected) in [(false, "/srv/bench work"), (true, "/srv/bench work/repo")] {
            let result = picked.clone();
            window
                .update(cx, |frame, window, cx| {
                    ChatSystem::prompt_place(
                        &mail,
                        endpoint,
                        "reef@lab",
                        None,
                        Rc::new(move |_, place, _, _| *result.borrow_mut() = Some(place)),
                        frame,
                        window,
                        cx,
                    );
                    frame
                        .minibuffer_mut()
                        .unwrap()
                        .set_input("/srv/bench work".into(), window, cx);
                })
                .unwrap();
            cx.run_until_parked();
            if choose {
                window
                    .update(cx, |frame, _, _| {
                        frame.minibuffer_mut().unwrap().select_by_delta(1)
                    })
                    .unwrap();
            }
            cx.dispatch_action(*window, rho_frame::keys::MinibufferConfirm);
            assert_eq!(picked.borrow_mut().take().unwrap().as_str(), expected);
        }
    }

    /// `project › new` takes a bare name while the window holds one host,
    /// and with two offers the name on each host, so the address says
    /// which host makes it.
    #[gpui::test]
    fn project_new_asks_for_a_host_only_among_two(cx: &mut gpui::TestAppContext) {
        use super::super::idea::tests::{attach_box, init};
        let (window, mail, _db) = init(cx);
        let ask = |typed: &str, cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| {
                    ChatSystem::prompt_project_address(mail.clone(), frame, window, cx);
                    let prompt = frame.minibuffer_mut().unwrap();
                    assert_eq!(prompt.prompt(), "new project:");
                    prompt.set_input(typed.into(), window, cx);
                })
                .unwrap();
            cx.run_until_parked();
            let offered = window
                .update(cx, |frame, _, _| {
                    let prompt = frame.minibuffer_mut().unwrap();
                    let first = prompt.selected_candidate().map(|(row, _)| row.value);
                    prompt.select_by_delta(1);
                    let second = prompt.selected_candidate().map(|(row, _)| row.value);
                    prompt.select_by_delta(-1);
                    [first, second]
                })
                .unwrap();
            cx.dispatch_action(window.into(), rho_frame::keys::MinibufferConfirm);
            cx.run_until_parked();
            let asked = window
                .update(cx, |frame, _, _| {
                    frame
                        .minibuffer_mut()
                        .map(|prompt| prompt.prompt().to_owned())
                })
                .unwrap();
            cx.dispatch_action(window.into(), rho_frame::keys::MinibufferCancel);
            cx.run_until_parked();
            (offered, asked)
        };
        let one = |value: &str| Some(value.to_owned());
        assert_eq!(
            ask("kelp", cx),
            ([one("kelp"), one("kelp")], one("kelp works on:"))
        );
        cx.update(|cx| attach_box(&mail, cx));
        assert_eq!(
            ask("kelp", cx),
            (
                [one("kelp@box"), one("kelp@lab")],
                one("kelp@box works on:")
            )
        );
        assert_eq!(
            ask("kelp@l", cx),
            (
                [one("kelp@lab"), one("kelp@lab")],
                one("kelp@lab works on:")
            )
        );
    }

    /// `Start a project` from an idea: the runtime prompt starts on the
    /// draft's `on:` line when the host knows it, and on the host's default
    /// otherwise.
    #[gpui::test]
    fn a_new_project_starts_on_the_runtime_the_idea_wrote(cx: &mut gpui::TestAppContext) {
        use super::super::idea::tests::init;
        let (window, mail, _db) = init(cx);
        let endpoint = EndpointId([3; 32]);
        let first_runtime = |written: Option<&'static str>, cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |frame, window, cx| {
                    ChatSystem::prompt_project(
                        mail.clone(),
                        NewProject {
                            host: endpoint,
                            name: ProjectName::parse("kelp").unwrap(),
                        },
                        None,
                        written,
                        frame,
                        window,
                        cx,
                    );
                    frame
                        .minibuffer_mut()
                        .unwrap()
                        .set_input("/srv/kelp".into(), window, cx);
                })
                .unwrap();
            cx.run_until_parked();
            // The place, then the mode as offered.
            for _ in 0..2 {
                cx.dispatch_action(window.into(), rho_frame::keys::MinibufferConfirm);
                cx.run_until_parked();
            }
            let first = window
                .update(cx, |frame, _, _| {
                    let prompt = frame.minibuffer_mut().unwrap();
                    assert_eq!(prompt.prompt(), "kelp runtime:");
                    prompt
                        .selected_candidate()
                        .map(|(row, _)| (row.value, row.description))
                })
                .unwrap();
            cx.dispatch_action(window.into(), rho_frame::keys::MinibufferCancel);
            cx.run_until_parked();
            first
        };
        let default = cx.update(|cx| {
            mail.read(cx)
                .runtime_catalogue(endpoint)
                .unwrap()
                .default_runtime()
                .unwrap()
                .to_string()
        });
        assert_ne!(default, "haiku-low", "the case tells the two apart");
        assert_eq!(
            first_runtime(Some("haiku-low"), cx),
            Some(("haiku-low".to_owned(), "on:".to_owned()))
        );
        assert_eq!(
            first_runtime(None, cx),
            Some((default.clone(), "default".to_owned()))
        );
        assert_eq!(
            first_runtime(Some("no-such-runtime"), cx),
            Some((default, "default".to_owned())),
            "a word the host does not know is no runtime"
        );
    }

    /// Every project command is in `Commands`, from Desk and from a
    /// thread alike, by its flat name, and asks for a project where the
    /// screen names none.
    #[gpui::test]
    fn the_project_commands_work_from_anywhere(cx: &mut gpui::TestAppContext) {
        let (_window, mail, _db) =
            super::super::search_bench::fixture(cx, crate::HostChat::default());
        let thread = ThreadId {
            host: EndpointId([3; 32]),
            message: MessageId(ChatSeq(2)),
        };
        for surface in [Frame::desk_address(), thread.address()] {
            let point = Point {
                surface,
                subject: rho_frame::Subject::Own,
                card: None,
            };
            let rows = cx.update(|cx| {
                mail.update(cx, |mail, cx| {
                    mail.project_commands(&point, cx)
                        .iter()
                        .map(|command| (command.name(), command.target_text().map(str::to_owned)))
                        .collect::<Vec<_>>()
                })
            });
            let pick = Some("pick project…".to_owned());
            assert_eq!(
                rows,
                [
                    ("new project", None),
                    ("set project place", pick.clone()),
                    ("set project mode", pick.clone()),
                    ("set project runtime", pick.clone()),
                    ("rename project", pick.clone()),
                    ("retire project", pick.clone()),
                ]
            );
        }
    }

    /// The runtime prompt offers what the host said, not what this build
    /// knows: a runtime only the host has is offered, and the current one
    /// comes first.
    #[test]
    fn the_runtime_prompt_offers_the_hosts_catalogue() {
        let mut catalogue = RuntimeCatalogue::host();
        let newer = rho_agent_types::RuntimeChoice {
            label: catalogue.label("sol-high").unwrap(),
            kind: rho_agent_types::RuntimeKind::Rho,
            usual: false,
        };
        catalogue
            .choices
            .retain(|choice| choice.label.model() != "sol");
        catalogue.choices.push(newer.clone());
        let current = catalogue.label("haiku-low").unwrap();
        let offered = runtime_choices(&catalogue, "", Some((&current, "current")));
        assert_eq!(offered[0].value, "haiku-low");
        assert_eq!(offered[0].description, "current");
        assert!(
            offered
                .iter()
                .any(|candidate| candidate.value == "sol-high")
        );
        assert!(!offered.iter().any(|candidate| candidate.value == "sol-low"));
        assert!(catalogue.label("sol-low").is_err());
        let sol = runtime_choices(&catalogue, "SO", Some((&current, "current")));
        assert_eq!(
            sol.iter()
                .map(|candidate| candidate.value.as_str())
                .filter(|value| value.starts_with("sol-"))
                .collect::<Vec<_>>(),
            ["sol-high"]
        );
    }
}
