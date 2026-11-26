//! A project chooses its agent here, not on any of its devices.
//! The per-project gate orders starts and setting changes, so two ideas
//! from two devices cannot both start the standing agent.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, anyhow, ensure};
use camino::{Utf8Path, Utf8PathBuf};
use rho_agent_types::{
    Address, AgentId, ContentPart, MessageId, ProjectChange, ProjectConfig, ProjectId, ProjectMode,
    ProjectName, ProjectPlace, RuntimeLabel, UnixMs, Voice, WorksetKind, one_line,
};
use rho_agents_client::protocol::snippets::{self, Function};
use tokio::sync::Mutex;

use super::AgentPool;
use crate::store::{
    MailRead as _, MailWrite as _, NewMessage, ProjectRead as _, ProjectWrite as _, StoreRead as _,
};

#[derive(Default)]
pub(super) struct Projects {
    gates: std::sync::Mutex<HashMap<ProjectId, Arc<Mutex<()>>>>,
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
    pub failed_start: Option<MessageId>,
    pub content: Vec<ContentPart>,
    pub host_label: String,
    pub runtime: Option<RuntimeLabel>,
}

impl AgentPool {
    /// A project the user names, at a place, in a mode and on a runtime.
    /// Asking again for the same project with the same settings is the
    /// same project.
    pub async fn create_project(
        &self,
        name: ProjectName,
        config: ProjectConfig,
    ) -> anyhow::Result<ProjectId> {
        let id = self
            .make_project(Address::User, name, config, |held, config| held == config)
            .await?;
        Ok(id)
    }

    /// A project an agent starts a thread in. It works where the agent
    /// works, so it is never without a place; an existing project of that
    /// name is that project.
    pub(super) async fn agent_project(
        &self,
        agent: AgentId,
        name: ProjectName,
    ) -> anyhow::Result<ProjectId> {
        let head = self.db.read().agent(agent).context("agent was deleted")?;
        let props = &head.spec.props;
        let place = props
            .get("repo")
            .or_else(|| props.get("cwd"))
            .context("the agent has no place to give its project")?;
        let runtime = RuntimeLabel::parse(&format!(
            "{}-{}",
            head.spec.runtime.model_name(),
            head.spec.runtime.effort_name()
        ))
        .unwrap_or_default();
        let config = ProjectConfig {
            place: ProjectPlace::parse(place).map_err(|error| anyhow!("{error}"))?,
            mode: ProjectMode::One,
            runtime,
        };
        self.make_project(Address::Agent(agent), name, config, |_, _| true)
            .await
    }

    async fn make_project(
        &self,
        from: Address,
        name: ProjectName,
        config: ProjectConfig,
        same: impl FnOnce(&ProjectConfig, &ProjectConfig) -> bool,
    ) -> anyhow::Result<ProjectId> {
        let mut write = self.db.write().await;
        let read = self.db.read();
        if let Some(root) = read.project_named(name.as_str()) {
            let id = ProjectId(root);
            ensure!(
                read.project(id).is_some_and(|held| same(&held, &config)),
                "project {} already exists",
                name.as_str()
            );
            return Ok(id);
        }
        drop(read);
        let at = UnixMs::now();
        let root = write.post(
            at,
            None,
            NewMessage {
                from,
                voice: Voice::Message,
                content: vec![ContentPart::Text {
                    text: name.as_str().to_owned(),
                }],
            },
            None,
        )?;
        let id = ProjectId(root);
        write.set_project(at, id, config);
        write.commit();
        Ok(id)
    }

    pub async fn change_project(&self, id: ProjectId, change: ProjectChange) -> anyhow::Result<()> {
        let _gate = self.projects.lock(id).await;
        let mut write = self.db.write().await;
        let held = self
            .db
            .read()
            .project(id)
            .context("no such project on this host")?;
        let config = match change {
            // A new place or mode needs a new agent; the runtime is a
            // default for the next new agent, not a change to one running.
            ProjectChange::Place(place) => {
                write.clear_standing_agent(id);
                ProjectConfig {
                    place,
                    ..held.clone()
                }
            }
            ProjectChange::Mode(mode) => {
                if mode != held.mode {
                    write.clear_standing_agent(id);
                }
                ProjectConfig {
                    mode,
                    ..held.clone()
                }
            }
            ProjectChange::Runtime(runtime) => ProjectConfig {
                runtime,
                ..held.clone()
            },
        };
        if config == held {
            return Ok(());
        }
        write.set_project(UnixMs::now(), id, config);
        write.commit();
        Ok(())
    }

    pub async fn post_project_idea(
        self: &Arc<Self>,
        idea: ProjectIdea,
    ) -> anyhow::Result<ProjectIdeaResult> {
        let _gate = self.projects.lock(idea.project).await;
        let (config, posted, standing) = {
            let read = self.db.read();
            let config = read
                .project(idea.project)
                .context("no such project on this host")?;
            ensure!(
                read.root_of(idea.parent) == Some(idea.project.0),
                "idea is not under this project"
            );
            if let Some(target) = idea.failed_start {
                ensure!(
                    read.message(target).is_some_and(|failed| {
                        failed.from == Address::StartFailed
                            && read.mail_thread(&failed) == idea.parent
                    }),
                    "the message being retried is not a failed start in this thread"
                );
            }
            let posted = read.posted_by_client(idea.client_id);
            if let Some(message) = posted
                && read.project_start_complete(message)
            {
                return Ok(ProjectIdeaResult::Posted(message));
            }
            let standing = match config.mode {
                ProjectMode::One if posted.is_none() => read
                    .standing_agent(idea.project)
                    .and_then(|agent| read.agent(agent).map(|head| (agent, head))),
                _ => None,
            };
            (config, posted, standing)
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
            let at = UnixMs::now();
            let message = write.post_idea(
                at,
                idea.parent,
                idea.failed_start,
                idea.content,
                idea.client_id,
            )?;
            if let Some(agent) = standing {
                // The standing agent works in the idea's thread from now
                // on, and reads the idea that started it.
                let thread = if idea.failed_start.is_some() {
                    idea.parent
                } else {
                    message
                };
                write.join(at, thread, agent)?;
                write.deliver(agent, message);
                write.complete_project_start(message);
            }
            write.commit();
            message
        };
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
        let origin = idea.failed_start.map_or(message, |_| idea.parent);
        let result = self
            .start_project_agent(origin, &config, idea.runtime.as_ref(), resumed)
            .await;
        let mut write = self.db.write().await;
        match result {
            Ok(agent) => {
                if origin != message {
                    write.deliver(agent, message);
                }
                if config.mode == ProjectMode::One {
                    write.set_standing_agent(idea.project, agent);
                }
            }
            Err(error) => {
                self.write_start_failure(&mut write, message, format!(
                    "failed · did not start · {} on {}: {}\n\nreply to retry, or change where it works from the project's palette.",
                    config.place.as_str(), one_line(&idea.host_label), one_line(&format!("{error:#}")),
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
        config: &ProjectConfig,
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
        let place = &config.place;
        let function = Function::default();
        let runtime = runtime.unwrap_or(&config.runtime);
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
