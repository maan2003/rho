//! The Slack surface for rho agents: Socket Mode events drive the agent
//! pool, replies post back into the thread.
//!
//! Tokens live in a sealed memfd ([`SecretStore`]) stashed in the systemd fd
//! store, so they survive daemon restarts without touching disk. Each Slack
//! thread maps to one agent, persisted in rho-db (`platform_sessions`), so
//! conversations survive restarts too: first message creates the agent,
//! later messages continue it, and the agent's final answer for each turn is
//! posted back as the threaded reply.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rho_agent::MessageDelivery;
use rho_agent::db::{
    AgentMode, AgentReadTxnExt as _, AgentWriteTxnExt as _, DeepConfig, DeepEffort, Status, TopicId,
};
use rho_agent::pool::AgentPool;
use rho_db::RhoDb;
use tokio::sync::mpsc;

use crate::{
    MessageEvent, SecretStore, SlackApi, SlackConfig, SlackConfigRecord, SlackReadTxnExt as _,
    SlackWriteTxnExt as _, run_connection,
};

/// FDNAME under which the secrets memfd lives in the systemd fd store.
const FD_STORE_NAME: &str = "platform-secrets";
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(2);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);

pub struct SlackManager {
    pool: Arc<AgentPool>,
    db: RhoDb,
    /// The "slack" topic Slack-born agents are created in.
    topic_id: TopicId,
    /// user id → display name, so mention tags and author lines read as
    /// names instead of `U03AB12CD` (filled via `users.info`).
    user_names: tokio::sync::Mutex<HashMap<String, String>>,
    /// In-flight Slack messages keyed by their session agent. A completed
    /// turn removes its matching in-progress reaction without adding a
    /// success reaction.
    in_progress: tokio::sync::Mutex<HashMap<rho_agent::db::AgentId, Vec<SlackReaction>>>,
    secrets: std::sync::Mutex<Option<Arc<SecretStore>>>,
    /// Aborting the previous run loop on secret rotation.
    run_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SlackManager {
    /// Finds (or creates) the "slack" topic up front.
    pub async fn new(pool: Arc<AgentPool>, db: RhoDb) -> Arc<Self> {
        let mut write = db.write().await;
        write.init_slack_tables();
        write.commit();
        let existing = db
            .read()
            .list_topics()
            .into_iter()
            .find(|(_, topic)| topic.name == "slack")
            .map(|(topic_id, _)| topic_id);
        let topic_id = match existing {
            Some(topic_id) => topic_id,
            None => {
                let mut write = db.write().await;
                let topic_id =
                    write.create_topic(rho_core::UnixMs::now(), "slack".to_owned(), Status::Normal);
                write.commit();
                topic_id
            }
        };
        let manager = Arc::new(Self {
            pool,
            db,
            topic_id,
            user_names: tokio::sync::Mutex::new(HashMap::new()),
            in_progress: tokio::sync::Mutex::new(HashMap::new()),
            secrets: std::sync::Mutex::new(None),
            run_task: std::sync::Mutex::new(None),
        });
        manager.start_turn_delivery_loop();
        manager
    }

    /// Reclaim secrets stashed before the last daemon restart and connect
    /// if any were found.
    pub fn resume_from_fd_store(self: &Arc<Self>) {
        match SecretStore::take_from_listen_fds(FD_STORE_NAME) {
            Ok(Some(store)) => {
                tracing::info!("resuming slack connection from fd store");
                self.start(Arc::new(store));
            }
            Ok(None) => {}
            Err(error) => tracing::error!(%error, "reclaiming platform secrets fd"),
        }
    }

    /// Install fresh secrets: seal them into a memfd, stash it in the systemd
    /// fd store for restart survival, and (re)connect.
    pub async fn install_secrets(
        self: &Arc<Self>,
        secrets: BTreeMap<String, String>,
        coordinator_repo: Utf8PathBuf,
    ) -> anyhow::Result<String> {
        if !secrets.contains_key("SLACK_BOT_TOKEN") || !secrets.contains_key("SLACK_APP_TOKEN") {
            anyhow::bail!("both SLACK_BOT_TOKEN and SLACK_APP_TOKEN are required");
        }
        {
            let mut write = self.db.write().await;
            write.set_slack_config(SlackConfigRecord {
                coordinator_repo: coordinator_repo.clone(),
            });
            write.commit();
        }
        let store = SecretStore::create(&secrets).context("sealing platform secrets")?;
        let stashed = store
            .stash_in_fd_store(FD_STORE_NAME)
            .context("stashing platform secrets in the systemd fd store")?;
        self.start(Arc::new(store));
        Ok(if stashed {
            "secrets installed and stashed in the systemd fd store".to_owned()
        } else {
            "secrets installed (no systemd notify socket: they will not survive a daemon restart)"
                .to_owned()
        })
    }

    /// Start or restart Slack from the daemon-owned platform secret store.
    ///
    /// The store may also contain non-Slack platform secrets; Slack only
    /// connects when both expected Slack tokens are present.
    pub fn start_from_store(self: &Arc<Self>, store: Arc<SecretStore>) -> anyhow::Result<()> {
        let secrets = store.read().context("reading platform secrets")?;
        if !secrets.contains_key("SLACK_BOT_TOKEN") || !secrets.contains_key("SLACK_APP_TOKEN") {
            anyhow::bail!("both SLACK_BOT_TOKEN and SLACK_APP_TOKEN are required");
        }
        self.start(store);
        Ok(())
    }

    /// Persist Slack's coordinator repo and start from the daemon-owned
    /// platform secret store.
    pub async fn configure_and_start_from_store(
        self: &Arc<Self>,
        store: Arc<SecretStore>,
        coordinator_repo: Utf8PathBuf,
    ) -> anyhow::Result<()> {
        {
            let mut write = self.db.write().await;
            write.set_slack_config(SlackConfigRecord { coordinator_repo });
            write.commit();
        }
        self.start_from_store(store)
    }

    fn start(self: &Arc<Self>, store: Arc<SecretStore>) {
        *self.secrets.lock().expect("secrets lock") = Some(store);
        let mut task = self.run_task.lock().expect("run task lock");
        if let Some(previous) = task.take() {
            previous.abort();
        }
        let manager = self.clone();
        *task = Some(tokio::spawn(async move {
            if let Err(error) = manager.run_loop().await {
                tracing::error!(%error, "slack connection loop stopped");
            }
        }));
    }

    fn start_turn_delivery_loop(self: &Arc<Self>) {
        let manager = self.clone();
        let mut completed = self.pool.subscribe_completed_turns();
        tokio::spawn(async move {
            loop {
                match completed.recv().await {
                    Ok(report) => manager.deliver_completed_turn(report).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    fn slack_config(&self) -> anyhow::Result<SlackConfig> {
        let store = self
            .secrets
            .lock()
            .expect("secrets lock")
            .clone()
            .context("no platform secrets installed")?;
        let mut secrets = store.read().context("reading platform secrets")?;
        Ok(SlackConfig::new(
            secrets
                .remove("SLACK_BOT_TOKEN")
                .context("SLACK_BOT_TOKEN not among installed secrets")?,
            secrets
                .remove("SLACK_APP_TOKEN")
                .context("SLACK_APP_TOKEN not among installed secrets")?,
        ))
    }

    async fn deliver_completed_turn(&self, report: rho_agent::pool::AgentTurnCompleted) {
        let Some(thread) = self.slack_thread_for_agent(report.agent_id) else {
            return;
        };
        let config = match self.slack_config() {
            Ok(config) => config,
            Err(error) => {
                tracing::debug!(%error, "skipping slack turn delivery without secrets");
                return;
            }
        };
        let api = SlackApi::new(&config.api_base);
        if let Some(reaction) = self.take_in_progress(report.agent_id).await
            && let Err(error) = api
                .reactions_remove(&config.bot_token, &reaction.channel, &reaction.ts, "eyes")
                .await
        {
            tracing::debug!(%error, "removing in-progress reaction");
        }
        let text = if report.final_answer.trim().is_empty() {
            "(the agent finished without a text response)".to_owned()
        } else {
            report.final_answer
        };
        if let Err(error) = api
            .post_message(
                &config.bot_token,
                &thread.channel,
                Some(&thread.thread_ts),
                &crate::mrkdwn::to_mrkdwn(&text),
            )
            .await
        {
            tracing::error!(%error, "posting slack turn completion");
        }
    }

    async fn add_in_progress(&self, agent_id: rho_agent::db::AgentId, event: &MessageEvent) {
        self.in_progress
            .lock()
            .await
            .entry(agent_id)
            .or_default()
            .push(SlackReaction {
                channel: event.channel.clone(),
                ts: event.ts.clone(),
            });
    }

    async fn take_in_progress(&self, agent_id: rho_agent::db::AgentId) -> Option<SlackReaction> {
        let mut in_progress = self.in_progress.lock().await;
        let reactions = in_progress.get_mut(&agent_id)?;
        let reaction = reactions.remove(0);
        if reactions.is_empty() {
            in_progress.remove(&agent_id);
        }
        Some(reaction)
    }

    fn slack_thread_for_agent(&self, agent_id: rho_agent::db::AgentId) -> Option<SlackThread> {
        self.db
            .read()
            .list_slack_sessions()
            .into_iter()
            .find_map(|(session_key, session_agent)| {
                (session_agent == agent_id).then(|| SlackThread::parse(&session_key))?
            })
    }

    /// Reconnect loop: one Socket Mode connection at a time. Routine
    /// refreshes reconnect immediately; failures back off with doubling up
    /// to a minute.
    async fn run_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let config = Arc::new(self.slack_config()?);
        let api = SlackApi::new(&config.api_base);

        // One consumer across reconnects: in-flight turns keep replying
        // while the connection cycles.
        let (tx, mut rx) = mpsc::channel::<MessageEvent>(64);
        {
            let manager = self.clone();
            let api = api.clone();
            let config = config.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    let manager = manager.clone();
                    let api = api.clone();
                    let config = config.clone();
                    tokio::spawn(async move {
                        manager.handle_event(&api, &config, event).await;
                    });
                }
            });
        }

        let mut backoff = RECONNECT_BACKOFF_MIN;
        loop {
            let connection = async {
                let identity = api.auth_test(&config.bot_token).await?;
                run_connection(&api, &config, &identity.user_id, &tx).await
            };
            match connection.await {
                Ok(()) => backoff = RECONNECT_BACKOFF_MIN,
                Err(error) => {
                    tracing::error!(%error, "slack connection failed");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                }
            }
        }
    }

    async fn handle_event(&self, api: &SlackApi, config: &SlackConfig, event: MessageEvent) {
        let session_key = event.session_key();
        let known_session = self.db.read().get_slack_session(&session_key).is_some();
        // Respond to DMs, mentions, and follow-ups in threads we already
        // carry; stay silent on ambient channel chatter.
        if !(event.is_mention || event.channel_type == "im" || known_session) {
            return;
        }
        if let Err(error) = api
            .reactions_add(&config.bot_token, &event.channel, &event.ts, "eyes")
            .await
        {
            tracing::debug!(%error, "adding in-progress reaction");
        }
        let (reply, failed) = match self.run_turn(api, config, &session_key, &event).await {
            Ok(reply) => (reply, false),
            Err(error) => {
                tracing::error!(%error, session_key, "slack turn failed");
                (
                    Some(format!("rho hit an error handling this message: {error:#}")),
                    true,
                )
            }
        };
        if failed || reply.is_some() {
            let _ = api
                .reactions_remove(&config.bot_token, &event.channel, &event.ts, "eyes")
                .await;
        }
        if failed
            && let Err(error) = api
                .reactions_add(&config.bot_token, &event.channel, &event.ts, "x")
                .await
        {
            tracing::debug!(%error, "adding error reaction");
        }
        if let Some(reply) = reply
            && let Err(error) = api
                .post_message(
                    &config.bot_token,
                    &event.channel,
                    Some(event.thread_root()),
                    &crate::mrkdwn::to_mrkdwn(&reply),
                )
                .await
        {
            tracing::error!(%error, "posting slack reply");
        }
    }

    /// One inbound message: find or create the thread's coordinator agent and
    /// enqueue the Slack text. Final answers are posted by the generic
    /// completed-turn subscriber, so returning None suppresses an immediate
    /// reply.
    async fn run_turn(
        &self,
        api: &SlackApi,
        config: &SlackConfig,
        session_key: &str,
        event: &MessageEvent,
    ) -> anyhow::Result<Option<String>> {
        let existing = self.db.read().get_slack_session(session_key);
        let (agent_id, agent, is_new) = match existing {
            Some(agent_id) if self.pool.agent_exists(agent_id) => {
                let (_, agent, _) = self
                    .pool
                    .load(agent_id)
                    .await
                    .context("loading slack session agent")?;
                (agent_id, agent, false)
            }
            _ => {
                let Some(config) = self.db.read().get_slack_config() else {
                    return Ok(Some(
                        "rho slack is not configured with a coordinator repo; run `rho slack init \
                         --dir <coordinator-repo>`"
                            .to_owned(),
                    ));
                };
                let start = rho_agent::StartWorkspace::Create {
                    repo: self.pool.repo(&config.coordinator_repo).await?,
                    parent_revset: "@-".to_owned(),
                };
                let (agent_id, agent) = self
                    .pool
                    .create(
                        self.topic_id,
                        AgentMode::Terra(DeepConfig {
                            effort: DeepEffort::Medium,
                            fast_mode: true,
                            code_mode: true,
                        }),
                        None,
                        start,
                    )
                    .await
                    .context("creating slack session agent")?;
                let mut write = self.db.write().await;
                write.set_slack_session(session_key, agent_id);
                write.commit();
                (agent_id, agent, true)
            }
        };

        // Joining mid-thread: the agent needs to see what was already said.
        let thread_context = if is_new && event.thread_ts.is_some() {
            self.thread_context(api, config, event).await
        } else {
            None
        };
        let text = self
            .inbound_text(api, config, event, is_new, thread_context)
            .await;
        self.add_in_progress(agent_id, event).await;
        agent.send_user_message(text, MessageDelivery::NextRequest);
        Ok(None)
    }
}

struct SlackReaction {
    channel: String,
    ts: String,
}

struct SlackThread {
    channel: String,
    thread_ts: String,
}

impl SlackThread {
    fn parse(session_key: &str) -> Option<Self> {
        let rest = session_key.strip_prefix("slack:")?;
        let (channel, thread_ts) = rest.split_once(':')?;
        Some(Self {
            channel: channel.to_owned(),
            thread_ts: thread_ts.to_owned(),
        })
    }
}

impl SlackManager {
    async fn inbound_text(
        &self,
        api: &SlackApi,
        config: &SlackConfig,
        event: &MessageEvent,
        is_new: bool,
        thread_context: Option<String>,
    ) -> String {
        let user = match &event.user {
            Some(user_id) => self.user_name(api, config, user_id).await,
            None => "unknown user".to_owned(),
        };
        let body = self.humanize_mentions(api, config, &event.text).await;
        let mut text = String::new();
        if let Some(context) = thread_context {
            text.push_str(&context);
        }
        text.push_str(&format!(
            "[slack message from {user} in {}]\n{body}",
            event.channel
        ));
        if is_new {
            text.push_str(
                "\n\n(This conversation comes from a Slack thread; your final \
                 response each turn is posted back to it. You are running in \
                 the configured Slack coordinator repository; do not switch \
                 repositories in-place. For repo-specific work elsewhere, \
                 delegate with spawn_agent using workspace=new and an explicit \
                 repo path. Keep responses concise and self-contained.)",
            );
        }
        text
    }

    /// What the thread already said, for an agent joining mid-thread; None
    /// when there's no usable history (fetch failure degrades to no context).
    async fn thread_context(
        &self,
        api: &SlackApi,
        config: &SlackConfig,
        event: &MessageEvent,
    ) -> Option<String> {
        let replies = match api
            .conversations_replies(&config.bot_token, &event.channel, event.thread_root(), 30)
            .await
        {
            Ok(replies) => replies,
            Err(error) => {
                tracing::debug!(%error, "fetching thread context");
                return None;
            }
        };
        let mut lines = Vec::new();
        for message in replies {
            if message.ts == event.ts || message.text.is_empty() {
                continue;
            }
            let name = match &message.user {
                Some(user_id) => self.user_name(api, config, user_id).await,
                None => "bot".to_owned(),
            };
            let text = self.humanize_mentions(api, config, &message.text).await;
            lines.push(format!("{name}: {text}"));
        }
        if lines.is_empty() {
            return None;
        }
        Some(format!(
            "[earlier messages in this thread]\n{}\n\n",
            lines.join("\n")
        ))
    }

    /// Cached `users.info` lookup; falls back to the raw id on failure.
    async fn user_name(&self, api: &SlackApi, config: &SlackConfig, user_id: &str) -> String {
        if let Some(name) = self.user_names.lock().await.get(user_id) {
            return name.clone();
        }
        match api.users_info(&config.bot_token, user_id).await {
            Ok(name) => {
                self.user_names
                    .lock()
                    .await
                    .insert(user_id.to_owned(), name.clone());
                name
            }
            Err(error) => {
                tracing::debug!(%error, user_id, "resolving slack user name");
                user_id.to_owned()
            }
        }
    }

    /// Replace `<@U…>` mention tags with `@display-name`.
    async fn humanize_mentions(&self, api: &SlackApi, config: &SlackConfig, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("<@") {
            out.push_str(&rest[..start]);
            let tail = &rest[start + 2..];
            match tail.find('>') {
                Some(end) if tail[..end].chars().all(|c| c.is_ascii_alphanumeric()) => {
                    out.push('@');
                    out.push_str(&self.user_name(api, config, &tail[..end]).await);
                    rest = &tail[end + 1..];
                }
                _ => {
                    out.push_str("<@");
                    rest = tail;
                }
            }
        }
        out.push_str(rest);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::SlackThread;

    #[test]
    fn parses_slack_session_key() {
        let thread = SlackThread::parse("slack:C123:1700000000.000001").unwrap();
        assert_eq!(thread.channel, "C123");
        assert_eq!(thread.thread_ts, "1700000000.000001");
        assert!(SlackThread::parse("discord:C123:1").is_none());
        assert!(SlackThread::parse("slack:C123").is_none());
    }
}
