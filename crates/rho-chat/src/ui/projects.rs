//! Project setup uses the same place and mode prompts from the idea's
//! `to:` chooser and the palette. These prompts write to the host; only
//! its mail session can change what every Desk shows.

use std::rc::Rc;

use gpui::{App, Context, Entity, Window};
use rho_agent_types::{
    EndpointId, ProjectChange, ProjectConfig, ProjectId, ProjectMode, ProjectName, ProjectPlace,
    RuntimeCatalogue, RuntimeLabel,
};
use rho_frame::{Candidate, Frame, Point, Row, Rows};
use rho_window::style::StyleClass;

use super::ChatSystem;
use super::idea::IdeaProject;
use crate::protocol::{ChangeProject, CreateProject};
use crate::{ThreadId, names};

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
        let mode = match config.mode {
            ProjectMode::One => "one agent",
            ProjectMode::PerIdea => "per idea",
        };
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
    /// project, or on the one the user names.
    pub(super) fn project_rows(&self, point: &Point, cx: &Context<Self>) -> Vec<Row> {
        if self.hosts().is_empty() {
            return Vec::new();
        }
        let this = cx.entity();
        let here = self
            .thread_of(&point.surface)
            .and_then(|thread| self.project_root(thread));
        // Each row has its own key: a menu keeps only the first row on a key.
        let mut rows = Rows::new("project");
        let create = this.clone();
        rows.push(Row::run("n", "new", move |frame, window, cx| {
            Self::prompt_project_address(create.clone(), frame, window, cx);
        }));
        let place = this.clone();
        rows.push(Row::run("w", "works on…", move |frame, window, cx| {
            let this = place.clone();
            Self::for_project(
                place.clone(),
                here,
                Rc::new(move |frame, project, address, window, cx| {
                    let current = this
                        .read(cx)
                        .project_config(project)
                        .map(|config| config.place);
                    let change = this.clone();
                    Self::prompt_place(
                        &this,
                        project.host,
                        &address,
                        current.as_ref(),
                        Rc::new(move |_, place, _, cx| {
                            change.update(cx, |mail, cx| {
                                mail.change_project(project, ProjectChange::Place(place), cx)
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
        }));
        let mode = this.clone();
        rows.push(Row::run("a", "agents…", move |frame, window, cx| {
            let this = mode.clone();
            Self::for_project(
                mode.clone(),
                here,
                Rc::new(move |frame, project, address, window, cx| {
                    let Some(config) = this.read(cx).project_config(project) else {
                        return no_project(frame, &address, cx);
                    };
                    let change = this.clone();
                    prompt_mode(
                        &address,
                        config.mode,
                        Rc::new(move |_, mode, _, cx| {
                            change.update(cx, |mail, cx| {
                                mail.change_project(project, ProjectChange::Mode(mode), cx)
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
        }));
        rows.push(Row::run("r", "runtime…", move |frame, window, cx| {
            let runtime = this.clone();
            Self::for_project(
                this.clone(),
                here,
                Rc::new(move |frame, project, address, window, cx| {
                    let Some(config) = runtime.read(cx).project_config(project) else {
                        return no_project(frame, &address, cx);
                    };
                    let Some(catalogue) = runtime.read(cx).runtime_catalogue(project.host) else {
                        return host_away(frame, cx);
                    };
                    let current = config.runtime;
                    let change = runtime.clone();
                    prompt_runtime(
                        format!("{address} runtime ({current}):"),
                        catalogue,
                        current,
                        "current",
                        Rc::new(move |_, runtime, _, cx| {
                            change.update(cx, |mail, cx| {
                                mail.change_project(project, ProjectChange::Runtime(runtime), cx)
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
        }));
        rows.into_commands()
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
                let hosts = this.read(cx).hosts();
                let (name, alias) = names::split(&input);
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
                    Err(reason) => {
                        frame.echo(reason, StyleClass::SystemInfo, cx);
                        return;
                    }
                };
                Self::prompt_project(
                    this.clone(),
                    NewProject { host, name },
                    None,
                    frame,
                    window,
                    cx,
                );
            }),
            window,
            cx,
        );
    }

    pub(super) fn prompt_project(
        this: Entity<Self>,
        project: NewProject,
        created: Option<ProjectCreated>,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        let address = this.read(cx).new_project_address(&project);
        let Some(catalogue) = this.read(cx).runtime_catalogue(project.host) else {
            return host_away(frame, cx);
        };
        let Some(default) = catalogue.default_runtime().cloned() else {
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
                            "default",
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

    fn change_project(&self, project: ThreadId, change: ProjectChange, cx: &mut Context<Self>) {
        self.call(
            project.host,
            ChangeProject {
                project: ProjectId(project.message),
                change,
            },
            cx,
            |mail, result, cx| {
                if let Err(error) = result {
                    mail.frame.echo(
                        format!("project: {error:#}"),
                        StyleClass::SystemImportant,
                        cx,
                    );
                }
            },
        );
    }
}

type PlaceAnswer = Rc<dyn Fn(&mut Frame, ProjectPlace, &mut Window, &mut Context<Frame>)>;
type ModeAnswer = Rc<dyn Fn(&mut Frame, ProjectMode, &mut Window, &mut Context<Frame>)>;

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

    /// Every project command is in the palette, from Desk and from a
    /// thread alike.
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
            let labels = cx.update(|cx| {
                mail.update(cx, |mail, cx| {
                    mail.project_rows(&point, cx)
                        .iter()
                        .map(|row| match &row.pick {
                            rho_frame::menu::Pick::Run { label, .. } => label.clone(),
                            _ => String::new(),
                        })
                        .collect::<Vec<_>>()
                })
            });
            assert_eq!(
                labels,
                [
                    "project › new",
                    "project › works on…",
                    "project › agents…",
                    "project › runtime…"
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
