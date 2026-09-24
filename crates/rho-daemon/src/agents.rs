//! The agents part of the daemon: every stream opened by
//! [`rho_agent_host_proto::Open::Agents`]. Its session carries the journal,
//! the live tails, new agents and the quota; requests, terminals, shells
//! and workspace channels are streams of their own.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use rho_agent::MessageDelivery;
use rho_agent::db::{AgentId, AgentReadTxnExt as _, AgentWriteTxnExt as _};
use rho_agent_host_proto::agents::{ClientFrame, Open, Reply, Request, ServerFrame};
use rho_agent_host_proto::{AgentCommand, Opened, WorkspaceInfo, write_frame};
use rho_db::RhoDb;
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
        Open::Request(request) => {
            let reply = match handle_request(&services, request).await {
                Ok((reply, refresh)) => {
                    if let Refresh::Ready = refresh {
                        // Registry changes show on every client (GUI rails
                        // and a waiting CLI), so the refreshed snapshot goes
                        // through the daemon-wide event fanout.
                        let _ = services.events.send(services.ready_message().await);
                    }
                    reply
                }
                // The whole chain, not just the outermost context: a new
                // agent that failed said "create managed workspace" and
                // kept the reason to itself, which is not something a
                // reader can act on.
                Err(error) => Reply::Failed {
                    reason: format!("{error:#}"),
                },
            };
            write_frame(&mut writer, &reply).await
        }
        Open::Terminal {
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
        Open::Shell { agent } => serve_shell(services, reader, writer, agent).await,
        Open::Workspace { workspace } => {
            serve_workspace_channel(services, reader, writer, workspace).await
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
    let journal_head = services.db.read().journal_head();
    let _ = outgoing_tx.send(ServerFrame::JournalHead {
        machine_seed: services.machine_seed,
        journal_head,
    });
    // Agents made anywhere, by clients or by agents spawning children. The
    // journal carries them whole; this only says which are new.
    let created_task = {
        let mut created = services.pool.subscribe_created();
        let outgoing_tx = outgoing_tx.clone();
        tokio::spawn(async move {
            loop {
                match created.recv().await {
                    Ok(created) => {
                        let frame = ServerFrame::AgentCreated {
                            agent_id: created.agent_id,
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
            match rho_agent_host_proto::read_frame_optional::<_, ClientFrame>(&mut reader).await {
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
    created_task.abort();
    quota_task.abort();
    services
        .pool
        .set_live_wants(stream_id, HashSet::new())
        .await;
    writer_task.abort();
    result
}

/// Contiguous by seq is the whole contract for rows. The daemon remembers
/// the last seq it sent; an append that is not the next one, or a lagged
/// subscription, sends it back to the journal from there. Rows the
/// mirror leaves behind (`strip` says nothing) advance the seq without a
/// message.
///
/// Live deltas are forwarded only once the loops have been asked to tell
/// their tails whole, which happens after the catch-up: a delta from
/// before that would be an append to a tail the client does not hold.
/// After a lag the same is done again, since deltas were lost.
fn spawn_log_follow(
    services: Arc<Services>,
    outgoing_tx: mpsc::UnboundedSender<rho_agent_host_proto::agents::ServerFrame>,
    since: rho_agent_host_proto::transcript::Seq,
) -> tokio::task::JoinHandle<()> {
    use rho_agent::transcript::Feed;
    tokio::spawn(async move {
        // Subscribed before the catch-up read, so a row appended during it
        // is queued rather than lost; the seq drops the duplicates.
        let mut feed = rho_agent::transcript::feed(&services.db);
        let mut sent = since;
        if !send_journal_from(&services.db, &outgoing_tx, &mut sent).await {
            return;
        }
        let mut told = false;
        services.pool.tell_tails().await;
        loop {
            match feed.recv().await {
                Ok(Feed::Live { agent_id, live }) => {
                    // The first whole tell for a loop starts with a phase
                    // (`Requesting`, `Waiting`, `Idle`); anything before
                    // one is from before the ask and is dropped.
                    if !told {
                        told = !matches!(
                            live,
                            rho_agent_host_proto::transcript::Live::Item { .. }
                                | rho_agent_host_proto::transcript::Live::Appended { .. }
                        );
                        if !told {
                            continue;
                        }
                    }
                    if outgoing_tx
                        .send(rho_agent_host_proto::agents::ServerFrame::Live { agent_id, live })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Feed::Appended(appended)) => {
                    if appended.seq <= sent {
                        continue;
                    }
                    if appended.seq != sent.next() {
                        if !send_journal_from(&services.db, &outgoing_tx, &mut sent).await {
                            return;
                        }
                        continue;
                    }
                    sent = appended.seq;
                    if let Some(entry) = appended.entry()
                        && outgoing_tx
                            .send(rho_agent_host_proto::agents::ServerFrame::Log {
                                entries: vec![entry],
                            })
                            .is_err()
                    {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !send_journal_from(&services.db, &outgoing_tx, &mut sent).await {
                        return;
                    }
                    told = false;
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
    outgoing_tx: &mpsc::UnboundedSender<rho_agent_host_proto::agents::ServerFrame>,
    sent: &mut rho_agent_host_proto::transcript::Seq,
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
                Some(rho_agent_host_proto::transcript::LogEntry {
                    seq,
                    agent_id,
                    pos: pos.into(),
                    event: rho_agent::transcript::strip(&event)?,
                })
            })
            .collect::<Vec<_>>();
        if !entries.is_empty()
            && outgoing_tx
                .send(rho_agent_host_proto::agents::ServerFrame::Log { entries })
                .is_err()
        {
            return false;
        }
        // Catching up must never starve the connection's own traffic.
        tokio::task::yield_now().await;
    }
}

/// Whether a handled request changed registry state that clients see through
/// `Ready` (agents and workdirs); `Ready` refreshes every control stream,
/// so all clients converge on the change at once.
enum Refresh {
    Ready,
    None,
}

/// One request stream's request. `Err` becomes a [`Reply::Failed`].
async fn handle_request(
    services: &Arc<Services>,
    request: Request,
) -> anyhow::Result<(Reply, Refresh)> {
    let reply = match request {
        Request::Command(command) => return handle_agent_command(services, command).await,
        Request::ClaudeAccounts => claude_accounts_message(&services.db, &services.claude)?,
        Request::SetClaudeAccount { name } => {
            // The account has to be there before an agent tries to mount it;
            // a switch to a name with no directory would fail at the next
            // turn of every agent at once.
            services.claude.bootstrap(&name)?;
            let mut write = services.db.write().await;
            write.set_claude_account(&name);
            write.commit();
            claude_accounts_message(&services.db, &services.claude)?
        }
        Request::SetAuthAccountEnabled { name, enabled } => {
            services.set_auth_account_enabled(&name, enabled).await;
            Reply::Done
        }
        Request::Visualization { id } => {
            let visualization = services
                .visualizations
                .get(&id)
                .with_context(|| format!("visualization {id} does not exist"))?;
            Reply::Visualization {
                id,
                mime_type: visualization.mime_type,
                content: visualization.content,
            }
        }
        Request::RecordVisualization { mime_type, content } => {
            let id = services.visualizations.record(mime_type, content).await?;
            Reply::VisualizationRecorded { id }
        }
        Request::QuotaUsage => Reply::QuotaUsage {
            summaries: usage::quota_summaries(&services.db, &services.inference),
        },
        Request::QuotaHistory => Reply::QuotaHistory {
            series: usage::quota_history(&services.db, &services.inference),
        },
        Request::GlobalUsage { since_ms } => {
            services.pool.flush_agent_usage(None).await;
            Reply::GlobalUsage {
                series: usage::global_usage(&services.db, since_ms),
            }
        }
        Request::AgentCostDistribution { since_ms } => {
            services.pool.flush_agent_usage(None).await;
            Reply::AgentCostDistribution {
                series: usage::agent_costs(&services.db, since_ms)?,
            }
        }
        Request::TerminalList { agent } => Reply::TerminalList {
            terminals: terminal_list(services, agent.as_deref()).await?,
        },
        Request::ShellStart { agent } => {
            shell_start(services, &agent).await?;
            Reply::Done
        }
        Request::ShellList { agent } => Reply::ShellList {
            shells: shell_list(services, agent.as_deref()).await?,
        },
        Request::ShellClose { agent } => {
            shell_close(services, &agent).await?;
            Reply::Done
        }
    };
    Ok((reply, Refresh::None))
}

async fn handle_agent_command(
    services: &Arc<Services>,
    command: AgentCommand,
) -> anyhow::Result<(Reply, Refresh)> {
    match command {
        AgentCommand::New {
            role,
            start,
            mode,
            mut content,
        } => {
            if let Some(content) = content.as_mut() {
                prepare_image_content(content).await?;
            }
            // Control streams hear of the agent from the pool's creation
            // broadcast; the reply tells this client which one is its own.
            let (agent_id, agent) = services.create(role, start, mode).await?;
            if let Some(content) = content {
                // The agent is fresh, so the lanes are equivalent here.
                agent
                    .send_user_content_accepted(content, MessageDelivery::NextRequest)
                    .await?;
            }
            Ok((Reply::AgentCreated { agent_id }, Refresh::Ready))
        }
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
                content.insert(0, rho_agent_host_proto::ContentPart::Text { text });
            }
            agent.send_user_content_accepted(content, delivery).await?;
            if notice.is_some() {
                agent.notice_carried();
            }
            Ok((Reply::Done, Refresh::None))
        }
        // A compaction rides the next request whichever lane the client
        // named; the lane is not a thing the runtime reads for it.
        AgentCommand::Compact {
            agent_id,
            delivery: _,
        } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.compact();
            Ok((Reply::Done, Refresh::None))
        }
        AgentCommand::ChangeRole { agent_id, role } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.change_role(role).await?;
            Ok((Reply::Done, Refresh::Ready))
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
            Ok((Reply::Done, Refresh::Ready))
        }
        AgentCommand::ChangePromptCacheKey { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.change_prompt_cache_key()?;
            Ok((Reply::Done, Refresh::None))
        }
        AgentCommand::Cancel { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.cancel();
            Ok((Reply::Done, Refresh::None))
        }
        AgentCommand::Rewind { agent_id, turns } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.rewind(turns).await?;
            Ok((Reply::Done, Refresh::Ready))
        }
        AgentCommand::Continue { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.retry();
            Ok((Reply::Done, Refresh::None))
        }
    }
}

fn claude_accounts_message(
    db: &RhoDb,
    claude: &rho_claude::accounts::ClaudePaths,
) -> anyhow::Result<Reply> {
    Ok(Reply::ClaudeAccounts {
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
    client.relay::<_, _, rho_agent_host_proto::shell::ShellClientFrame, rho_agent_host_proto::shell::ShellServerFrame>(reader, writer).await
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
) -> anyhow::Result<Vec<rho_agent_host_proto::shell::ShellInfo>> {
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
/// (per [`TerminalOpen`](rho_agent_host_proto::term::TerminalOpen)), replies
/// `Opened::Ready`, then pumps
/// [`rho_agent_host_proto::term`] frames until either side closes. Closing only
/// detaches; the terminal keeps running. A headless create replies and
/// returns without attaching.
#[expect(clippy::too_many_arguments)]
async fn serve_terminal<R, W>(
    services: Arc<Services>,
    reader: R,
    mut writer: W,
    agent: String,
    terminal_id: u64,
    open: rho_agent_host_proto::term::TerminalOpen,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let create = matches!(
        open,
        rho_agent_host_proto::term::TerminalOpen::Create { .. }
    );
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
        rho_agent_host_proto::term::TerminalOpen::Create { attach: false }
    ) {
        // Headless create: the terminal keeps running with no clients.
        return Ok(());
    }

    client
        .relay::<_, _, rho_agent_host_proto::term::TermClientFrame, rho_agent_host_proto::term::TermServerFrame>(
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
) -> anyhow::Result<Vec<rho_agent_host_proto::term::TerminalInfo>> {
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
async fn serve_workspace_channel<R, W>(
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

    use rho_agent_host_proto::workspace::{WorkspaceClientFrame, WorkspaceServerFrame};
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
                rho_agent_host_proto::write_frame_limited(
                    &mut writer,
                    &WorkspaceServerFrame::Changed {
                        paths: Vec::new(),
                        rescan: true,
                    },
                    rho_agent_host_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
                )
                .await?;
            }
            frame = rho_agent_host_proto::read_frame_limited::<_, WorkspaceClientFrame>(
                &mut reader,
                rho_agent_host_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
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
                rho_agent_host_proto::write_frame_limited(
                    &mut writer,
                    &response,
                    rho_agent_host_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
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
                rho_agent_host_proto::write_frame_limited(
                    &mut writer,
                    &WorkspaceServerFrame::Changed { paths, rescan },
                    rho_agent_host_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
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
    pos: rho_agent_host_proto::transcript::AgentPos,
) -> rho_agent_host_proto::transcript::DetailBody {
    use rho_agent_host_proto::transcript::DetailBody;
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
                    .filter_map(rho_agent::transcript::item)
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
                            .and_then(|item| rho_agent::transcript::item(&item)),
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
                    .then_some(rho_agent_host_proto::transcript::Item::Text { text, phase: None })
                    .into_iter()
                    .chain(calls.into_iter().map(|call| {
                        rho_agent_host_proto::transcript::Item::ToolCall {
                            id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                            format: rho_agent_host_proto::transcript::ArgumentsFormat::Json,
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
                        rho_agent::live::to_item(item)
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
) -> rho_agent_host_proto::transcript::DetailResult {
    use rho_agent_host_proto::transcript::ToolStatus;
    rho_agent_host_proto::transcript::DetailResult {
        id: result.call_id.as_str().to_owned(),
        status: match result.body.status {
            rho_agent_host_proto::ToolOutputStatus::Success => ToolStatus::Success,
            rho_agent_host_proto::ToolOutputStatus::Error => ToolStatus::Error,
            rho_agent_host_proto::ToolOutputStatus::Cancelled => ToolStatus::Cancelled,
        },
        output: result.body.recorded_output().to_owned(),
        error: None,
    }
}

fn detail_update(
    update: &rho_inference::types::ToolUpdate,
) -> rho_agent_host_proto::transcript::DetailResult {
    rho_agent_host_proto::transcript::DetailResult {
        id: update.call_id.as_str().to_owned(),
        status: rho_agent_host_proto::transcript::ToolStatus::Success,
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
                status: rho_agent_host_proto::ToolOutputStatus::Success,
            },
            started_at: rho_agent_host_proto::UnixMs(1),
            finished_at: rho_agent_host_proto::UnixMs(2),
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
            at: rho_agent_host_proto::UnixMs(3),
        };
        assert_eq!(detail_update(&update).output, "complete update");
    }
}
