//! The agents part of the agent host, and the parts about one agent's workset:
//! its terminals and shells. The session carries the directory, the chat,
//! the rows of the worksets a client follows, the live tails and the quota;
//! requests, terminals and shells are streams of their own.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use rho_agent::db::{AgentReadTxnExt as _, AgentWriteTxnExt as _};
use rho_agent::record::{Event, Record};
use rho_agent::store::{AgentHead, Appended, ChatRead as _, StoreRead as _, WorksetHead};
use rho_agent_types::{AgentId, ChatSeq, WorksetId, WorksetKind, Wpos};
use rho_agents_client::protocol::transcript::Live;
use rho_agents_client::protocol::worksets::{
    AgentEntry, AgentLog, AgentLogPage, Cancel, ChangeTopic, ClientFrame, CreateStream,
    CreateWorkset, CreatedWorkset, DeleteWorksets, Eval, EvalResult, Leave, LogEvent, LogNext,
    MoveMessages, MoveTo, Open, Post, Request, RestartWorkset, ServerFrame, StartTopic,
    WorksetEntry,
};
use rho_agents_client::protocol::{
    AgentCostDistribution, ClaudeAccountList, ClaudeAccounts, GlobalUsage, QuotaHistory,
    QuotaUsage, RecordVisualization, SetAuthAccountEnabled, SetClaudeAccount, Visualization,
    VisualizationContent,
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

/// How many rows or chat events travel in one frame while a client is catching
/// up, so a cold copy goes in pages the connection can interleave.
const LOG_PAGE: usize = 512;

/// How often a session hears the quota when nothing has moved it.
const QUOTA_REFRESH: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// What a client asks the session's follower for.
enum Follow {
    Chat(ChatSeq),
    Worksets(Vec<(WorksetId, Wpos)>),
    Focus(HashSet<AgentId>),
}

/// A client's session: `Hello`, the directory, then whatever the client
/// follows (the chat, some worksets' rows, the live tails of the agents it
/// looks at) as it lands, beside the auth state and the quota. A stream of
/// its own, so a catch-up queues behind nothing and holds nothing up.
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
    // Both subscribed before anything is read, so nothing that lands in
    // between is lost; positions drop what is told twice.
    let feed = rho_agent::store::feed(&services.db);
    let statuses = services.pool.watch_statuses().await;
    let (chat_head, agent_counter) = {
        let read = services.db.read();
        (read.chat_head(), read.last_agent_counter())
    };
    let _ = outgoing_tx.send(ServerFrame::Hello {
        machine_seed: services.machine_seed,
        chat_head,
        agent_counter,
    });
    let _ = outgoing_tx.send(directory(&services.db));
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
    let (follow_tx, follow_rx) = mpsc::unbounded_channel();
    let follower = tokio::spawn(follow(
        Arc::clone(&services),
        outgoing_tx.clone(),
        feed,
        statuses,
        follow_rx,
    ));
    // Names this stream's focus in the pool's live set, so it leaves with
    // the stream.
    let stream_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let result = loop {
        let frame =
            match rho_rpc::protocol::read_frame_optional::<_, ClientFrame>(&mut reader).await {
                Ok(Some(frame)) => frame,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
        match frame {
            ClientFrame::FollowChat { since } => {
                let _ = follow_tx.send(Follow::Chat(since));
            }
            ClientFrame::FollowWorksets { from } => {
                let _ = follow_tx.send(Follow::Worksets(from));
            }
            ClientFrame::Focus { agent_ids } => {
                if agent_ids.len() > 64 {
                    break Err(anyhow::anyhow!("too many focused agents"));
                }
                let agent_ids = agent_ids.into_iter().collect::<HashSet<_>>();
                // Focus is what this client is looking at, nothing more: it
                // never loads an agent. The pool unions it across streams
                // into the live set; a loaded agent in it shows its response.
                services
                    .pool
                    .set_live_wants(stream_id, agent_ids.clone())
                    .await;
                let _ = follow_tx.send(Follow::Focus(agent_ids));
            }
            ClientFrame::Detail { workset, pos, more } => {
                // One answer per position, each naming its own `pos`.
                for pos in std::iter::once(pos).chain(more) {
                    let body = detail(&services.db, workset, pos);
                    let _ = outgoing_tx.send(ServerFrame::Detail { workset, pos, body });
                }
            }
        }
    };
    follower.abort();
    auth_task.abort();
    quota_task.abort();
    services
        .pool
        .set_live_wants(stream_id, HashSet::new())
        .await;
    writer_task.abort();
    result
}

/// Every workset and agent on the host.
fn directory(db: &RhoDb) -> ServerFrame {
    let read = db.read();
    ServerFrame::Directory {
        worksets: read
            .worksets()
            .into_iter()
            .map(|(workset, head)| workset_entry(workset, &head))
            .collect(),
        agents: read
            .agents()
            .into_iter()
            .map(|(agent, head)| agent_entry(agent, &head))
            .collect(),
    }
}

fn workset_entry(workset: WorksetId, head: &WorksetHead) -> WorksetEntry {
    WorksetEntry {
        workset,
        kind: head.kind.clone(),
        created_at: head.created_at,
    }
}

fn agent_entry(agent: AgentId, head: &AgentHead) -> AgentEntry {
    AgentEntry {
        agent,
        workset: head.workset,
        spec: crate::transcript::spec_view(&head.spec),
        created_at: head.created_at,
    }
}

/// The directory frame one appended row calls for, if it changed what the
/// directory says.
fn directory_change(db: &RhoDb, appended: Appended) -> Option<ServerFrame> {
    match appended {
        Appended::Chat(_) => None,
        Appended::WorksetDeleted(workset) => Some(ServerFrame::WorksetDeleted { workset }),
        Appended::Row { workset, pos } => {
            let read = db.read();
            match read.workset_row(workset, pos)?.event {
                Event::WorksetCreated { .. } => read
                    .workset(workset)
                    .map(|head| ServerFrame::Workset(workset_entry(workset, &head))),
                Event::AgentCreated { agent, .. } | Event::AgentChanged { agent, .. } => read
                    .agent(agent)
                    .map(|head| ServerFrame::Agent(agent_entry(agent, &head))),
                _ => None,
            }
        }
    }
}

/// The session's one pipe for everything that follows the store: directory
/// changes, then chat and rows past what the client holds, then the
/// statuses, which arrive after the rows they reflect commit. Rows are read
/// from the store, never from the feed: an append only says something
/// landed. A lagged feed sends the directory whole.
///
/// Statuses are full snapshots, the latest per agent: an unsent one is
/// replaced rather than queued, so a slow connection never falls behind.
async fn follow(
    services: Arc<Services>,
    outgoing_tx: mpsc::UnboundedSender<ServerFrame>,
    mut feed: broadcast::Receiver<Appended>,
    statuses: Arc<rho_agent::host::pool::Statuses>,
    mut asks: mpsc::UnboundedReceiver<Follow>,
) {
    let mut chat: Option<ChatSeq> = None;
    let mut worksets: HashMap<WorksetId, Wpos> = HashMap::new();
    let mut focus: HashSet<AgentId> = HashSet::new();
    let mut carries = Carries::new();
    loop {
        // Taken before the rows are read: an agent's status arrives after
        // the rows it reflects commit, so they go out first.
        let changed = statuses.take();
        if let Some(sent) = chat.as_mut()
            && !send_chat_from(&services.db, &outgoing_tx, sent).await
        {
            return;
        }
        for (workset, next) in &mut worksets {
            if !send_rows_from(&services.db, &outgoing_tx, &mut carries, *workset, next).await {
                return;
            }
        }
        for (agent_id, status) in changed {
            if outgoing_tx
                .send(ServerFrame::Status {
                    agent_id,
                    state: status.runtime.clone(),
                })
                .is_err()
            {
                return;
            }
            if focus.contains(&agent_id) {
                let live = Live::Snapshot {
                    state: status.runtime.clone(),
                    response: status.response.clone(),
                    draft: status.draft.clone(),
                };
                if outgoing_tx
                    .send(ServerFrame::Live { agent_id, live })
                    .is_err()
                {
                    return;
                }
            }
        }
        tokio::select! {
            appended = feed.recv() => {
                let mut next = Some(appended);
                while let Some(appended) = next.take() {
                    let frame = match appended {
                        Ok(appended) => directory_change(&services.db, appended),
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            Some(directory(&services.db))
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    };
                    if let Some(frame) = frame
                        && outgoing_tx.send(frame).is_err()
                    {
                        return;
                    }
                    next = match feed.try_recv() {
                        Ok(appended) => Some(Ok(appended)),
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            Some(Err(broadcast::error::RecvError::Lagged(n)))
                        }
                        Err(broadcast::error::TryRecvError::Empty) => None,
                        Err(broadcast::error::TryRecvError::Closed) => return,
                    };
                }
            }
            () = statuses.changed() => {}
            ask = asks.recv() => match ask {
                None => return,
                Some(Follow::Chat(since)) => chat = Some(since),
                Some(Follow::Worksets(from)) => {
                    // A workset already followed goes on from what this
                    // stream sent: rows past the client's position are on
                    // their way to it, and are never sent twice.
                    worksets = from
                        .into_iter()
                        .map(|(workset, from)| {
                            (workset, worksets.get(&workset).map_or(from, |sent| from.max(*sent)))
                        })
                        .collect();
                    carries.clear();
                }
                Some(Follow::Focus(agents)) => {
                    // The pool made them live before this ask was sent, but
                    // a status it told then may have been taken while they
                    // were not yet in focus, and one live through another
                    // connection is not told again. Each newly focused agent
                    // goes out now, on the next turn of this loop.
                    let added = agents.difference(&focus).copied().collect::<Vec<_>>();
                    services.pool.resend_statuses(&statuses, added).await;
                    focus = agents;
                }
            },
        }
    }
}

