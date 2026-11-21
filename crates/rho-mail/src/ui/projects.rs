//! Project setup uses the same place and mode prompts from the Idea
//! address chooser and the Desk. These prompts write to the host; only
//! its mail session can change what every Desk shows.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{App, Context, Entity, Window};
use redb::{TableDefinition, TableHandle as _};
use rho_agent_types::{
    ChatSeq, EndpointId, MessageId, ProjectChange, ProjectConfig, ProjectId, ProjectMode,
    ProjectName, ProjectPlace,
};
use rho_db::{RhoDb, Sen};
use rho_dealer::Addressed as _;
use rho_frame::idea::NewIdeaProject;
use rho_frame::{Candidate, Frame, Point, Row};
use rho_window::style::StyleClass;
use senax_encoder::{Decode, Encode};

use super::{CreatedProject, MailSystem};
use crate::protocol::{ChangeProject, CreateProject, ImportProject};
use crate::{AgentRef, ThreadId};

pub type KnownPlaces = Rc<dyn Fn(EndpointId, &App) -> Vec<Candidate>>;

impl MailSystem {
    pub fn set_known_places(&mut self, places: KnownPlaces) {
        self.known_places = places;
    }

    pub(super) fn project_config(&self, thread: ThreadId) -> Option<&ProjectConfig> {
        self.settings
            .get(&thread.host)?
            .get(&ProjectId(thread.message))
    }

    pub(super) fn project_details(&self, project: ThreadId) -> Option<String> {
        if let Some(config) = self.project_config(project) {
            let mode = match config.mode {
                ProjectMode::One => "one agent",
                ProjectMode::PerIdea => "per idea",
            };
            Some(format!("on {} · {mode}", config.place.label()))
        } else if self.settings.contains_key(&project.host) {
            Some("no place set".into())
        } else {
            None
        }
    }

    pub(super) fn import_legacy_projects(&mut self, endpoint: EndpointId, cx: &mut Context<Self>) {
        let Some(settings) = self.settings.get(&endpoint) else {
            return;
        };
        let Some(log) = self.host_mail(endpoint) else {
            return;
        };
        let pending: Vec<_> = self
            .legacy_places
            .keys()
            .filter(|(host, project)| *host == endpoint && log.is_project(project.0))
            .copied()
            .collect();
        let imports: Vec<_> = pending
            .into_iter()
            .filter_map(|(host, project)| {
                let configured = settings.contains_key(&project);
                let config = self.legacy_places.remove(&(host, project));
                if configured {
                    None
                } else {
                    Some((project, config?))
                }
            })
            .collect();
        for (project, config) in imports {
            self.call(
                endpoint,
                ImportProject {
                    project,
                    config: config.clone(),
                },
                cx,
                move |mail, result, _| {
                    if result.is_err() {
                        // Retry at the next snapshot. The old rows are also
                        // retained on disk if this window closes.
                        mail.legacy_places.insert((endpoint, project), config);
                    }
                },
            );
        }
    }

