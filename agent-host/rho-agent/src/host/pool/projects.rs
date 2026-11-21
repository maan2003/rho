//! A project chooses its agent here, not on any of its devices.
//! The per-project gate orders starts and setting changes, so two ideas
//! from two devices cannot both start the standing agent.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, anyhow, ensure};
use camino::{Utf8Path, Utf8PathBuf};
use rho_agent_types::{
    Address, ContentPart, Mention, MessageId, ProjectChange, ProjectConfig, ProjectId, ProjectMode,
    ProjectName, ProjectPlace, RuntimeLabel, UnixMs, WorksetKind, one_line,
};
use rho_agents_client::protocol::snippets::{self, Function, Runtime};
use tokio::sync::{Mutex, watch};

use super::AgentPool;
use crate::store::{
    MailRead as _, MailWrite as _, NewMessage, ProjectRead as _, ProjectSettings,
    ProjectWrite as _, Routing, StoreRead as _,
};

#[derive(Default)]
pub(super) struct Projects {
    gates: std::sync::Mutex<HashMap<ProjectId, Arc<Mutex<()>>>>,
    changed: watch::Sender<()>,
}

impl Projects {
    async fn lock(&self, project: ProjectId) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = self
            .gates
            .lock()
            .expect("poison")
            .entry(project)
            .or_default()
            .clone();
        gate.lock_owned().await
    }
}

pub enum ProjectIdeaResult {
    Posted(MessageId),
    RuntimeRefused(String),
}

pub struct ProjectIdea {
    pub client_id: u64,
    pub project: ProjectId,
    pub parent: MessageId,
    pub reply_to: Option<MessageId>,
    pub content: Vec<ContentPart>,
    pub host_label: String,
    pub runtime: Option<RuntimeLabel>,
}

impl AgentPool {
    pub fn watch_projects(&self) -> watch::Receiver<()> {
        self.projects.changed.subscribe()
    }

    /// One-time conversion of the old rule, using the newest agent only
    /// here. A place no running or persisted agent can tell us is left
    /// unconfigured, so a device can import the former answer or the user
    /// can give one. Neither mail nor agent state is rewritten.
    pub(super) async fn init_projects(&self) {
        let read = self.db.read();
        if read.has_project_settings() {
            return;
        }
        let mut latest = HashMap::new();
        let agents = read.agents();
        let repositories: HashMap<_, _> = agents
            .iter()
            .filter_map(|(_, head)| {
                let cwd = head.spec.props.get("cwd")?;
                let repo = head.spec.props.get("repo")?;
                Some(((head.workset, cwd.as_str()), repo.as_str()))
            })
            .collect();
        for (agent, head) in &agents {
            let Some((project, place)) = (|| {
                let origin = head
                    .spec
                    .props
                    .get("origin")?
                    .strip_prefix("chat:")?
                    .parse()
                    .ok()?;
                let project = ProjectId(read.root_of(MessageId(rho_agent_types::ChatSeq(origin)))?);
                if !read.is_project(project) {
                    return None;
                }
                let workset = read.workset(head.workset)?;
                let place = match workset.kind {
                    WorksetKind::Host { cwd } => ProjectPlace::parse(
                        head.spec
                            .props
                            .get("cwd")
                            .map_or(cwd.as_str(), String::as_str),
                    )
                    .ok(),
                    // Relative host paths cannot be reconstructed; preserve
                    // their old workset rather than invent a new target.
                    WorksetKind::Isolated => {
                        let repo =
                            head.spec
                                .props
                                .get("repo")
                                .map(String::as_str)
                                .or_else(|| {
                                    head.spec.props.get("cwd").and_then(|cwd| {
                                        repositories.get(&(head.workset, cwd.as_str())).copied()
                                    })
                                })?;
                        let place = ProjectPlace::parse(repo).ok()?;
                        if place.is_repository() {
                            Some(place)
                        } else if Utf8Path::new(repo).is_absolute() {
                            // A legacy 'repository (url or path)' can be
                            // described losslessly
                            // by a file URL, not as a working directory.
                            let url = url::Url::from_file_path(repo).ok()?;
                            ProjectPlace::parse(url.as_str()).ok()
                        } else {
                            None
                        }
                    }
                }?;
                Some((project, place))
            })() else {
                continue;
            };
            latest
                .entry(project)
                .and_modify(|held: &mut (_, _, _)| {
                    if (head.created_at, *agent) > (held.0, held.1) {
                        *held = (head.created_at, *agent, place.clone());
                    }
                })
                .or_insert((head.created_at, *agent, place));
        }
        drop(read);
        let mut write = self.db.write().await;
        write.init_project_tables();
        for (project, (_, agent, place)) in latest {
            write.set_project_settings(
                project,
                &ProjectSettings {
                    place,
                    routing: Routing::One { agent: Some(agent) },
                },
            );
        }
        write.commit();
    }