/// Pages the chat out from after `sent`, moving it as it goes. False when
/// the connection is gone.
async fn send_chat_from(
    db: &RhoDb,
    outgoing_tx: &mpsc::UnboundedSender<ServerFrame>,
    sent: &mut ChatSeq,
) -> bool {
    loop {
        let records = db.read().chat_since(*sent, LOG_PAGE);
        let Some(last) = records.last() else {
            return true;
        };
        *sent = last.seq;
        if outgoing_tx.send(ServerFrame::Chat { records }).is_err() {
            return false;
        }
        // Catching up must never starve the connection's own traffic.
        tokio::task::yield_now().await;
    }
}

/// Pages a workset's rows out from `next`, moving it as it goes. Rows the
/// transcript leaves behind advance `next` without a message. False when
/// the connection is gone.
async fn send_rows_from(
    db: &RhoDb,
    outgoing_tx: &mpsc::UnboundedSender<ServerFrame>,
    carries: &mut Carries,
    workset: WorksetId,
    next: &mut Wpos,
) -> bool {
    loop {
        let page = db.read().workset_rows(workset, *next, LOG_PAGE);
        let Some((last, _)) = page.last() else {
            return true;
        };
        *next = last.next();
        let rows = page
            .iter()
            .filter_map(|(pos, record)| Some((*pos, strip_row(db, carries, *pos, record)?)))
            .collect::<Vec<_>>();
        if !rows.is_empty()
            && outgoing_tx
                .send(ServerFrame::Rows { workset, rows })
                .is_err()
        {
            return false;
        }
        tokio::task::yield_now().await;
    }
}

