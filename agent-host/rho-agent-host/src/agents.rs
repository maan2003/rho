//! The agents part of the agent host, and the parts about one agent's workset:
//! its terminals and shells. The agents session carries the journal, the
//! live tails, new agents and the quota; requests, terminals and shells are
//! streams of their own.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use rho_agent::db::{AgentReadTxnExt as _, AgentWriteTxnExt as _};
use rho_agent_types::{AgentId, Seq};
use rho_agents_client::protocol::{
    AgentCommand, AgentCostDistribution, ClaudeAccountList, ClaudeAccounts, ClientFrame,
    GlobalUsage, NewAgent, Open, QuotaHistory, QuotaUsage, RecordVisualization, Request,
    ServerFrame, SetAuthAccountEnabled, SetClaudeAccount, Visualization, VisualizationContent,
};
use rho_db::RhoDb;
use rho_rpc::protocol::{Answer, Call, Opened, write_frame};
use rho_shell_view::protocol as shell;
use rho_terminal::protocol as term;
use tokio::sync::{broadcast, mpsc};

use crate::{NEXT_CONNECTION_ID, Services, prepare_image_content, usage};

/// Serves one agents stream, whichever kind its opening frame asked for.
pub(crate) async fn serve<R, W>(
    services: Arc<Services>,
    open: Open,
    reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match open {
        Open::Session => serve_session(services, reader, writer).await,
        Open::Request(request) => serve_call(&services, request, &mut writer).await,
    }
}

/// Serves one terminals stream: a terminal, or a call about them.
pub(crate) async fn serve_terminals<R, W>(
    services: Arc<Services>,
    open: term::Open,
    reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match open {
        term::Open::Terminal {
            agent,
            terminal_id,
            open,
            cols,
            rows,
        } => {
            serve_terminal(
                services,
                reader,
                writer,
                agent,
                terminal_id,
                open,
                cols,
                rows,
            )
            .await
        }
        term::Open::Request(term::Request::TerminalList(call)) => {
            respond(
                &mut writer,
                call,
                |term::TerminalList { agent }| async move {
                    terminal_list(&services, agent.as_deref()).await
                },
            )
            .await
        }
    }
}

/// Serves one shells stream: an attached shell, or a call about them.
pub(crate) async fn serve_shells<R, W>(
    services: Arc<Services>,
    open: shell::Open,
    reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let request = match open {
        shell::Open::Attach { agent } => {
            return serve_shell(services, reader, writer, agent).await;
        }
        shell::Open::Request(request) => request,
    };
    let writer = &mut writer;
    match request {
        shell::Request::ShellStart(call) => {
            respond(writer, call, |shell::ShellStart { agent }| async move {
                shell_start(&services, &agent).await
            })
            .await
        }
        shell::Request::ShellList(call) => {
            respond(writer, call, |shell::ShellList { agent }| async move {
                shell_list(&services, agent.as_deref()).await
            })
            .await
        }
        shell::Request::ShellClose(call) => {
            respond(writer, call, |shell::ShellClose { agent }| async move {
                shell_close(&services, &agent).await
            })
            .await
        }
    }
}

/// One open agents session, counted in [`Services::sessions`] for as long
/// as it lives, however the session ends.
struct Attached(Arc<Services>);