    pub async fn create_project(
        &self,
        name: ProjectName,
        config: ProjectConfig,
    ) -> anyhow::Result<ProjectId> {
        let mut write = self.db.write().await;
        let read = self.db.read();
        if let Some(root) = read.project_named(name.as_str()) {
            let id = ProjectId(root);
            ensure!(
                read.project_settings(id)
                    .is_some_and(|held| held.config() == config),
                "project {} already exists",
                name.as_str()
            );
            return Ok(id);
        }
        drop(read);
        let root = write.post(
            UnixMs::now(),
            None,
            NewMessage {
                from: Address::User,
                reply_to: None,
                mentions: Vec::new(),
                content: vec![ContentPart::Text {
                    text: name.as_str().to_owned(),
                }],
            },
            None,
        )?;
        let id = ProjectId(root);
        write.set_project_settings(id, &ProjectSettings::new(config));
        write.commit();
        self.projects.changed.send_replace(());
        Ok(id)
    }

    /// Does not overwrite a host-owned setting with an old device copy.
    pub async fn import_project(&self, id: ProjectId, config: ProjectConfig) -> anyhow::Result<()> {
        let _gate = self.projects.lock(id).await;
        let mut write = self.db.write().await;
        let read = self.db.read();
        ensure!(read.is_project(id), "no such project on this host");
        if read.project_settings(id).is_some() {
            return Ok(());
        }
        write.set_project_settings(id, &ProjectSettings::new(config));
        write.commit();
        self.projects.changed.send_replace(());
        Ok(())
    }

    pub async fn change_project(&self, id: ProjectId, change: ProjectChange) -> anyhow::Result<()> {
        let _gate = self.projects.lock(id).await;
        let mut write = self.db.write().await;
        let read = self.db.read();
        ensure!(read.is_project(id), "no such project on this host");
        let held = read.project_settings(id).map(|settings| settings.config());
        let config = match (change, &held) {
            (ProjectChange::Place(place), Some(held)) => ProjectConfig {
                place,
                mode: held.mode,
            },
            // The legacy workflow had one shared destination. Setting its
            // previously unknown place is the only missing answer.
            (ProjectChange::Place(place), None) => ProjectConfig {
                place,
                mode: ProjectMode::One,
            },
            (ProjectChange::Mode(mode), Some(held)) => ProjectConfig {
                place: held.place.clone(),
                mode,
            },
            (ProjectChange::Mode(_), None) => anyhow::bail!("set where this project works first"),
        };
        if held.as_ref() == Some(&config) {
            return Ok(());
        }
        write.set_project_settings(id, &ProjectSettings::new(config));
        write.commit();
        self.projects.changed.send_replace(());
        Ok(())
    }

