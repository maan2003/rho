//! The agents part of the daemon, and the parts about one agent's workset:
//! its terminals, shells and workspace files. The agents session carries
//! the journal, the live tails, new agents and the quota; requests,
//! terminals, shells and workspace channels are streams of their own.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use rho_agent::db::{AgentReadTxnExt as _, AgentWriteTxnExt as _};
use rho_agent_types::{AgentId, MessageDelivery, Seq, WorkspaceInfo};
use rho_agents_client::protocol::{
    AgentCommand, AgentCostDistribution, ClaudeAccountList, ClaudeAccounts, ClientFrame,
    GlobalUsage, NewAgent, Open, QuotaHistory, QuotaUsage, RecordVisualization, Request,
    ServerFrame, SetAuthAccountEnabled, SetClaudeAccount, Visualization, VisualizationContent,
};
use rho_db::RhoDb;
use rho_rpc::parts::{Answer, Call, Opened, write_frame};
use rho_shell_view::protocol as shell;
use rho_terminal::protocol as term;
use tokio::sync::{broadcast, mpsc};

use crate::{
    NEXT_CONNECTION_ID, Services, open_checkout, prepare_image_content, usage, workspace_channel,
};

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
        let frame = match rho_rpc::parts::read_frame_optional::<_, ClientFrame>(&mut reader).await {
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
                    since,
                ));
            }
            ClientFrame::Focus { agent_ids } => {
                if agent_ids.len() > 64 {
                    break Err(anyhow::anyhow!("too many focused agents"));
                }
                // Focus is what this client is looking at, nothing more: it
                // never loads an agent. The pool unions it across streams
                // into the live set; a loaded agent in it tells its tail.
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

/// Rows are read from the journal, never from the feed: an append only
/// says a row landed, and the connection pages the journal from the last
/// seq it sent. A lagged subscription does the same. Rows the transcript
/// leaves behind (`strip` says nothing) advance the seq without a message.
///
/// Each connection tells each loop's tail itself, from the loop's status:
/// a teller that has told nothing tells the tail whole, so a connection
/// that just caught up, or lost statuses to a lag, starts from nothing.
/// The loops are asked for their status once caught up, so an idle one
/// is told too.
fn spawn_log_follow(
    services: Arc<Services>,
    outgoing_tx: mpsc::UnboundedSender<rho_agents_client::protocol::ServerFrame>,
    since: Seq,
) -> tokio::task::JoinHandle<()> {
    use rho_agent::journal::Feed;
    tokio::spawn(async move {
        // Subscribed before the catch-up read, so a row appended during it
        // is queued rather than lost; the seq drops the duplicates.
        let mut feed = rho_agent::journal::feed(&services.db);
        let mut sent = since;
        if !send_journal_from(&services.db, &outgoing_tx, &mut sent).await {
            return;
        }
        let mut tellers = HashMap::<AgentId, crate::live::Teller>::new();
        services.pool.tell_tails().await;
        loop {
            match feed.recv().await {
                Ok(Feed::Status {
                    agent_id,
                    status,
                    queue,
                    reset,
                }) => {
                    let teller = tellers.entry(agent_id).or_default();
                    if reset {
                        teller.reset();
                    }
                    let queue = queue.and_then(|queue| {
                        let items = queue
                            .iter()
                            .map(crate::live::queued_item)
                            .collect::<Vec<_>>();
                        teller.tell_queue(&items)
                    });
                    for live in queue.into_iter().chain(teller.tell(&status.kind)) {
                        if outgoing_tx
                            .send(rho_agents_client::protocol::ServerFrame::Live { agent_id, live })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Ok(Feed::Appended(appended)) => {
                    if appended.seq > sent
                        && !send_journal_from(&services.db, &outgoing_tx, &mut sent).await
                    {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !send_journal_from(&services.db, &outgoing_tx, &mut sent).await {
                        return;
                    }
                    tellers.clear();
                    services.pool.tell_tails().await;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

/// Pages the journal out from after `sent`, moving it as it goes. False
/// when the connection is gone.
async fn send_journal_from(
    db: &RhoDb,
    outgoing_tx: &mpsc::UnboundedSender<rho_agents_client::protocol::ServerFrame>,
    sent: &mut rho_agent_types::Seq,
) -> bool {
    loop {
        let page = db.read().journal_since(*sent, LOG_PAGE);
        let Some((last, _, _, _)) = page.last() else {
            return true;
        };
        *sent = *last;
        let entries = page
            .into_iter()
            .filter_map(|(seq, agent_id, pos, event)| {
                Some(rho_agents_client::protocol::transcript::LogEntry {
                    seq,
                    agent_id,
                    pos: pos.into(),
                    event: crate::transcript::strip(&event)?,
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
        mode,
        mut content,
    } = new;
    if let Some(content) = content.as_mut() {
        prepare_image_content(content).await?;
    }
    let (agent_id, agent) = services.create(role, start, mode).await?;
    if let Some(content) = content {
        // The agent is fresh, so the lanes are equivalent here.
        agent
            .send_user_content_accepted(content, MessageDelivery::NextRequest)
            .await?;
    }
    Ok(agent_id)
}

async fn handle_agent_command(
    services: &Arc<Services>,
    command: AgentCommand,
) -> anyhow::Result<()> {
    match command {
        AgentCommand::Send {
            agent_id,
            mut content,
            delivery,
        } => {
            prepare_image_content(&mut content).await?;
            let (_, agent, _) = services.load(agent_id).await?;
            // What Rho has to tell the agent goes ahead of the person's
            // words, once: the loop's head forgets it as soon as the
            // message is accepted, the log when the message's row lands.
            let notice = agent.head().pending_notice;
            if let Some(text) = notice.clone() {
                content.insert(0, rho_agent_types::ContentPart::Text { text });
            }
            agent.send_user_content_accepted(content, delivery).await?;
            if notice.is_some() {
                agent.notice_carried();
            }
        }
        // A compaction rides the next request whichever lane the client
        // named; the lane is not a thing the runtime reads for it.
        AgentCommand::Compact {
            agent_id,
            delivery: _,
        } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.compact();
        }
        AgentCommand::ChangeRole { agent_id, role } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.change_role(role).await?;
        }
        AgentCommand::ChangeMode { agent_id, mode } => {
            let changed = services.pool.change_mode(agent_id, mode).await?;
            for id in changed {
                if id != agent_id && services.pool.is_live(id) {
                    services.load(id).await?;
                }
            }
            // Back at once, in the new view, for whoever is looking.
            services.load(agent_id).await?;
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

/// Attaches a dedicated Comint-style shell stream. The daemon retains the
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

/// Serves a stream dedicated to one daemon-owned terminal: spawns or attaches
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

/// The daemon's terminals, or one agent's.
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

/// Serves a bounded typed file channel rooted in one workspace checkout.
pub(crate) async fn serve_workspace_channel<R, W>(
    services: Arc<Services>,
    mut reader: R,
    mut writer: W,
    workspace: WorkspaceInfo,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let checkout = match open_checkout(&services, &workspace).await {
        Ok((_, checkout)) => checkout,
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
    let files = match workspace_channel::WorkspaceFiles::open(checkout) {
        Ok(files) => Arc::new(files),
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
    let watcher_setup = match files.start_watcher() {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: format!("watch workspace: {error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    write_frame(&mut writer, &Opened::Ready).await?;

    use rho_files::protocol::{WorkspaceClientFrame, WorkspaceServerFrame};
    let mut changes = watcher_setup.changes;
    let changes_overflowed = watcher_setup.overflowed;
    let mut watcher_ready = Some(watcher_setup.ready);
    // Keep the watcher alive after its asynchronous directory registration
    // completes. The leading underscore documents that ownership is the only
    // purpose of this value.
    let mut _watcher = None;
    let mut pending_watch_directories = std::collections::BTreeSet::<camino::Utf8PathBuf>::new();
    loop {
        tokio::select! {
            result = async { watcher_ready.as_mut().expect("watcher setup is enabled").await }, if watcher_ready.is_some() => {
                // Drop the completed JoinHandle before retaining its watcher.
                watcher_ready.take();
                match result {
                    Ok(Ok(watcher)) => {
                        _watcher = Some(watcher);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "workspace watcher registration failed");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "workspace watcher registration task failed");
                    }
                }
                if let Some(watcher) = _watcher.as_mut() {
                    for directory in std::mem::take(&mut pending_watch_directories) {
                        if let Err(error) = files.watch_directory_tree(watcher, &directory) {
                            tracing::warn!(%directory, %error, "watch newly created workspace directory");
                        }
                    }
                }
                // A watcher cannot report changes made before its directory
                // registration completed. Treat that window like overflow; the
                // GUI already reconciles it by reloading open buffers and
                // scheduling a fresh semantic barrier.
                rho_rpc::parts::write_frame_limited(
                    &mut writer,
                    &WorkspaceServerFrame::Changed {
                        paths: Vec::new(),
                        rescan: true,
                    },
                    rho_files::protocol::MAX_WORKSPACE_FRAME_LEN,
                )
                .await?;
            }
            frame = rho_rpc::parts::read_frame_limited::<_, WorkspaceClientFrame>(
                &mut reader,
                rho_files::protocol::MAX_WORKSPACE_FRAME_LEN,
            ) => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) if error.chain().any(|cause| {
                        cause.downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof)
                    }) => return Ok(()),
                    Err(error) => return Err(error),
                };
                let response = match frame {
                    WorkspaceClientFrame::Open { request_id, path } => {
                        let result = files.read(path.clone()).await;
                        WorkspaceServerFrame::Opened { request_id, path, result }
                    }
                    WorkspaceClientFrame::Reload { request_id, path } => {
                        let result = files.read(path.clone()).await;
                        WorkspaceServerFrame::Reloaded { request_id, path, result }
                    }
                    WorkspaceClientFrame::Save { request_id, path, revision, contents } => {
                        let result = files.save(path.clone(), Some(revision), contents).await;
                        WorkspaceServerFrame::Saved { request_id, path, result }
                    }
                    WorkspaceClientFrame::Overwrite { request_id, path, contents } => {
                        let result = files.save(path.clone(), None, contents).await;
                        WorkspaceServerFrame::Saved { request_id, path, result }
                    }
                };
                rho_rpc::parts::write_frame_limited(
                    &mut writer,
                    &response,
                    rho_files::protocol::MAX_WORKSPACE_FRAME_LEN,
                )
                .await?;
            }
            Some(first) = changes.recv() => {
                let (paths, directories, explicit_rescan) =
                    workspace_channel::drain_changes(first, &mut changes);
                pending_watch_directories.extend(directories);
                if let Some(watcher) = _watcher.as_mut() {
                    for directory in std::mem::take(&mut pending_watch_directories) {
                        if let Err(error) = files.watch_directory_tree(watcher, &directory) {
                            tracing::warn!(%directory, %error, "watch newly created workspace directory");
                        }
                    }
                }
                let overflowed = changes_overflowed.swap(false, Ordering::AcqRel);
                let rescan = explicit_rescan || overflowed;
                rho_rpc::parts::write_frame_limited(
                    &mut writer,
                    &WorkspaceServerFrame::Changed { paths, rescan },
                    rho_files::protocol::MAX_WORKSPACE_FRAME_LEN,
                )
                .await?;
            }
        }
    }
}
/// The bodies one raw row carries, for a client that asked by position:
/// a request's tool results whole, or a response as the transcript draws
/// it. Rows a rewind hid still answer; the client asked for one it holds.
fn agent_detail(
    db: &RhoDb,
    agent_id: AgentId,
    pos: rho_agent_types::AgentPos,
) -> rho_agents_client::protocol::transcript::DetailBody {
    use rho_agents_client::protocol::transcript::DetailBody;
    let event = db.read().agent_event(agent_id, pos.into());
    if let Some(native) = event.as_ref().and_then(rho_agent::AgentEvent::native_event) {
        use rho_agent::native::NativeEvent;
        return match native {
            NativeEvent::RequestStarted { input, .. } => DetailBody::Results(
                input
                    .iter()
                    .flat_map(|item| match item {
                        rho_inference::types::ContextBlock::ToolResults { results } => {
                            results.iter().map(detail_result).collect::<Vec<_>>()
                        }
                        rho_inference::types::ContextBlock::ToolUpdate(update) => {
                            vec![detail_update(&update)]
                        }
                        _ => Vec::new(),
                    })
                    .collect(),
            ),
            NativeEvent::ResponseFinished { output, .. } => DetailBody::Response(
                output
                    .iter()
                    .filter_map(|entry| match entry {
                        rho_inference::types::ContextBlock::InferenceResponse { items, .. } => {
                            Some(items)
                        }
                        _ => None,
                    })
                    .flatten()
                    .filter_map(crate::transcript::item)
                    .collect(),
            ),
            NativeEvent::RequestFailed { partial, .. } => DetailBody::Response(
                partial
                    .items
                    .iter()
                    .filter_map(|slot| match slot {
                        rho_inference::types::StreamingContextItemState::Pending(item)
                        | rho_inference::types::StreamingContextItemState::Finished(item) => item
                            .to_context_item()
                            .ok()
                            .and_then(|item| crate::transcript::item(&item)),
                        _ => None,
                    })
                    .collect(),
            ),
        };
    }
    match event {
        Some(rho_agent::AgentEvent::Transcript { line, .. }) => match line {
            rho_agent::TranscriptLine::Assistant { text, calls, .. } => DetailBody::Response(
                (!text.is_empty())
                    .then_some(rho_agents_client::protocol::transcript::Item::Text {
                        text,
                        phase: None,
                    })
                    .into_iter()
                    .chain(calls.into_iter().map(|call| {
                        rho_agents_client::protocol::transcript::Item::ToolCall {
                            id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                            format: rho_agents_client::protocol::transcript::ArgumentsFormat::Json,
                        }
                    }))
                    .collect(),
            ),
            rho_agent::TranscriptLine::ToolResults { results } => {
                DetailBody::Results(results.iter().map(detail_result).collect())
            }
            rho_agent::TranscriptLine::User { .. }
            | rho_agent::TranscriptLine::Compacted { .. } => DetailBody::Nothing,
        },
        Some(rho_agent::AgentEvent::Failed { partial, .. }) => DetailBody::Response(
            partial
                .items
                .iter()
                .filter_map(|slot| match slot {
                    rho_inference::types::StreamingContextItemState::Pending(item)
                    | rho_inference::types::StreamingContextItemState::Finished(item) => {
                        crate::live::to_item(item)
                    }
                    rho_inference::types::StreamingContextItemState::Empty => None,
                })
                .collect(),
        ),
        _ => DetailBody::Nothing,
    }
}

fn detail_result(
    result: &rho_inference::types::ToolResult,
) -> rho_agents_client::protocol::transcript::DetailResult {
    use rho_agents_client::protocol::transcript::ToolStatus;
    rho_agents_client::protocol::transcript::DetailResult {
        id: result.call_id.as_str().to_owned(),
        status: match result.body.status {
            rho_agent_types::ToolOutputStatus::Success => ToolStatus::Success,
            rho_agent_types::ToolOutputStatus::Error => ToolStatus::Error,
            rho_agent_types::ToolOutputStatus::Cancelled => ToolStatus::Cancelled,
        },
        output: result.body.recorded_output().to_owned(),
        error: None,
    }
}

fn detail_update(
    update: &rho_inference::types::ToolUpdate,
) -> rho_agents_client::protocol::transcript::DetailResult {
    rho_agents_client::protocol::transcript::DetailResult {
        id: update.call_id.as_str().to_owned(),
        status: rho_agents_client::protocol::transcript::ToolStatus::Success,
        output: update.recorded_output().to_owned(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{detail_result, detail_update};

    #[test]
    fn tool_detail_reads_the_complete_host_record() {
        let result = rho_inference::types::ToolResult {
            call_id: rho_inference::types::ToolCallId::try_from("call-1").unwrap(),
            tool_type: rho_inference::types::ToolType::Custom,
            body: rho_inference::types::ToolOutput {
                output: Arc::new("bounded model view".to_owned()),
                full_output: Some(Arc::new("complete host record".to_owned())),
                images: Arc::new(Vec::new()),
                status: rho_agent_types::ToolOutputStatus::Success,
            },
            started_at: rho_agent_types::UnixMs(1),
            finished_at: rho_agent_types::UnixMs(2),
            metadata: None,
        };

        assert_eq!(detail_result(&result).output, "complete host record");

        let update = rho_inference::types::ToolUpdate {
            status: None,
            images: Default::default(),
            call_id: rho_inference::types::ToolCallId::try_from("call-1").unwrap(),
            tool_type: rho_inference::types::ToolType::Custom,
            output: Arc::new("bounded update".to_owned()),
            full_output: Some(Arc::new("complete update".to_owned())),
            at: rho_agent_types::UnixMs(3),
        };
        assert_eq!(detail_update(&update).output, "complete update");
    }
}