impl Attached {
    fn new(services: &Arc<Services>) -> Self {
        services.sessions.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(services))
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.0.sessions.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How many journal entries travel in one `ServerFrame::Log` while a
/// client is catching up. A cold client's first copy is a whole history,
/// so it goes in pages the connection can interleave.
const LOG_PAGE: usize = 512;

/// How often a session hears the quota when nothing has moved it.
const QUOTA_REFRESH: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// A client's agents session: whose journal this is and how far it runs,
/// then the rows past wherever the client's copy stops, every append after
/// them and the live tails, until the client goes; and beside them the
/// agents created and the quota as it moves. A stream of its own so that a
/// catch-up of thousands of pages queues behind nothing and holds nothing
/// up. A second `Follow` starts the follow again from its `since`.
async fn serve_session<R, W>(
    services: Arc<Services>,
    mut reader: R,
    writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let _attached = Attached::new(&services);
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<ServerFrame>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(frame) = outgoing_rx.recv().await {
            if write_frame(&mut writer, &frame).await.is_err() {
                break;
            }
        }
    });
    // Subscribed before the head is read, so a creation in between is in
    // the head's counter or on the receiver (occasionally both, harmlessly).
    let mut created = services.pool.subscribe_created();
    let (journal_head, agent_counter) = {
        let read = services.db.read();
        (read.journal_head(), read.last_agent_counter())
    };
    let _ = outgoing_tx.send(ServerFrame::JournalHead {
        machine_seed: services.machine_seed,
        journal_head,
        agent_counter,
    });
    // Which accounts agents run on: now, and whenever the inference state
    // moves.
    let auth_task = {
        let services = Arc::clone(&services);
        let outgoing_tx = outgoing_tx.clone();
        let mut state = services.inference.subscribe();
        tokio::spawn(async move {
            loop {
                let _ = state.borrow_and_update();
                let auth = services.auth_state();
                if outgoing_tx.send(ServerFrame::Auth { auth }).is_err() {
                    break;
                }
                if state.changed().await.is_err() {
                    break;
                }
            }
        })
    };
    // Agents made anywhere, by clients or by agents spawning children. The
    // journal carries them whole; this only says which are new. A lagged
    // receiver misses a counter that the next creation brings up to date.
    let created_task = {
        let services = Arc::clone(&services);
        let outgoing_tx = outgoing_tx.clone();
        tokio::spawn(async move {
            loop {
                match created.recv().await {
                    Ok(created) => {
                        let frame = ServerFrame::AgentCreated {
                            agent_id: created.agent_id,
                            agent_counter: services.db.read().last_agent_counter(),
                        };
                        if outgoing_tx.send(frame).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };
    // Now, whenever an observation moves it, and every so often besides:
    // burn and resets move with time alone.
    let quota_task = {
        let services = Arc::clone(&services);
        let outgoing_tx = outgoing_tx.clone();
        let mut changed = services.quota.subscribe();
        tokio::spawn(async move {
            let mut refresh = tokio::time::interval(QUOTA_REFRESH);
            loop {
                tokio::select! {
                    _ = refresh.tick() => {}
                    result = changed.changed() => {
                        if result.is_err() {
                            break;
                        }
                    }
                }
                let summaries = usage::quota_summaries(&services.db, &services.inference);
                if outgoing_tx
                    .send(ServerFrame::QuotaUsage { summaries })
                    .is_err()
                {
                    break;
                }
            }
        })
    };
    // Names this stream's focus in the pool's live set, so it leaves with
    // the stream.
    let stream_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let mut follow: Option<tokio::task::JoinHandle<()>> = None;
    let result = loop {
        let frame =
            match rho_rpc::protocol::read_frame_optional::<_, ClientFrame>(&mut reader).await {
                Ok(Some(frame)) => frame,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
        match frame {
            ClientFrame::Follow { since } => {
                if let Some(previous) = follow.take() {
                    previous.abort();
                }
                follow = Some(spawn_log_follow(
                    Arc::clone(&services),
                    outgoing_tx.clone(),
                    Scope::All { since },
                ));
            }
            ClientFrame::FollowAgents { agent_ids } => {
                if agent_ids.len() > 256 {
                    break Err(anyhow::anyhow!("too many followed agents"));
                }
                if let Some(previous) = follow.take() {
                    previous.abort();
                }
                follow = Some(spawn_log_follow(
                    Arc::clone(&services),
                    outgoing_tx.clone(),
                    Scope::Agents(agent_ids),
                ));
            }
            ClientFrame::Focus { agent_ids } => {
                if agent_ids.len() > 64 {
                    break Err(anyhow::anyhow!("too many focused agents"));
                }
                // Focus is what this client is looking at, nothing more: it
                // never loads an agent. The pool unions it across streams
                // into the live set; a loaded agent in it shows its response.
                services
                    .pool
                    .set_live_wants(stream_id, agent_ids.into_iter().collect())
                    .await;
            }
            ClientFrame::Detail {
                agent_id,
                pos,
                more,
            } => {
                // One answer per position, each naming its own `pos`. A chunk
                // asks once and is answered as many times as it asked for.
                for pos in std::iter::once(pos).chain(more) {
                    let body = agent_detail(&services.db, agent_id, pos);
                    let _ = outgoing_tx.send(ServerFrame::Detail {
                        agent_id,
                        pos,
                        body,
                    });
                }
            }
        }
    };
    if let Some(follow) = follow {
        follow.abort();
    }
    auth_task.abort();
    created_task.abort();
    quota_task.abort();
    services
        .pool
        .set_live_wants(stream_id, HashSet::new())
        .await;
    writer_task.abort();
    result
}

/// One pipe per connection: rows and live statuses go out in one order.
/// Rows are read from the journal, never from the feed: an append only
/// says a row landed, and the connection pages the journal from the last
/// seq it sent. A lagged subscription does the same. Rows the transcript
/// leaves behind (`strip` says nothing) advance the seq without a message.
///
/// Statuses are full snapshots, the latest per agent: an unsent one is
/// replaced rather than queued, so a slow connection never falls behind.
fn spawn_log_follow(
    services: Arc<Services>,
    outgoing_tx: mpsc::UnboundedSender<rho_agents_client::protocol::ServerFrame>,
    scope: Scope,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Both subscribed before the first read, so nothing that lands
        // during it is lost; the seq drops duplicate rows.
        let statuses = services.pool.watch_statuses().await;
        let mut feed = rho_agent::journal::feed(&services.db);
        let (mut sent, agents) = match scope {
            Scope::All { since } => (since, None),
            Scope::Agents(agent_ids) => {
                let Some(head) = send_history(&services.db, &outgoing_tx, &agent_ids).await else {
                    return;
                };
                (head, Some(agent_ids.into_iter().collect::<HashSet<_>>()))
            }
        };
        let wanted = |agent_id: &AgentId| {
            agents
                .as_ref()
                .is_none_or(|agents| agents.contains(agent_id))
        };
        loop {
            // Taken before the rows are read: an agent's status arrives
            // after the rows it reflects commit, so they go out first.
            let changed = statuses.take();
            if !send_journal_from(&services.db, &outgoing_tx, &mut sent, &wanted).await {
                return;
            }
            for (agent_id, status) in changed {
                if !wanted(&agent_id) {
                    continue;
                }
                let live = rho_agents_client::protocol::transcript::Live::Snapshot {
                    state: status.runtime.clone(),
                    response: status.response.clone(),
                    draft: status.draft.clone(),
                };
                if outgoing_tx
                    .send(rho_agents_client::protocol::ServerFrame::Live { agent_id, live })
                    .is_err()
                {
                    return;
                }
            }
            loop {
                tokio::select! {
                    appended = feed.recv() => match appended {
                        Ok(appended) if appended.seq <= sent => continue,
                        Err(broadcast::error::RecvError::Closed) => return,
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => break,
                    },
                    () = statuses.changed() => break,
                }
            }
        }
    })
}

/// What a follow sends: the whole journal from a seq, or a few agents.
enum Scope {
    All { since: Seq },
    Agents(Vec<AgentId>),
}

/// Each agent's whole history, as `Log` pages stamped with the journal
/// head read before it. Returns that head, where the follow goes on from,
/// or `None` when the connection is gone. A row that lands between the
/// head and the read goes out twice, here and in the follow; the reader
/// drops a position it already holds.
async fn send_history(
    db: &RhoDb,
    outgoing_tx: &mpsc::UnboundedSender<rho_agents_client::protocol::ServerFrame>,
    agent_ids: &[AgentId],
) -> Option<Seq> {
    let head = db.read().journal_head();
    for &agent_id in agent_ids {
        let mut from = rho_agent_types::AgentPos(0);
        // An agent this host does not hold has no history to send.
        while let Ok(page) =
            agent_log_page(db, rho_agents_client::protocol::AgentLog { agent_id, from })
        {
            let entries = page
                .rows
                .into_iter()
                .map(
                    |(pos, event)| rho_agents_client::protocol::transcript::LogEntry {
                        seq: head,
                        agent_id,
                        pos,
                        event,
                    },
                )
                .collect::<Vec<_>>();
            if !entries.is_empty()
                && outgoing_tx
                    .send(rho_agents_client::protocol::ServerFrame::Log { entries })
                    .is_err()
            {
                return None;
            }
            let Some(next) = page.next else {
                break;
            };
            from = next;
            tokio::task::yield_now().await;
        }
    }
    Some(head)
}

/// Pages the journal out from after `sent`, moving it as it goes. False
/// when the connection is gone.
async fn send_journal_from(
    db: &RhoDb,
    outgoing_tx: &mpsc::UnboundedSender<rho_agents_client::protocol::ServerFrame>,
    sent: &mut rho_agent_types::Seq,
    wanted: &impl Fn(&AgentId) -> bool,
) -> bool {
    let mut carries = Carries::new();
    loop {
        let page = db.read().journal_since(*sent, LOG_PAGE);
        let Some((last, _, _, _)) = page.last() else {
            return true;
        };
        *sent = *last;
        let entries = page
            .into_iter()
            .filter(|(_, agent_id, _, _)| wanted(agent_id))
            .filter_map(|(seq, agent_id, pos, event)| {
                Some(rho_agents_client::protocol::transcript::LogEntry {
                    seq,
                    agent_id,
                    pos: pos.into(),
                    event: strip_row(db, &mut carries, agent_id, pos, &event)?,
                })
            })
            .collect::<Vec<_>>();
        if !entries.is_empty()
            && outgoing_tx
                .send(rho_agents_client::protocol::ServerFrame::Log { entries })
                .is_err()
        {
            return false;
        }
        // Catching up must never starve the connection's own traffic.
        tokio::task::yield_now().await;
    }
}

/// Each agent's newest visible step as of the rows already read, so a
/// request reads its carry back from the log once per agent, not once per
/// request: that walk is as long as the gap since the last step.
type Carries = HashMap<AgentId, Option<rho_agent::inference::Carry>>;

/// A row as clients read it, or `None` for one they never see.
fn strip_row(
    db: &RhoDb,
    carries: &mut Carries,
    agent_id: AgentId,
    pos: rho_agent::log::AgentEventPos,
    event: &rho_agent::AgentEvent<'_>,
) -> Option<rho_agents_client::protocol::transcript::TranscriptEvent> {
    let mut prior_carry = None;
    match event {
        rho_agent::AgentEvent::Entry(rho_agent::entry::Entry::Step { carry, .. }) => {
            carries.insert(agent_id, Some(carry.clone()));
        }
        rho_agent::AgentEvent::Rewound { .. } => {
            carries.remove(&agent_id);
        }
        rho_agent::AgentEvent::Entry(rho_agent::entry::Entry::RequestSent { .. }) => {
            prior_carry = carries
                .entry(agent_id)
                .or_insert_with(|| db.read().agent_input_carry(agent_id, pos))
                .as_ref();
        }
        _ => {}
    }
    crate::transcript::strip(event, prior_carry)
}

/// Every agent, by its head and its first and last rows: one read each, and
/// none of the log between.
fn agent_directory(db: &RhoDb) -> Vec<rho_agents_client::protocol::DirectoryEntry> {
    let read = db.read();
    let mut carries = Carries::new();
    read.list_agents()
        .into_iter()
        .filter_map(|(agent_id, head)| {
            let first = rho_agent::log::AgentEventPos::new(0);
            let created = strip_row(
                db,
                &mut carries,
                agent_id,
                first,
                &read.agent_event(agent_id, first)?,
            )?;
            let last_active = head
                .next
                .pos
                .checked_sub(1)
                .map(rho_agent::log::AgentEventPos::new)
                .and_then(|last| {
                    strip_row(
                        db,
                        &mut carries,
                        agent_id,
                        last,
                        &read.agent_event(agent_id, last)?,
                    )
                })
                .map_or(created.at(), |event| event.at());
            Some(rho_agents_client::protocol::DirectoryEntry {
                agent_id,
                created,
                role: head.config.role,
                title: head.generated_title,
                last_active,
            })
        })
        .collect()
}

/// One page of an agent's rows, stripped as the journal strips them.
fn agent_log_page(
    db: &RhoDb,
    rho_agents_client::protocol::AgentLog { agent_id, from }: rho_agents_client::protocol::AgentLog,
) -> anyhow::Result<rho_agents_client::protocol::AgentLogPage> {
    let page = db.read().agent_rows(agent_id, from.into(), LOG_PAGE);
    anyhow::ensure!(
        !page.is_empty() || from.0 > 0,
        "no agent {} on this host",
        agent_id.encoded()
    );
    let next = (page.len() == LOG_PAGE)
        .then(|| page.last().map(|(pos, _)| pos.next().into()))
        .flatten();
    let mut carries = Carries::new();
    let rows = page
        .iter()
        .filter_map(|(pos, event)| {
            Some((
                (*pos).into(),
                strip_row(db, &mut carries, agent_id, *pos, event)?,
            ))
        })
        .collect();
    Ok(rho_agents_client::protocol::AgentLogPage { rows, next })
}

/// Answers one call with its handler's reply: the call names the reply's
/// type, so no arm can answer with another call's. `Err` becomes
/// [`Answer::Failed`].
async fn respond<C, W, F>(
    writer: &mut W,
    call: C,
    handle: impl FnOnce(C) -> F,
) -> anyhow::Result<()>
where
    C: Call,
    W: tokio::io::AsyncWrite + Unpin,
    F: Future<Output = anyhow::Result<C::Reply>>,
{
    let reply = handle(call).await;
    write_frame(writer, &Answer::from(reply)).await
}

/// One call stream's call.
async fn serve_call<W>(
    services: &Arc<Services>,
    request: Request,
    writer: &mut W,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match request {
        Request::New(call) => respond(writer, call, |call| new_agent(services, call)).await,
        Request::Command(call) => {
            respond(writer, call, |call| handle_agent_command(services, call)).await
        }
        Request::ClaudeAccounts(call) => {
            respond(writer, call, |ClaudeAccounts| async {
                claude_accounts(&services.db, &services.claude)
            })
            .await
        }
        Request::SetClaudeAccount(call) => {
            respond(writer, call, |SetClaudeAccount { name }| async move {
                // The account has to be there before an agent tries to
                // mount it; a switch to a name with no directory would fail
                // at the next turn of every agent at once.
                services.claude.bootstrap(&name)?;
                let mut write = services.db.write().await;
                write.set_claude_account(&name);
                write.commit();
                claude_accounts(&services.db, &services.claude)
            })
            .await
        }
        Request::AgentLog(call) => {
            respond(writer, call, |call| async {
                agent_log_page(&services.db, call)
            })
            .await
        }
        Request::AgentDirectory(call) => {
            respond(writer, call, |_| async {
                Ok(agent_directory(&services.db))
            })
            .await
        }
        Request::SetAuthAccountEnabled(call) => {
            respond(
                writer,
                call,
                |SetAuthAccountEnabled { name, enabled }| async move {
                    services.set_auth_account_enabled(&name, enabled).await;
                    Ok(())
                },
            )
            .await
        }
        Request::Visualization(call) => {
            respond(writer, call, |Visualization { id }| async move {
                let visualization = services
                    .visualizations
                    .get(&id)
                    .with_context(|| format!("visualization {id} does not exist"))?;
                Ok(VisualizationContent {
                    mime_type: visualization.mime_type,
                    content: visualization.content,
                })
            })
            .await
        }
        Request::RecordVisualization(call) => {
            respond(
                writer,
                call,
                |RecordVisualization { mime_type, content }| {
                    services.visualizations.record(mime_type, content)
                },
            )
            .await
        }
        Request::QuotaUsage(call) => {
            respond(writer, call, |QuotaUsage| async {
                Ok(usage::quota_summaries(&services.db, &services.inference))
            })
            .await
        }
        Request::QuotaHistory(call) => {
            respond(writer, call, |QuotaHistory| async {
                Ok(usage::quota_history(&services.db, &services.inference))
            })
            .await
        }
        Request::GlobalUsage(call) => {
            respond(writer, call, |GlobalUsage { since_ms }| async move {
                services.pool.flush_agent_usage(None).await;
                Ok(usage::global_usage(&services.db, since_ms))
            })
            .await
        }
        Request::AgentCostDistribution(call) => {
            respond(
                writer,
                call,
                |AgentCostDistribution { since_ms }| async move {
                    services.pool.flush_agent_usage(None).await;
                    usage::agent_costs(&services.db, since_ms)
                },
            )
            .await
        }
    }
}

/// Starts a new agent. Sessions hear of it from the pool's creation
/// broadcast; the answer tells this client which one is its own.
async fn new_agent(services: &Arc<Services>, new: NewAgent) -> anyhow::Result<AgentId> {
    let NewAgent {
        role,
        start,
        mut content,
    } = new;
    if let Some(content) = content.as_mut() {
        prepare_image_content(content).await?;
    }
    let (agent_id, agent) = services.create(role, start).await?;
    if let Some(content) = content {
        agent
            .send_user_content_accepted(rho_agent::entry::MessageId::new(), content)
            .await?;
    }
    Ok(agent_id)
}

pub(crate) async fn handle_agent_command(
    services: &Arc<Services>,
    command: AgentCommand,
) -> anyhow::Result<()> {
    match command {
        AgentCommand::Send {
            agent_id,
            mut messages,
        } => {
            for message in &mut messages {
                prepare_image_content(&mut message.content).await?;
            }
            let (_, agent, _) = services.load(agent_id).await?;
            // One at a time, in order: a message fails before any after it
            // is logged, and the client sends them all again.
            for message in messages {
                let mut content = message.content;
                // What Rho has to tell the agent goes ahead of the person's
                // words, once: the loop's head forgets it as soon as the
                // message is accepted, the log when the message's row lands.
                let notice = agent.head().pending_notice;
                if let Some(text) = notice.clone() {
                    content.insert(0, rho_agent_types::ContentPart::Text { text });
                }
                agent
                    .send_user_content_accepted(rho_agent::entry::MessageId(message.id), content)
                    .await?;
                if notice.is_some() {
                    agent.notice_carried();
                }
            }
        }
        AgentCommand::Compact { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.compact();
        }
        AgentCommand::ChangeRole { agent_id, role } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.change_role(role).await?;
        }
        AgentCommand::ChangePromptCacheKey { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.change_prompt_cache_key()?;
        }
        AgentCommand::Cancel { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.cancel();
        }
        AgentCommand::Rewind { agent_id, turns } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.rewind(turns).await?;
        }
        AgentCommand::Continue { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.retry();
        }
        AgentCommand::RestartWorkset { agent_id } => {
            let workset = services
                .pool
                .db()
                .read()
                .get_agent(agent_id)
                .place()
                .workset;
            services.pool.restart_workset(workset, true).await?;
        }
    }
    Ok(())
}

fn claude_accounts(
    db: &RhoDb,
    claude: &rho_claude::accounts::ClaudePaths,
) -> anyhow::Result<ClaudeAccountList> {
    Ok(ClaudeAccountList {
        accounts: claude.list()?,
        current: db.read().claude_account(),
    })
}

/// Attaches a dedicated Comint-style shell stream. The agent host retains the
/// process when this client detaches.
async fn serve_shell<R, W>(
    services: Arc<Services>,
    reader: R,
    mut writer: W,
    agent: String,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let client = match shell_attach(&services, &agent).await {
        Ok(client) => client,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: format!("{error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    write_frame(&mut writer, &Opened::Ready).await?;
    client.relay::<_, _, rho_shell_view::protocol::ShellClientFrame, rho_shell_view::protocol::ShellServerFrame>(reader, writer).await
}

async fn shell_start(services: &Arc<Services>, agent: &str) -> anyhow::Result<()> {
    let agent = services.resolve_display_agent_id(agent).await?;
    let process = services.pool.execution(agent).await?;
    let cwd = services.db.read().get_agent(agent).config.place.cwd;
    process
        .action(rho_agent::WorksetAction::ShellStart {
            agent,
            cwd,
            program: rho_shell_program().into(),
            pager: rho_pager_program().into(),
        })
        .await?;
    Ok(())
}

async fn shell_attach(
    services: &Arc<Services>,
    agent: &str,
) -> anyhow::Result<rho_agent::WorksetClient> {
    let agent = services.resolve_display_agent_id(agent).await?;
    services
        .pool
        .execution(agent)
        .await?
        .attach(rho_agent::WorksetAttach::Shell { agent })
        .await
}

async fn shell_list(
    services: &Arc<Services>,
    agent: Option<&str>,
) -> anyhow::Result<Vec<rho_shell_view::protocol::ShellInfo>> {
    let filter = match agent {
        Some(agent) => Some(services.resolve_display_agent_id(agent).await?.encoded()),
        None => None,
    };
    let mut shells = Vec::new();
    for process in services.pool.executions().await {
        if let rho_agent::WorksetReply::Shells(entries) =
            process.action(rho_agent::WorksetAction::ShellList).await?
        {
            shells.extend(
                entries
                    .into_iter()
                    .filter(|entry| filter.as_ref().is_none_or(|agent| &entry.agent == agent)),
            );
        }
    }
    Ok(shells)
}

async fn shell_close(services: &Arc<Services>, agent: &str) -> anyhow::Result<()> {
    let agent = services.resolve_display_agent_id(agent).await?;
    services
        .pool
        .execution(agent)
        .await?
        .action(rho_agent::WorksetAction::ShellClose { agent })
        .await?;
    Ok(())
}

fn rho_shell_program() -> std::ffi::OsString {
    if let Some(program) = std::env::var_os("RHO_SHELL") {
        return program;
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join("rho-shell");
        if sibling.is_file() {
            return sibling.into_os_string();
        }
    }
    "rho-shell".into()
}

fn rho_pager_program() -> std::ffi::OsString {
    if let Some(program) = std::env::var_os("RHO_PAGER") {
        return program;
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join("rho-pager");
        if sibling.is_file() {
            return sibling.into_os_string();
        }
    }
    "rho-pager".into()
}

/// Serves a stream dedicated to one host-owned terminal: spawns or attaches
/// (per [`TerminalOpen`](rho_terminal::protocol::TerminalOpen)), replies
/// `Opened::Ready`, then pumps
/// [`rho_terminal::protocol`] frames until either side closes. Closing only
/// detaches; the terminal keeps running. A headless create replies and
/// returns without attaching.
#[expect(clippy::too_many_arguments)]
async fn serve_terminal<R, W>(
    services: Arc<Services>,
    reader: R,
    mut writer: W,
    agent: String,
    terminal_id: u64,
    open: rho_terminal::protocol::TerminalOpen,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let create = matches!(open, rho_terminal::protocol::TerminalOpen::Create { .. });
    let attached = terminal_attach(&services, &agent, terminal_id, create, cols, rows).await;
    let client = match attached {
        Ok(attached) => attached,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: format!("{error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    write_frame(&mut writer, &Opened::Ready).await?;
    if matches!(
        open,
        rho_terminal::protocol::TerminalOpen::Create { attach: false }
    ) {
        // Headless create: the terminal keeps running with no clients.
        return Ok(());
    }

    client
        .relay::<_, _, rho_terminal::protocol::TermClientFrame, rho_terminal::protocol::TermServerFrame>(
            reader, writer,
        )
        .await
}

/// Resolve metadata and attach to workset-owned execution; no agent activation.
async fn terminal_attach(
    services: &Arc<Services>,
    agent: &str,
    terminal_id: u64,
    create: bool,
    cols: u16,
    rows: u16,
) -> anyhow::Result<rho_agent::WorksetClient> {
    let agent = services.resolve_display_agent_id(agent).await?;
    let process = services.pool.execution(agent).await?;
    let cwd = services.db.read().get_agent(agent).config.place.cwd;
    let shell = services
        .user_environment
        .get("SHELL")
        .and_then(|shell| shell.to_str())
        .unwrap_or("bash");
    let shell = std::fs::canonicalize(shell)
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| shell.to_owned());
    process
        .attach(rho_agent::WorksetAttach::Terminal {
            agent,
            terminal: terminal_id,
            create,
            cols,
            rows,
            cwd,
            shell,
        })
        .await
}

/// The agent host's terminals, or one agent's.
async fn terminal_list(
    services: &Arc<Services>,
    agent: Option<&str>,
) -> anyhow::Result<Vec<rho_terminal::protocol::TerminalInfo>> {
    let filter = match agent {
        Some(agent) => Some(services.resolve_display_agent_id(agent).await?.encoded()),
        None => None,
    };
    let mut terminals = Vec::new();
    for process in services.pool.executions().await {
        if let rho_agent::WorksetReply::Terminals(entries) = process
            .action(rho_agent::WorksetAction::TerminalList)
            .await?
        {
            terminals.extend(
                entries
                    .into_iter()
                    .filter(|entry| filter.as_ref().is_none_or(|agent| &entry.agent == agent)),
            );
        }
    }
    Ok(terminals)
}

/// a request's tool results whole, or a response as the transcript draws
/// it. Rows a rewind hid still answer; the client asked for one it holds.
fn agent_detail(
    db: &RhoDb,
    agent_id: AgentId,
    pos: rho_agent_types::AgentPos,
) -> rho_agents_client::protocol::transcript::DetailBody {
    use rho_agents_client::protocol::transcript::DetailBody;
    let event = db.read().agent_event(agent_id, pos.into());
    match event {
        Some(rho_agent::AgentEvent::Entry(rho_agent::entry::Entry::RequestSent {
            report, ..
        })) => DetailBody::Results(
            crate::transcript::report_results(
                &report,
                db.read().agent_input_carry(agent_id, pos.into()).as_ref(),
            )
            .into_iter()
            .map(
                |result| rho_agents_client::protocol::transcript::DetailResult {
                    id: result.display_id().to_owned(),
                    status: rho_agents_client::protocol::transcript::ToolStatus::Reported,
                    output: result.text,
                    error: None,
                },
            )
            .collect(),
        ),
        Some(rho_agent::AgentEvent::Entry(rho_agent::entry::Entry::Step { carry, .. })) => {
            DetailBody::Response(crate::transcript::step_items(&carry.display_calls()))
        }
        Some(rho_agent::AgentEvent::Transcript { line, .. }) => match line {
            rho_agent::TranscriptLine::Assistant { calls, .. } => DetailBody::Response(
                calls
                    .into_iter()
                    .map(
                        |call| rho_agents_client::protocol::transcript::Item::ToolCall {
                            id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                            format: rho_agents_client::protocol::transcript::ArgumentsFormat::Json,
                        },
                    )
                    .collect(),
            ),
            rho_agent::TranscriptLine::ToolResults { results } => DetailBody::Results(
                results
                    .iter()
                    .map(|result| {
                        let mut detail = detail_result(result);
                        detail.status =
                            rho_agents_client::protocol::transcript::ToolStatus::Reported;
                        detail
                    })
                    .collect(),
            ),
            rho_agent::TranscriptLine::User { .. }
            | rho_agent::TranscriptLine::Compacted { .. } => DetailBody::Nothing,
        },
        Some(rho_agent::AgentEvent::Failed { partial, .. }) => DetailBody::Response(
            partial
                .items
                .iter()
                .filter_map(|slot| match slot {
                    rho_agent_types::transcript::StreamingContextItemState::Pending(item)
                    | rho_agent_types::transcript::StreamingContextItemState::Finished(item) => {
                        crate::transcript::to_item(item)
                    }
                    rho_agent_types::transcript::StreamingContextItemState::Empty => None,
                })
                .collect(),
        ),
        _ => DetailBody::Nothing,
    }
}

fn detail_result(
    result: &rho_agent_types::transcript::ToolResult,
) -> rho_agents_client::protocol::transcript::DetailResult {
    use rho_agents_client::protocol::transcript::ToolStatus;
    rho_agents_client::protocol::transcript::DetailResult {
        id: result.call_id.as_str().to_owned(),
        status: ToolStatus::Reported,
        output: result.body.recorded_output().to_owned(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rho_agent::AgentEvent;
    use rho_agent::db::AgentWriteTxnExt as _;
    use rho_agent::entry::{Entry, Report, RequestNotice};
    use rho_agent_types::{Seq, UnixMs};
    use rho_agents_client::protocol::ServerFrame;
    use rho_agents_client::protocol::transcript::TranscriptEvent;

    use super::{agent_directory, detail_result, send_history, send_journal_from};

    /// A follow of one agent sends that agent's history at the head, then
    /// only its new rows, though another agent writes between them.
    #[tokio::test]
    async fn a_follow_of_one_agent_sends_its_history_then_only_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("rho.redb"));
        let created = AgentEvent::Created {
            role: Default::default(),
            binding: rho_agent::log::SessionBinding::ResponsesSol(Default::default()),
            runtime: rho_agent::log::AgentRuntime::Rho {
                prompt_cache_key: rho_agent::inference::PromptCacheKey::generate(),
            },
            place: rho_agent_types::Place {
                workset: Some("0123456789ab".parse().unwrap()),
                cwd: "/src/rho".into(),
            },
            spawned_by: rho_agent::log::AgentSpawnedBy::Direct,
            spawn_name: None,
            created_at: UnixMs(0),
            parent: None,
        };
        let said = |text: &str| {
            AgentEvent::Entry(Entry::Received {
                at: UnixMs(1),
                id: rho_agent::entry::MessageId(1),
                from: rho_agent::entry::Party::Human,
                body: vec![rho_agent::entry::Block::Text(text.into())],
            })
        };
        let mut write = db.write().await;
        write.init_agent_tables();
        let followed = write.alloc_agent_id();
        let other = write.alloc_agent_id();
        write.append_agent_event(followed, &created);
        write.append_agent_event(other, &created);
        write.append_agent_event(other, &said("not this one"));
        write.append_agent_event(followed, &said("before"));
        write.commit();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let head = send_history(&db, &tx, &[followed]).await.unwrap();
        assert_eq!(head, Seq(4));
        let mut history = Vec::new();
        while let Ok(ServerFrame::Log { entries }) = rx.try_recv() {
            history.extend(entries);
        }
        assert!(
            history
                .iter()
                .all(|entry| entry.agent_id == followed && entry.seq == head)
        );
        assert_eq!(
            history.iter().map(|entry| entry.pos.0).collect::<Vec<_>>(),
            [0, 1]
        );

        let mut write = db.write().await;
        write.append_agent_event(other, &said("still not"));
        write.append_agent_event(followed, &said("after"));
        write.commit();
        let mut sent = head;
        assert!(send_journal_from(&db, &tx, &mut sent, &|agent| *agent == followed).await);
        let mut after = Vec::new();
        while let Ok(ServerFrame::Log { entries }) = rx.try_recv() {
            after.extend(entries);
        }
        assert_eq!(sent, Seq(6));
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!(
            (after[0].agent_id, after[0].pos.0, after[0].seq),
            (followed, 2, Seq(6))
        );
        assert!(
            matches!(&after[0].event, TranscriptEvent::Received { text, .. } if text == "after")
        );

        let mut write = db.write().await;
        write.append_agent_event(
            other,
            &AgentEvent::Entry(Entry::Received {
                at: UnixMs(7),
                id: rho_agent::entry::MessageId(2),
                from: rho_agent::entry::Party::Human,
                body: vec![rho_agent::entry::Block::Text("late".into())],
            }),
        );
        write.commit();
        let directory = agent_directory(&db);
        assert_eq!(
            directory
                .iter()
                .map(|entry| (entry.agent_id, entry.last_active))
                .collect::<std::collections::BTreeMap<_, _>>(),
            [(followed, UnixMs(1)), (other, UnixMs(7))].into(),
            "each agent by its last row"
        );
        assert!(
            directory
                .iter()
                .all(|entry| matches!(entry.created, TranscriptEvent::Created { .. }))
        );
    }

    #[tokio::test]
    async fn a_request_after_a_rewind_reports_the_step_still_standing() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("rho.redb"));
        let step = |id: &str| {
            AgentEvent::Entry(Entry::Step {
                at: UnixMs(1),
                exec: None,
                prose: String::new(),
                carry: rho_agent::inference::Carry::new(
                    serde_json::json!({"items": [{
                        "type": "custom_tool_call", "name": "exec", "call_id": id, "input": "cell"
                    }]}),
                    vec![rho_agent::inference::Call::new(id, "cell".into())],
                    false,
                ),
                usage: None,
            })
        };
        let request = AgentEvent::Entry(Entry::RequestSent {
            at: UnixMs(2),
            why: rho_agent::entry::Wake::Notify,
            report: Report {
                notices: vec![RequestNotice::Restarted],
                ..Default::default()
            },
            compact: false,
        });
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent = write.alloc_agent_id();
        write.append_agent_event(
            agent,
            &AgentEvent::Created {
                role: Default::default(),
                binding: rho_agent::log::SessionBinding::ResponsesSol(Default::default()),
                runtime: rho_agent::log::AgentRuntime::Rho {
                    prompt_cache_key: rho_agent::inference::PromptCacheKey::generate(),
                },
                place: rho_agent_types::Place {
                    workset: Some("0123456789ab".parse().unwrap()),
                    cwd: "/src/rho".into(),
                },
                spawned_by: rho_agent::log::AgentSpawnedBy::Direct,
                spawn_name: None,
                created_at: UnixMs(0),
                parent: None,
            },
        );
        write.append_agent_event(agent, &step("exec-1"));
        write.append_agent_event(agent, &request);
        let taken_back = write.append_agent_event(agent, &step("exec-2"));
        write.append_agent_event(
            agent,
            &AgentEvent::Rewound {
                to: taken_back,
                at: UnixMs(3),
            },
        );
        write.append_agent_event(agent, &request);
        write.commit();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(send_journal_from(&db, &tx, &mut Seq(0), &|_| true).await);
        let mut reported = Vec::new();
        while let Ok(ServerFrame::Log { entries }) = rx.try_recv() {
            for entry in entries {
                if let TranscriptEvent::NotebookReport { calls, .. } = entry.event {
                    reported.push(calls);
                }
            }
        }
        assert_eq!(reported, [["exec-1"], ["exec-1"]]);
    }

    #[test]
    fn tool_detail_reads_the_complete_host_record() {
        let result = rho_agent_types::transcript::ToolResult {
            call_id: rho_agent_types::transcript::ExecId::try_from("call-1").unwrap(),
            tool_type: rho_agent_types::transcript::ToolType::Custom,
            body: rho_agent_types::transcript::ToolOutput {
                output: Arc::new("bounded model view".to_owned()),
                full_output: Some(Arc::new("complete host record".to_owned())),
                images: Arc::new(Vec::new()),
                status: rho_agent_types::ToolOutputStatus::Success,
            },
            started_at: rho_agent_types::UnixMs(1),
            finished_at: rho_agent_types::UnixMs(2),
        };

        assert_eq!(detail_result(&result).output, "complete host record");
    }
}