    pub async fn post_project_idea(
        self: &Arc<Self>,
        idea: ProjectIdea,
    ) -> anyhow::Result<ProjectIdeaResult> {
        let _gate = self.projects.lock(idea.project).await;
        let (settings, posted, standing) = {
            let read = self.db.read();
            ensure!(
                read.is_project(idea.project),
                "no such project on this host"
            );
            ensure!(
                read.root_of(idea.parent) == Some(idea.project.0),
                "idea is not under this project"
            );
            if let Some(target) = idea.reply_to {
                ensure!(
                    read.message(target).is_some_and(|mail| {
                        mail.from == Address::StartFailed && read.mail_thread(&mail) == idea.parent
                    }),
                    "the mail being retried is not a failed start in this thread"
                );
            }
            let settings = read.project_settings(idea.project);
            let posted = read.posted_by_client(idea.client_id);
            if let Some(message) = posted
                && read.project_start_complete(message)
            {
                return Ok(ProjectIdeaResult::Posted(message));
            }
            let standing = match settings.as_ref().map(|s| &s.routing) {
                Some(Routing::One { agent: Some(agent) }) if posted.is_none() => {
                    read.agent(*agent).map(|head| (*agent, head))
                }
                _ => None,
            };
            (settings, posted, standing)
        };
        if let (Some((agent, head)), Some(runtime)) = (&standing, &idea.runtime)
            && (
                head.spec.runtime.model_name(),
                head.spec.runtime.effort_name(),
            ) != (runtime.model(), runtime.effort())
        {
            // Use the same SetSpec path as the palette; it owns the kind
            // rule. The message isn't logged or delivered before it agrees.
            let changed = self
                .eval(head.workset, snippets::set_runtime(*agent, runtime))
                .await;
            let refusal = match changed {
                Ok(result) => result.error.map(|error| last_error(&error).to_owned()),
                Err(error) => Some(format!("{error:#}")),
            };
            if let Some(refusal) = refusal {
                return Ok(ProjectIdeaResult::RuntimeRefused(refusal));
            }
        }
        let resumed = posted.is_some();
        let standing = standing.map(|(agent, _)| agent);
        let message = if let Some(message) = posted {
            // An earlier attempt posted this before it could start the
            // agent. Don't post it again or send it to a different agent.
            message
        } else {
            let mut write = self.db.write().await;
            let message = write.post(
                UnixMs::now(),
                Some(idea.parent),
                NewMessage {
                    from: Address::User,
                    reply_to: idea.reply_to,
                    // Don't fan a new idea out to inherited members.
                    mentions: standing.into_iter().map(Mention::Direct).collect(),
                    content: idea.content,
                },
                Some(idea.client_id),
            )?;
            if standing.is_some() {
                write.complete_project_start(message);
            }
            write.commit();
            message
        };
        self.user_visited(message);
        self.consider_naming(message);

        if let Some(agent) = standing {
            self.wake_receivers(message, None);
            // Stop unloads a persistent agent rather than deleting it.
            // Delivery brings that same identity back, never a new one.
            if let Err(error) = self.load(agent).await {
                eprintln!("rho-agent: loading project agent {agent:?}: {error:#}");
            }
            return Ok(ProjectIdeaResult::Posted(message));
        }

        // Retrying a refused start gives the new agent the original brief
        // first, then the words of the retry; neither is rewritten.
        let origin = idea.reply_to.map_or(message, |_| idea.parent);
        let result = self
            .start_project_agent(origin, settings.as_ref(), idea.runtime.as_ref(), resumed)
            .await;
        let mut write = self.db.write().await;
        match result {
            Ok(agent) => {
                if origin != message {
                    write.deliver_project_retry(agent, message);
                }
                if let Some(mut settings) = settings
                    && matches!(settings.routing, Routing::One { .. })
                {
                    settings.routing = Routing::One { agent: Some(agent) };
                    write.set_project_settings(idea.project, &settings);
                }
            }
            Err(error) => {
                let place = settings
                    .as_ref()
                    .map_or("no place set", |s| s.place.as_str());
                self.write_start_failure(&mut write, message, format!(
                    "failed · did not start · {place} on {}: {}\n\nreply to retry, or change where it works from the project's palette.",
                    one_line(&idea.host_label), one_line(&format!("{error:#}")),
                ))?;
            }
        }
        write.complete_project_start(message);
        write.commit();
        Ok(ProjectIdeaResult::Posted(message))
    }

    async fn start_project_agent(
        self: &Arc<Self>,
        message: MessageId,
        settings: Option<&ProjectSettings>,
        runtime: Option<&RuntimeLabel>,
        resumed: bool,
    ) -> anyhow::Result<rho_agent_types::AgentId> {
        // A connection or host may go away after the worker created
        // the agent but before the attempt's receipt was committed.
        let origin = format!("chat:{}", message.0.0);
        if resumed
            && let Some((agent, _)) = self
                .db
                .read()
                .agents()
                .into_iter()
                .find(|(_, head)| head.spec.props.get("origin") == Some(&origin))
        {
            return Ok(agent);
        }
        let place = &settings.context("the project does not have a place")?.place;
        let function = Function::default();
        let default = Runtime::default();
        let runtime = runtime.unwrap_or(&default);
        let (kind, code) = if place.is_repository() {
            (
                WorksetKind::Isolated,
                snippets::new_agent_on(
                    &function,
                    Utf8Path::new(place.as_str()),
                    "",
                    runtime,
                    Some(message),
                ),
            )
        } else {
            let path = Utf8Path::new(place.as_str());
            let cwd = match path.strip_prefix("~") {
                Ok(rest) => {
                    let home = dirs::home_dir()
                        .and_then(|dir| Utf8PathBuf::try_from(dir).ok())
                        .context("the host home directory is unavailable")?;
                    home.join(rest)
                }
                Err(_) => path.to_owned(),
            };
            let code = snippets::new_agent_in(&function, &cwd, runtime, Some(message));
            (WorksetKind::Host { cwd }, code)
        };
        let (_, evaluated) = self.create_workset(kind, code).await?;
        evaluated.agent.ok_or_else(|| {
            anyhow!(
                "{}",
                evaluated
                    .error
                    .as_deref()
                    .map(last_error)
                    .unwrap_or("no agent was created")
            )
        })
    }
}

fn last_error(traceback: &str) -> &str {
    traceback
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(traceback)
}
