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
use futures_util::StreamExt as _;
use rho_agent::db::{
    AgentMode, AgentReadTxnExt as _, AgentWriteTxnExt as _, DeepConfig, DeepEffort, Status, TopicId,
};
use rho_agent::pool::{AgentPool, RunningAgent};
use rho_agent::{AgentState, AgentStateKind, MessageDelivery};
use rho_core::{ContextBlock, InferenceResponseItem, MessagePhase, text_content};
use rho_db::RhoDb;
use tokio::sync::mpsc;

use crate::{MessageEvent, SecretStore, SlackApi, SlackConfig, run_connection};

/// FDNAME under which the secrets memfd lives in the systemd fd store.
const FD_STORE_NAME: &str = "platform-secrets";
/// Give up on a turn (and apologize on Slack) after this long.
const TURN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
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
    secrets: std::sync::Mutex<Option<Arc<SecretStore>>>,
    /// Aborting the previous run loop on secret rotation.
    run_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SlackManager {
    /// Finds (or creates) the "slack" topic up front.
    pub async fn new(pool: Arc<AgentPool>, db: RhoDb) -> Arc<Self> {
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
        Arc::new(Self {
            pool,
            db,
            topic_id,
            user_names: tokio::sync::Mutex::new(HashMap::new()),
            secrets: std::sync::Mutex::new(None),
            run_task: std::sync::Mutex::new(None),
        })
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
    pub fn install_secrets(
        self: &Arc<Self>,
        secrets: BTreeMap<String, String>,
    ) -> anyhow::Result<String> {
        if !secrets.contains_key("SLACK_BOT_TOKEN") || !secrets.contains_key("SLACK_APP_TOKEN") {
            anyhow::bail!("both SLACK_BOT_TOKEN and SLACK_APP_TOKEN are required");
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
        let known_session = self.db.read().get_platform_session(&session_key).is_some();
        // Respond to DMs, mentions, and follow-ups in threads we already
        // carry; stay silent on ambient channel chatter.
        if !(event.is_mention || event.channel_type == "im" || known_session) {
            return;
        }
        // Turns take minutes; the reactions are the only in-flight feedback.
        // Failures (missing reactions:write scope) must not block the turn.
        if let Err(error) = api
            .reactions_add(&config.bot_token, &event.channel, &event.ts, "eyes")
            .await
        {
            tracing::debug!(%error, "adding in-progress reaction");
        }
        let result = self.run_turn(api, config, &session_key, &event).await;
        let verdict = if result.is_ok() {
            "white_check_mark"
        } else {
            "x"
        };
        let reply = result.unwrap_or_else(|error| {
            tracing::error!(%error, session_key, "slack turn failed");
            Some(format!("rho hit an error handling this message: {error:#}"))
        });
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
        let _ = api
            .reactions_remove(&config.bot_token, &event.channel, &event.ts, "eyes")
            .await;
        if let Err(error) = api
            .reactions_add(&config.bot_token, &event.channel, &event.ts, verdict)
            .await
        {
            tracing::debug!(%error, "adding verdict reaction");
        }
    }

    /// One inbound message: find or create the thread's agent, run a turn,
    /// and return the text to post back (None suppresses the reply).
    async fn run_turn(
        &self,
        api: &SlackApi,
        config: &SlackConfig,
        session_key: &str,
        event: &MessageEvent,
    ) -> anyhow::Result<Option<String>> {
        let existing = self.db.read().get_platform_session(session_key);
        let (agent, is_new) = match existing {
            Some(agent_id) if self.pool.agent_exists(agent_id) => {
                let (_, agent, _) = self
                    .pool
                    .load(agent_id)
                    .await
                    .context("loading slack session agent")?;
                (agent, false)
            }
            _ => {
                let repo = match self.pick_workdir(&event.text) {
                    Ok(repo) => repo,
                    Err(guidance) => return Ok(Some(guidance)),
                };
                let start = rho_agent::StartWorkspace::Create {
                    repo: self.pool.repo(&repo).await?,
                    parent_revset: "@-".to_owned(),
                };
                let (agent_id, agent) = self
                    .pool
                    .create(
                        self.topic_id,
                        AgentMode::Deep(DeepConfig {
                            effort: DeepEffort::Medium,
                            fast_mode: true,
                            code_mode: false,
                        }),
                        None,
                        start,
                    )
                    .await
                    .context("creating slack session agent")?;
                let mut write = self.db.write().await;
                write.set_platform_session(session_key, agent_id);
                write.commit();
                (agent, true)
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
        agent.send_user_message(text, MessageDelivery::NextRequest);
        let state = tokio::time::timeout(TURN_TIMEOUT, wait_for_turn_end(&agent))
            .await
            .map_err(|_| anyhow::anyhow!("agent turn did not finish within {TURN_TIMEOUT:?}"))?;
        let text = last_final_response(&state);
        Ok(Some(if text.trim().is_empty() {
            "(the agent finished without a text response)".to_owned()
        } else {
            text
        }))
    }

    /// The repo a new session's agent works in: the sole registered workdir,
    /// or a leading "@<workdir>" in the first message. `Err` carries user
    /// guidance.
    fn pick_workdir(&self, text: &str) -> Result<camino::Utf8PathBuf, String> {
        let workdirs = self.db.read().list_workdirs();
        match &workdirs[..] {
            [(path, _)] => Ok(path.clone()),
            [] => {
                Err("rho has no registered workdirs; register one in the rho GUI first".to_owned())
            }
            many => {
                let prefixed = text
                    .split_whitespace()
                    .next()
                    .and_then(|first| first.strip_prefix('@'))
                    .and_then(|name| {
                        many.iter()
                            .find(|(_, record)| record.name.eq_ignore_ascii_case(name))
                    });
                match prefixed {
                    Some((path, _)) => Ok(path.clone()),
                    None => {
                        let names: Vec<_> = many
                            .iter()
                            .map(|(_, record)| record.name.as_str())
                            .collect();
                        Err(format!(
                            "start your first message with @<workdir> to pick a repo: {}",
                            names.join(", ")
                        ))
                    }
                }
            }
        }
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
                 response each turn is posted back to it. Keep responses \
                 concise and self-contained.)",
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

/// Mid-turn: a reply is not ready while the agent is in these states.
fn is_working(kind: &AgentStateKind) -> bool {
    matches!(
        kind,
        AgentStateKind::ApiStreaming { .. } | AgentStateKind::ToolCalling { .. }
    )
}

/// Wait until the turn our queued message starts has ended: first let the
/// agent go working, then return the state that left working.
async fn wait_for_turn_end(agent: &RunningAgent) -> AgentState {
    let changes = agent.subscribe();
    futures_util::pin_mut!(changes);
    let mut seen_working = is_working(&agent.state().kind);
    let mut last = agent.state();
    while let Some(state) = changes.next().await {
        let working = is_working(&state.kind);
        if seen_working && !working && state.queued_inputs.is_empty() {
            return state;
        }
        seen_working |= working;
        last = state;
    }
    last
}

/// The last completed assistant answer, preferring final-answer text over
/// commentary (same selection as the voice surface).
fn last_final_response(state: &AgentState) -> String {
    for block in state.blocks.iter().rev() {
        let ContextBlock::InferenceResponse { items, .. } = block.as_ref() else {
            continue;
        };
        let text_of = |wanted_final: bool| {
            items
                .iter()
                .filter_map(|item| match item {
                    InferenceResponseItem::AssistantMessage { content, phase, .. } => {
                        let is_final = *phase == Some(MessagePhase::FinalAnswer);
                        (is_final == wanted_final || !wanted_final).then(|| text_content(content))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = {
            let final_text = text_of(true);
            if final_text.trim().is_empty() {
                text_of(false)
            } else {
                final_text
            }
        };
        if !text.trim().is_empty() {
            return text;
        }
    }
    String::new()
}