    pub(super) fn project_rows(&self, point: &Point, cx: &Context<Self>) -> Vec<Row> {
        let mut rows = Vec::new();
        let this = cx.entity();
        let on_agent = point
            .subject
            .as_ref()
            .is_some_and(|subject| subject.kind() == AgentRef::KIND);
        if !self.hosts().is_empty() && !on_agent {
            let this = this.clone();
            rows.push(Row::run("c", "new project", move |frame, window, cx| {
                Self::prompt_project_address(this.clone(), frame, window, cx);
            }));
        }
        let Some(project) = self.thread_of(&point.surface) else {
            return rows;
        };
        let Some(address) = self.project_address(project) else {
            return rows;
        };
        let config = self.project_config(project);
        let place = config.map(|c| c.place.clone());
        let change = this.clone();
        let place_address = address.clone();
        rows.push(Row::run("", "works on…", move |frame, window, cx| {
            let change = change.clone();
            Self::prompt_place(
                &change,
                project.host,
                &place_address,
                place.as_ref(),
                Rc::new({
                    let change = change.clone();
                    move |_, place, _, cx| {
                        change.update(cx, |mail, cx| {
                            mail.change_project(project, ProjectChange::Place(place), cx)
                        });
                    }
                }),
                frame,
                window,
                cx,
            );
        }));
        if let Some(config) = config {
            let mode = config.mode;
            rows.push(Row::run(
                "",
                format!("agents: {}", mode.label()),
                move |frame, window, cx| {
                    let change = this.clone();
                    prompt_mode(
                        &address,
                        mode,
                        Rc::new(move |_, mode, _, cx| {
                            change.update(cx, |mail, cx| {
                                mail.change_project(project, ProjectChange::Mode(mode), cx)
                            });
                        }),
                        frame,
                        window,
                        cx,
                    );
                },
            ));
        }
        rows
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
                let (name, alias) = input.rsplit_once('@').unwrap_or((input, ""));
                let Ok(name) = ProjectName::parse(name) else {
                    return Vec::new();
                };
                choices
                    .read(cx)
                    .hosts()
                    .into_iter()
                    .filter(|(_, host)| host.to_lowercase().starts_with(&alias.to_lowercase()))
                    .map(|(_, host)| Candidate {
                        value: format!("{}@{host}", name.as_str()),
                        description: String::new(),
                    })
                    .collect()
            }),
            Rc::new(move |frame, input, window, cx| {
                let hosts = this.read(cx).hosts();
                let Some((name, alias)) = input.rsplit_once('@') else {
                    frame.echo("new project: enter name@host", StyleClass::SystemInfo, cx);
                    return;
                };
                let Some((host, _)) = hosts
                    .into_iter()
                    .find(|(_, host)| host.eq_ignore_ascii_case(alias))
                else {
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
                    NewIdeaProject { host, name },
                    CreatedProject::Open,
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
        project: NewIdeaProject,
        created: CreatedProject,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        let address = format!(
            "{}@{}",
            project.name.as_str(),
            this.read(cx).host_name(project.host)
        );
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
                prompt_mode(
                    &address,
                    ProjectMode::One,
                    Rc::new(move |_, mode, _, cx| {
                        this.update(cx, |mail, cx| {
                            mail.create_project(
                                project.clone(),
                                ProjectConfig {
                                    place: place.clone(),
                                    mode,
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
        if let Some(current) = current
            && let Some(prompt) = frame.minibuffer_mut()
        {
            prompt.set_input(current.as_str().to_owned(), window, cx);
        }
    }

    fn create_project(
        &self,
        project: NewIdeaProject,
        config: ProjectConfig,
        created: CreatedProject,
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

// The former on-device answers remain on disk as a downgrade copy. Import
// only to an unconfigured host, never use them as live launch settings.
#[derive(Clone, Debug, Decode, Encode)]
enum Answered {
    Repository {
        repo: String,
        #[senax(rename = "rev")]
        _rev: String,
    },
    Directory {
        cwd: String,
    },
}

pub(super) fn legacy_places(db: &RhoDb) -> HashMap<(EndpointId, ProjectId), ProjectConfig> {
    let places: TableDefinition<([u8; 32], u64), Sen<Answered>> =
        TableDefinition::new("gui_channel_place_v1");
    let read = db.read();
    if !read.has_table(places.name()) {
        return HashMap::new();
    }
    read.open_table(places)
        .iter()
        .filter_map(|(key, stored)| {
            let (host, project) = key.value();
            let place = match stored.value().into_owned() {
                Answered::Directory { cwd } => ProjectPlace::parse(&cwd).ok()?,
                Answered::Repository { repo, .. } => {
                    let place = ProjectPlace::parse(&repo).ok()?;
                    if place.is_repository() {
                        place
                    } else {
                        let url = url::Url::from_file_path(repo).ok()?;
                        ProjectPlace::parse(url.as_str()).ok()?
                    }
                }
            };
            Some((
                (EndpointId(host), ProjectId(MessageId(ChatSeq(project)))),
                ProjectConfig {
                    place,
                    mode: ProjectMode::One,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rho_db::SenValue;

    use super::*;

    #[test]
    fn a_pre_project_device_can_still_read_its_old_place_to_import() {
        let db = RhoDb::in_memory();
        // Use the old literal names, including the revision even though an
        // ordinary project starts on the repository's default branch now.
        mod prior {
            #[derive(Clone, Debug, senax_encoder::Encode, senax_encoder::Decode)]
            pub enum Answered {
                Repository { repo: String, rev: String },
                Directory { cwd: String },
            }
        }
        futures::executor::block_on(async {
            let mut write = db.write().await;
            let table: TableDefinition<([u8; 32], u64), Sen<prior::Answered>> =
                TableDefinition::new("gui_channel_place_v1");
            write.open_table(table).insert(
                ([7; 32], 39),
                SenValue::borrowed(&prior::Answered::Repository {
                    repo: "/tmp/old work/tree".into(),
                    rev: "".into(),
                }),
            );
            write.open_table(table).insert(
                ([7; 32], 48),
                SenValue::borrowed(&prior::Answered::Directory {
                    cwd: "/tmp/not-a-clone".into(),
                }),
            );
            write.commit();
        });
        let answers = legacy_places(&db);
        let config = |id| &answers[&(EndpointId([7; 32]), ProjectId(MessageId(ChatSeq(id))))];
        assert_eq!(config(39).place.as_str(), "file:///tmp/old%20work/tree");
        assert_eq!(config(39).mode, ProjectMode::One);
        assert_eq!(config(48).place.as_str(), "/tmp/not-a-clone");
        assert!(!config(48).place.is_repository());
    }
}