/// Each agent's newest step as of the rows already read, so a request reads it
/// back from the log once per agent, not once per request.
type Carries = HashMap<AgentId, Option<rho_agent::inference::Carry>>;

/// A row as clients read it, or `None` for one they never see. Rows must
/// come in order per agent; a rewind sends the agent back to the store.
fn strip_row(
    db: &RhoDb,
    carries: &mut Carries,
    pos: Wpos,
    record: &Record<'_>,
) -> Option<LogEvent> {
    let Some(agent) = record.event.agent() else {
        return crate::transcript::strip(record, None);
    };
    if matches!(record.event, Event::Rewound { .. }) {
        carries.remove(&agent);
        return crate::transcript::strip(record, None);
    }
    let carry = carries
        .entry(agent)
        .or_insert_with(|| crate::transcript::carry_before(&db.read(), agent, pos));
    let stripped = crate::transcript::strip(record, carry.as_ref());
    if let Event::Native {
        entry: rho_agent::entry::Entry::Step { carry: step, .. },
        ..
    } = &record.event
    {
        *carry = Some(step.clone());
    }
    stripped
}

/// One page of an agent's rows, stripped as the session strips them: read
/// through the agent's index, so the workset's other rows cost nothing.
fn agent_log_page(db: &RhoDb, AgentLog { agent, from }: AgentLog) -> anyhow::Result<AgentLogPage> {
    let (page, end) = {
        let read = db.read();
        let head = read
            .agent(agent)
            .ok_or_else(|| anyhow::anyhow!("no agent {} on this host", agent.encoded()))?;
        let end = read
            .workset(head.workset)
            .ok_or_else(|| anyhow::anyhow!("no workset {} on this host", head.workset))?
            .next;
        (read.agent_log(agent, from, LOG_PAGE), end)
    };
    let next = match page.last() {
        Some((last, _)) if page.len() == LOG_PAGE => LogNext::Page(last.next()),
        _ => LogNext::Follow(end),
    };
    let mut carries = Carries::new();
    let rows = page
        .iter()
        .filter_map(|(pos, record)| Some((*pos, strip_row(db, &mut carries, *pos, record)?)))
        .collect();
    Ok(AgentLogPage { rows, next })
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

fn eval_result(evaluated: rho_agent::host::pool::Evaluated) -> EvalResult {
    EvalResult {
        output: evaluated.output,
        error: evaluated.error,
        agent: evaluated.agent,
    }
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
        Request::CreateWorkset(call) => {
            respond(writer, call, |CreateWorkset { kind, code }| async move {
                let kind = match kind {
                    WorksetKind::Host { cwd } => WorksetKind::Host {
                        cwd: crate::expand_home(&cwd).unwrap_or(cwd),
                    },
                    kind => kind,
                };
                let (workset, evaluated) = services.pool.create_workset(kind, code).await?;
                Ok(CreatedWorkset {
                    workset,
                    result: eval_result(evaluated),
                })
            })
            .await
        }
        Request::Eval(call) => {
            respond(writer, call, |Eval { workset, code }| async move {
                Ok(eval_result(services.pool.eval(workset, code).await?))
            })
            .await
        }
        Request::Post(call) => {
            respond(
                writer,
                call,
                |Post {
                     client_id,
                     topic,
                     mentions,
                     mut content,
                 }| async move {
                    prepare_image_content(&mut content).await?;
                    services
                        .pool
                        .post_user(client_id, topic, mentions, content)
                        .await
                },
            )
            .await
        }
        Request::StartTopic(call) => {
            respond(
                writer,
                call,
                |StartTopic {
                     client_id,
                     stream,
                     name,
                     mentions,
                     mut content,
                 }| async move {
                    prepare_image_content(&mut content).await?;
                    services
                        .pool
                        .start_user_topic(client_id, stream, name, mentions, content)
                        .await
                },
            )
            .await
        }
        Request::CreateStream(call) => {
            respond(writer, call, |CreateStream { name }| async move {
                services.pool.create_stream(name).await
            })
            .await
        }
        Request::Leave(call) => {
            respond(writer, call, |Leave { topic, agent }| async move {
                services.pool.leave_user(topic, agent).await
            })
            .await
        }
        Request::ChangeTopic(call) => {
            respond(
                writer,
                call,
                |ChangeTopic {
                     topic,
                     stream,
                     name,
                     resolved,
                 }| async move {
                    services
                        .pool
                        .change_topic(topic, stream, name, resolved)
                        .await
                },
            )
            .await
        }
        Request::MoveMessages(call) => {
            respond(
                writer,
                call,
                |MoveMessages {
                     client_id,
                     messages,
                     to,
                 }| async move {
                    let to = match to {
                        MoveTo::Topic(topic) => rho_agent::store::MoveTo::Topic(topic),
                        MoveTo::NewTopic { stream, name } => {
                            rho_agent::store::MoveTo::NewTopic { stream, name }
                        }
                    };
                    services.pool.move_messages(client_id, messages, to).await
                },
            )
            .await
        }
        Request::Cancel(call) => {
            respond(writer, call, |Cancel { agent }| async move {
                services.pool.cancel(agent).await
            })
            .await
        }
        Request::RestartWorkset(call) => {
            respond(writer, call, |RestartWorkset { agent }| async move {
                services.pool.restart_agent_workset(agent).await
            })
            .await
        }
        Request::DeleteWorksets(call) => {
            respond(writer, call, |DeleteWorksets { worksets }| async move {
                services.pool.delete_worksets(worksets).await
            })
            .await
        }
        Request::AgentLog(call) => {
            respond(writer, call, |call| async {
                agent_log_page(&services.db, call)
            })
            .await
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
    let agent = services.resolve_display_agent_id(agent)?;
    let process = services.pool.execution(agent).await?;
    let cwd = services.cwd_of(agent)?;
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
    let agent = services.resolve_display_agent_id(agent)?;
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
        Some(agent) => Some(services.resolve_display_agent_id(agent)?.encoded()),
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
    let agent = services.resolve_display_agent_id(agent)?;
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
    let agent = services.resolve_display_agent_id(agent)?;
    let process = services.pool.execution(agent).await?;
    let cwd = services.cwd_of(agent)?;
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
        Some(agent) => Some(services.resolve_display_agent_id(agent)?.encoded()),
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

/// A request's tool results whole, or a response as the transcript draws
/// it. Rows a rewind hid still answer; the client asked for one it holds.
fn detail(
    db: &RhoDb,
    workset: WorksetId,
    pos: Wpos,
) -> rho_agents_client::protocol::transcript::DetailBody {
    use rho_agent::entry::Entry;
    use rho_agent::log::TranscriptLine;
    use rho_agent::record::ClaudeRow;
    use rho_agents_client::protocol::transcript::{
        ArgumentsFormat, DetailBody, DetailResult, Item, ToolStatus,
    };
    let read = db.read();
    let Some(record) = read.workset_row(workset, pos) else {
        return DetailBody::Nothing;
    };
    match record.event {
        Event::Request {
            agent,
            output,
            inputs,
            notices,
            ..
        } => DetailBody::Results(
            crate::transcript::report_results(
                crate::transcript::carry_before(&read, agent, pos).as_ref(),
                &notices,
                &output,
                &inputs,
            )
            .into_iter()
            .map(|result| DetailResult {
                id: result.display_id().to_owned(),
                status: ToolStatus::Reported,
                output: result.text,
                error: None,
            })
            .collect(),
        ),
        Event::Native {
            entry: Entry::Step { carry, .. },
            ..
        } => DetailBody::Response(crate::transcript::step_items(&carry.display_calls())),
        Event::Claude {
            row: ClaudeRow::Transcript { line, .. },
            ..
        } => match line {
            TranscriptLine::Assistant { calls, .. } => DetailBody::Response(
                calls
                    .into_iter()
                    .map(|call| Item::ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                        format: ArgumentsFormat::Json,
                    })
                    .collect(),
            ),
            TranscriptLine::ToolResults { results } => {
                DetailBody::Results(results.iter().map(detail_result).collect())
            }
            TranscriptLine::User { .. } | TranscriptLine::Compacted { .. } => DetailBody::Nothing,
        },
        Event::Failed { partial, .. } => DetailBody::Response(
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
    use rho_agent::db::AgentWriteTxnExt as _;
    use rho_agent::entry::{Entry, Notice, RequestNotice};
    use rho_agent::record::{AgentSpec, ClaudeRow, Event, Record, Runtime};
    use rho_agent::store::StoreWrite as _;
    use rho_agent_types::{Role, UnixMs, WorksetId, WorksetKind, Wpos};
    use rho_agents_client::protocol::worksets::{AgentLog, LogEvent, LogNext};

    use super::{agent_log_page, detail, detail_result};

    fn step(id: &str) -> Entry {
        Entry::Step {
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
        }
    }

    /// A request with `output`, `inputs` and a notice. `workset` gives it its
    /// agent.
    fn request_of(
        output: &str,
        inputs: Vec<rho_agent::record::Input>,
        notices: Vec<RequestNotice>,
    ) -> Event<'static> {
        Event::Request {
            agent: rho_agent_types::AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(1))
                .unwrap(),
            why: rho_agent::entry::Wake::Notify,
            output: if output.is_empty() {
                Vec::new()
            } else {
                vec![rho_agent_types::ContentPart::Text {
                    text: output.into(),
                }]
            },
            inputs,
            notices,
            compact: false,
        }
    }

    fn request() -> Event<'static> {
        request_of("", Vec::new(), vec![RequestNotice::Restarted])
    }

    /// A workset with one native agent and its rows, as the loop writes
    /// them. Returns the workset, the agent and each row's position.
    async fn workset(
        db: &rho_db::RhoDb,
        rows: Vec<Event<'static>>,
    ) -> (WorksetId, rho_agent_types::AgentId, Vec<Wpos>) {
        let workset: WorksetId = "0123456789ab".parse().unwrap();
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent = write.alloc_agent_id();
        write.create_workset(workset, UnixMs(0), WorksetKind::Isolated, String::new());
        write
            .append(
                workset,
                &Record {
                    at: UnixMs(0),
                    event: Event::AgentCreated {
                        agent,
                        spec: AgentSpec {
                            runtime: Runtime::Native {
                                model: rho_agent::inference::config::InferenceModel::Gpt61Sol,
                                profile: Default::default(),
                                cache_key: rho_agent::inference::PromptCacheKey::generate(),
                            },
                            props: Default::default(),
                        },
                    },
                },
            )
            .unwrap();
        let mut positions = Vec::new();
        for event in rows {
            let event = match event {
                Event::Native { entry, .. } => Event::Native { agent, entry },
                Event::Rewound { to, .. } => Event::Rewound { agent, to },
                Event::Request {
                    why,
                    output,
                    inputs,
                    notices,
                    compact,
                    ..
                } => Event::Request {
                    agent,
                    why,
                    output,
                    inputs,
                    notices,
                    compact,
                },
                Event::Claude { row, .. } => Event::Claude { agent, row },
                event => event,
            };
            positions.push(
                write
                    .append(
                        workset,
                        &Record {
                            at: UnixMs(1),
                            event,
                        },
                    )
                    .unwrap(),
            );
        }
        write.commit();
        (workset, agent, positions)
    }

    fn native(entry: Entry) -> Event<'static> {
        Event::Native {
            agent: rho_agent_types::AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0))
                .unwrap(),
            entry,
        }
    }

    /// The report names the calls of the step it answers: the newest one
    /// still standing, read from the store when the page starts mid-way,
    /// and after a rewind that took the newer step back.
    #[tokio::test]
    async fn a_request_after_a_rewind_reports_the_step_still_standing() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("rho.redb"));
        // Rows 2..=5; the rewind at 6 takes back row 4 (exec-2) and on.
        let (workset, agent, positions) = workset(
            &db,
            vec![
                native(step("exec-1")),
                request(),
                native(step("exec-2")),
                request(),
                Event::Rewound {
                    agent: rho_agent_types::AgentId::from_counter(
                        1,
                        &rho_agent_types::AgentIdDomain(0),
                    )
                    .unwrap(),
                    to: Wpos(4),
                },
                request(),
            ],
        )
        .await;
        assert_eq!(positions[2], Wpos(4));
        let reports = |from| {
            agent_log_page(&db, AgentLog { agent, from })
                .unwrap()
                .rows
                .into_iter()
                .filter_map(|(pos, event)| match event {
                    LogEvent::Request { calls, .. } => Some((pos.0, calls)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let expected = vec![
            (3, vec!["exec-1".to_owned()]),
            (5, vec!["exec-2".to_owned()]),
            (7, vec!["exec-1".to_owned()]),
        ];
        assert_eq!(reports(Wpos::ZERO), expected);
        // A page that starts after the steps reads them back from the store.
        assert_eq!(reports(Wpos(5)), expected[1..]);
        assert_eq!(reports(Wpos(7)), expected[2..]);
        // The detail of the last report is read the same way.
        let rho_agents_client::protocol::transcript::DetailBody::Results(results) =
            detail(&db, workset, Wpos(7))
        else {
            panic!("a report's detail is its results")
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "exec-1");
    }

    /// Rows that say nothing a reader uses are left behind; a stop is told
    /// whichever runtime wrote it; a user's spoken request reads as asked.
    /// A request's detail is its own output and peers, and nothing an earlier
    /// request carried. A person's text and the peer's message show as blocks
    /// of their own in the row a reader keeps.
    #[tokio::test]
    async fn a_requests_detail_shows_what_it_carried() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("rho.redb"));
        let input = |role, text: &str, message| rho_agent::record::Input {
            role,
            message,
            content: vec![rho_agent_types::ContentPart::Text { text: text.into() }],
        };
        let prose = native(Entry::Step {
            exec: None,
            prose: "words".into(),
            carry: rho_agent::inference::Carry::new(serde_json::json!({}), vec![], false),
            usage: None,
        });
        let message = rho_agent_types::MessageId(rho_agent_types::ChatSeq(1));
        let (workset, _, pos) = workset(
            &db,
            vec![
                native(step("answered")),
                request_of("answered-output", Vec::new(), Vec::new()),
                prose,
                request_of("", Vec::new(), Vec::new()),
                native(step("open")),
                request_of(
                    "printed-output",
                    vec![
                        input(Role::User, "from-the-person", None),
                        input(Role::Tool, "peer-message", Some(message)),
                    ],
                    Vec::new(),
                ),
            ],
        )
        .await;
        let rho_agents_client::protocol::transcript::DetailBody::Results(results) =
            detail(&db, workset, pos[5])
        else {
            panic!("the request's results");
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "open");
        assert_eq!(results[0].output, "printed-output\n\npeer-message");

        use rho_agent::store::StoreRead as _;
        let read = db.read();
        let row = read.workset_row(workset, pos[5]).unwrap();
        let carry = crate::transcript::carry_before(&read, row.event.agent().unwrap(), pos[5]);
        let Some(LogEvent::Request { inputs, calls, .. }) =
            crate::transcript::strip(&row, carry.as_ref())
        else {
            panic!("a request reads as a request");
        };
        assert_eq!(calls, ["open"]);
        assert_eq!(
            inputs
                .iter()
                .map(|input| (input.text.as_str(), input.role, input.message))
                .collect::<Vec<_>>(),
            [
                ("from-the-person", Role::User, None),
                ("peer-message", Role::Tool, Some(message)),
            ]
        );
    }

    /// A `tool` input that is not a chat message is no longer mistaken for
    /// output: a reader keeps it, and the call's answer holds it.
    #[tokio::test]
    async fn a_plain_tool_input_is_not_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("rho.redb"));
        let (workset, _, pos) = workset(
            &db,
            vec![
                native(step("call")),
                request_of(
                    "",
                    vec![rho_agent::record::Input {
                        role: Role::Tool,
                        message: None,
                        content: vec![rho_agent_types::ContentPart::Text {
                            text: "plain-tool-input".into(),
                        }],
                    }],
                    Vec::new(),
                ),
            ],
        )
        .await;
        use rho_agent::store::StoreRead as _;
        let row = db.read().workset_row(workset, pos[1]).unwrap();
        let Some(LogEvent::Request { inputs, .. }) = crate::transcript::strip(&row, None) else {
            panic!("a request reads as a request");
        };
        assert_eq!(inputs[0].text, "plain-tool-input");
    }

    #[tokio::test]
    async fn rows_are_told_in_the_readers_words() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("rho.redb"));
        let spoken =
            "<realtime_delegation>\n  <input>fix &lt;b&gt;</input>\n</realtime_delegation>";
        let (_, agent, _) = workset(
            &db,
            vec![
                request_of(
                    "",
                    vec![rho_agent::record::Input {
                        role: Role::User,
                        message: Some(rho_agent_types::MessageId(rho_agent_types::ChatSeq(3))),
                        content: vec![rho_agent_types::ContentPart::Text {
                            text: spoken.to_owned(),
                        }],
                    }],
                    Vec::new(),
                ),
                native(Entry::Notice {
                    notice: Notice::Restarted,
                }),
                native(Entry::Notice {
                    notice: Notice::Stopped("quota".into()),
                }),
                Event::Claude {
                    agent: rho_agent_types::AgentId::from_counter(
                        1,
                        &rho_agent_types::AgentIdDomain(0),
                    )
                    .unwrap(),
                    row: ClaudeRow::Notice(Notice::Stopped("auth".into())),
                },
            ],
        )
        .await;
        let rows = agent_log_page(
            &db,
            AgentLog {
                agent,
                from: Wpos(2),
            },
        )
        .unwrap()
        .rows;
        assert_eq!(
            rows,
            vec![
                (
                    Wpos(2),
                    LogEvent::Request {
                        agent,
                        inputs: vec![rho_agents_client::protocol::worksets::InputView {
                            message: Some(rho_agent_types::MessageId(rho_agent_types::ChatSeq(3))),
                            text: "🎤 fix <b>".into(),
                            role: Role::User,
                        }],
                        calls: Vec::new(),
                        compaction: false,
                        at: UnixMs(1),
                    }
                ),
                (
                    Wpos(4),
                    LogEvent::Stopped {
                        agent,
                        error: "quota".into(),
                        at: UnixMs(1),
                    }
                ),
                (
                    Wpos(5),
                    LogEvent::Stopped {
                        agent,
                        error: "auth".into(),
                        at: UnixMs(1),
                    }
                ),
            ]
        );
    }

    /// A workset shaped like a migrated one: many agents' rows merged by
    /// time, a few agents active at once. Returns the workset, its agents
    /// and the agent with the most rows.
    async fn crowded_workset(
        db: &rho_db::RhoDb,
        agents: usize,
        rows: usize,
    ) -> (WorksetId, Vec<rho_agent_types::AgentId>) {
        let workset: WorksetId = "0123456789ab".parse().unwrap();
        let mut ids = Vec::new();
        {
            let mut write = db.write().await;
            write.init_agent_tables();
            write.create_workset(workset, UnixMs(0), WorksetKind::Isolated, String::new());
            write.commit();
        }
        let cell = "x = 1\n".repeat(60);
        let mut written = 0usize;
        let mut created = 0usize;
        // Agent `i` is active over rows [i * span, i * span + 4 * span).
        let span = rows / agents;
        while written < rows {
            let mut write = db.write().await;
            for _ in 0..10_000 {
                if written >= rows {
                    break;
                }
                while created < agents && created * span <= written {
                    let agent = write.alloc_agent_id();
                    write
                        .append(
                            workset,
                            &Record {
                                at: UnixMs(0),
                                event: Event::AgentCreated {
                                    agent,
                                    spec: AgentSpec {
                                        runtime: Runtime::Native {
                                            model: rho_agent::inference::config::InferenceModel::Gpt61Sol,
                                            profile: Default::default(),
                                            cache_key: rho_agent::inference::PromptCacheKey::generate(),
                                        },
                                        props: [("name".to_owned(), format!("crowded-{created}"))]
                                            .into_iter()
                                            .collect(),
                                    },
                                },
                            },
                        )
                        .unwrap();
                    ids.push(agent);
                    created += 1;
                }
                let active: Vec<_> = (0..created)
                    .filter(|i| written < i * span + 4 * span)
                    .collect();
                let agent = ids[active[written % active.len()]];
                let entry = match (written / active.len()) % 3 {
                    0 => {
                        let event = Event::Request {
                            agent,
                            why: rho_agent::entry::Wake::Notify,
                            output: Vec::new(),
                            inputs: Vec::new(),
                            notices: Vec::new(),
                            compact: false,
                        };
                        write
                            .append(
                                workset,
                                &Record {
                                    at: UnixMs(written as u64),
                                    event,
                                },
                            )
                            .unwrap();
                        written += 1;
                        continue;
                    }
                    _ => Entry::Step {
                        exec: None,
                        prose: String::new(),
                        carry: rho_agent::inference::Carry::new(
                            serde_json::json!({"items": [{
                                "type": "custom_tool_call", "name": "exec",
                                "call_id": format!("c{written}"), "input": cell
                            }]}),
                            vec![rho_agent::inference::Call::new(
                                format!("c{written}"),
                                cell.clone(),
                            )],
                            false,
                        ),
                        usage: None,
                    },
                };
                write
                    .append(
                        workset,
                        &Record {
                            at: UnixMs(1),
                            event: Event::Native { agent, entry },
                        },
                    )
                    .unwrap();
                written += 1;
            }
            write.commit();
        }
        (workset, ids)
    }

    /// An agent's log is its own rows, page by page, then where its
    /// workset's next row goes: what following the workset starts from.
    #[tokio::test]
    async fn an_agents_log_is_its_rows_then_where_to_follow() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("rho.redb"));
        // Three agents over 6,000 rows: the middle one writes about a
        // third of them, interleaved with the others, over several pages.
        let (workset, agents) = crowded_workset(&db, 3, 6_000).await;
        let agent = agents[1];
        let read = db.read();
        let expected =
            rho_agent::store::StoreRead::workset_rows(&read, workset, Wpos::ZERO, usize::MAX)
                .into_iter()
                .filter(|(_, record)| record.event.agent() == Some(agent))
                .map(|(pos, _)| pos)
                .collect::<Vec<_>>();
        let end = rho_agent::store::StoreRead::workset(&read, workset)
            .unwrap()
            .next;
        drop(read);
        assert!(expected.len() > 2 * super::LOG_PAGE, "it spans pages");

        let mut from = Wpos(1);
        let mut read = Vec::new();
        let follow = loop {
            let page = agent_log_page(&db, AgentLog { agent, from }).unwrap();
            assert!(
                page.rows
                    .iter()
                    .all(|(_, event)| event.agent() == Some(agent))
            );
            read.extend(page.rows.iter().map(|(pos, _)| *pos));
            match page.next {
                LogNext::Page(next) => from = next,
                LogNext::Follow(at) => break at,
            }
        };
        let expected_from = expected
            .into_iter()
            .filter(|pos| *pos >= Wpos(1))
            .collect::<Vec<_>>();
        assert_eq!(read, expected_from);
        assert_eq!(follow, end);
    }

    /// Not a test: times reading a crowded workset's agent the way the
    /// Activity view does. `cargo test --release -p rho-agent-host --lib
    /// crowded_workset_log -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn crowded_workset_log() {
        // `RHO_BENCH_DB` keeps the store, for a host to serve.
        let dir = tempfile::tempdir().unwrap();
        let path = std::env::var_os("RHO_BENCH_DB")
            .map_or_else(|| dir.path().join("rho.redb"), std::path::PathBuf::from);
        let db = rho_db::RhoDb::open(path);
        let started = std::time::Instant::now();
        let (_, agents) = crowded_workset(&db, 66, 330_000).await;
        eprintln!("built {} agents in {:?}", agents.len(), started.elapsed());

        let started = std::time::Instant::now();
        let mut from = Wpos::ZERO;
        let (mut pages, mut rows) = (0, 0);
        loop {
            let page = agent_log_page(
                &db,
                AgentLog {
                    agent: agents[30],
                    from,
                },
            )
            .unwrap();
            pages += 1;
            rows += page.rows.len();
            match page.next {
                LogNext::Page(next) => from = next,
                LogNext::Follow(_) => break,
            }
        }
        eprintln!(
            "one agent's log: {pages} pages, {rows} rows in {:?}",
            started.elapsed()
        );
    }

    /// What `carry_before` answers, by walking the whole workset forwards: the
    /// reference.
    fn carry_by_scan(
        read: &rho_db::ReadTxn,
        agent: rho_agent_types::AgentId,
        pos: Wpos,
    ) -> Option<rho_agent::inference::Carry> {
        use rho_agent::store::StoreRead as _;
        let head = read.agent(agent).unwrap();
        let mut kept: Vec<(Wpos, Record<'static>)> = Vec::new();
        for (row, record) in read.workset_rows(head.workset, head.created, usize::MAX) {
            if row >= pos {
                break;
            }
            if record.event.agent() != Some(agent) {
                continue;
            }
            if let Event::Rewound { to, .. } = &record.event {
                let keep = kept.partition_point(|(kept, _)| kept < to);
                kept.truncate(keep);
            }
            kept.push((row, record));
        }
        kept.into_iter()
            .rev()
            .find_map(|(_, record)| match record.event {
                Event::Native {
                    entry: Entry::Step { carry, .. },
                    ..
                } => Some(carry),
                _ => None,
            })
    }

    /// Three agents' steps, requests and rewinds (rewinds of rewinds among
    /// them), interleaved: the step before every row, read newest first from
    /// the agent's own rows, is the forward fold's.
    #[tokio::test]
    async fn carry_before_is_the_forward_fold_of_the_agents_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("rho.redb"));
        let workset: WorksetId = "0123456789ab".parse().unwrap();
        let mut write = db.write().await;
        write.init_agent_tables();
        write.create_workset(workset, UnixMs(0), WorksetKind::Isolated, String::new());
        let mut agents = Vec::new();
        let mut rows: Vec<Vec<Wpos>> = Vec::new();
        for _ in 0..3 {
            let agent = write.alloc_agent_id();
            let pos = write
                .append(
                    workset,
                    &Record {
                        at: UnixMs(0),
                        event: Event::AgentCreated {
                            agent,
                            spec: AgentSpec {
                                runtime: Runtime::Native {
                                    model: rho_agent::inference::config::InferenceModel::Gpt61Sol,
                                    profile: Default::default(),
                                    cache_key: rho_agent::inference::PromptCacheKey::generate(),
                                },
                                props: Default::default(),
                            },
                        },
                    },
                )
                .unwrap();
            agents.push(agent);
            rows.push(vec![pos]);
        }
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for n in 0..600 {
            let which = next(3);
            let agent = agents[which];
            let event = match next(9) {
                0 | 1 => native(step(&format!("exec-{n}"))),
                2..=6 => Event::Request {
                    agent,
                    why: rho_agent::entry::Wake::Notify,
                    output: Vec::new(),
                    inputs: Vec::new(),
                    notices: Vec::new(),
                    compact: false,
                },
                // A step with no call stands as the newest step.
                7 => native(Entry::Step {
                    exec: None,
                    prose: format!("prose-{n}"),
                    carry: rho_agent::inference::Carry::new(serde_json::json!({}), vec![], false),
                    usage: None,
                }),
                _ if rows[which].len() > 1 => {
                    let to = rows[which][1 + next(rows[which].len() - 1)];
                    Event::Rewound { agent, to }
                }
                _ => native(Entry::Notice {
                    notice: Notice::Restarted,
                }),
            };
            let event = match event {
                Event::Native { entry, .. } => Event::Native { agent, entry },
                event => event,
            };
            let pos = write
                .append(
                    workset,
                    &Record {
                        at: UnixMs(1),
                        event,
                    },
                )
                .unwrap();
            rows[which].push(pos);
        }
        write.commit();
        let read = db.read();
        let end = rho_agent::store::StoreRead::workset(&read, workset)
            .unwrap()
            .next;
        for agent in &agents {
            for pos in 0..=end.0 {
                let pos = Wpos(pos);
                assert_eq!(
                    crate::transcript::carry_before(&read, *agent, pos),
                    carry_by_scan(&read, *agent, pos),
                    "agent {} before {pos:?}",
                    agent.encoded()
                );
            }
        }
    }

    #[test]
    fn tool_detail_reads_the_complete_host_record() {
        let result = rho_agent_types::transcript::ToolResult {
            call_id: rho_agent_types::transcript::ExecId::try_from("call-1").unwrap(),
            tool_type: rho_agent_types::transcript::ToolType::Custom,
            body: rho_agent_types::transcript::ToolOutput {
                output: std::sync::Arc::new("bounded model view".to_owned()),
                full_output: Some(std::sync::Arc::new("complete host record".to_owned())),
                images: std::sync::Arc::new(Vec::new()),
                status: rho_agent_types::ToolOutputStatus::Success,
            },
            started_at: rho_agent_types::UnixMs(1),
            finished_at: rho_agent_types::UnixMs(2),
        };

        assert_eq!(detail_result(&result).output, "complete host record");
    }
}
