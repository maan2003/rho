//! The mail part of the agent host ([`rho_chat::protocol`]): a session
//! carries the host's log from where a client holds it; what the user does
//! in the chat is a call on a stream of its own.

use std::sync::Arc;

use anyhow::Context as _;
use rho_agent::host::pool::{ProjectIdea, ProjectIdeaResult};
use rho_agent::store::{Appended, MailRead as _, ProjectRead as _};
use rho_agent_types::ChatSeq;
use rho_chat::protocol::{
    ChangeProject, ClientFrame, CreateProject, Name, Open, Post, PostIdea, PostIdeaReply, Project,
    Request, ResolveProject, RetryIdea, ServerFrame,
};
use rho_db::RhoDb;
use rho_rpc::protocol::{Answer, Call, write_frame};
use tokio::sync::{broadcast, mpsc};

use crate::{Services, prepare_image_content};

/// How many events travel in one frame while a client is catching up, so
/// a cold copy goes in pages the connection can interleave.
const LOG_PAGE: usize = 512;

/// Serves one mail stream, whichever kind its opening frame asked for.
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

/// A client's session: `Hello`, then from its `Follow` on every event
/// past what it holds, as the log grows. The feed is subscribed before the
/// head is read, so nothing that lands between is lost; `since` drops what
/// the client has.
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
    let feed = rho_agent::store::feed(&services.db);
    let head = services.db.read().mail_head();
    let _ = outgoing_tx.send(ServerFrame::Hello {
        head,
        host: services.endpoint,
    });
    let (follow_tx, follow_rx) = mpsc::unbounded_channel();
    let follower = tokio::spawn(follow(
        Arc::clone(&services),
        outgoing_tx.clone(),
        feed,
        follow_rx,
    ));
    let result = loop {
        let frame =
            match rho_rpc::protocol::read_frame_optional::<_, ClientFrame>(&mut reader).await {
                Ok(Some(frame)) => frame,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
        match frame {
            ClientFrame::Follow { since } => {
                let _ = follow_tx.send(since);
            }
        }
    };
    follower.abort();
    writer_task.abort();
    result
}

/// Sends the log past `since` as it lands. Records come from the store,
/// never from the feed: an append only says something landed, and a lagged
/// feed loses nothing.
async fn follow(
    services: Arc<Services>,
    outgoing_tx: mpsc::UnboundedSender<ServerFrame>,
    mut feed: broadcast::Receiver<Appended>,
    mut asks: mpsc::UnboundedReceiver<ChatSeq>,
) {
    let mut sent: Option<ChatSeq> = None;
    let mut work_changed = services.pool.watch_thread_work();
    let mut projects_changed = services.pool.watch_projects();
    let mut send_projects = true;
    let mut send_work = true;
    let mut last_work = None;
    loop {
        if let Some(sent) = sent.as_mut()
            && !send_from(&services.db, &outgoing_tx, sent).await
        {
            return;
        }
        if sent.is_some() && send_work {
            work_changed.borrow_and_update();
            let threads = services.pool.thread_work();
            if last_work.as_ref() != Some(&threads) {
                if outgoing_tx
                    .send(ServerFrame::Work {
                        threads: threads.clone(),
                    })
                    .is_err()
                {
                    return;
                }
                last_work = Some(threads);
            }
            send_work = false;
        }
        if sent.is_some() && send_projects {
            projects_changed.borrow_and_update();
            let configured = services.db.read().configured_projects();
            if outgoing_tx
                .send(ServerFrame::Projects { configured })
                .is_err()
            {
                return;
            }
            send_projects = false;
        }
        tokio::select! {
            changed = projects_changed.changed() => {
                if changed.is_err() { return; }
                send_projects = true;
            }
            changed = work_changed.changed() => {
                if changed.is_err() {
                    return;
                }
                send_work = true;
            }
            appended = feed.recv() => match appended {
                Ok(Appended::Mail(_) | Appended::WorksetDeleted(_)) | Err(broadcast::error::RecvError::Lagged(_)) => send_work = true,
                Ok(Appended::Row { .. }) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
            ask = asks.recv() => match ask {
                None => return,
                Some(since) => sent = Some(since),
            },
        }
    }
}

/// Pages the log out from after `sent`, moving it as it goes. False when
/// the connection is gone.
async fn send_from(
    db: &RhoDb,
    outgoing_tx: &mpsc::UnboundedSender<ServerFrame>,
    sent: &mut ChatSeq,
) -> bool {
    loop {
        let records = db.read().mail_since(*sent, LOG_PAGE);
        let Some(last) = records.last() else {
            return true;
        };
        *sent = last.seq;
        if outgoing_tx.send(ServerFrame::Messages { records }).is_err() {
            return false;
        }
        // Catching up must never starve the connection's own traffic.
        tokio::task::yield_now().await;
    }
}

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

/// One mail request.
async fn serve_call<W>(
    services: &Arc<Services>,
    request: Request,
    writer: &mut W,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match request {
        Request::ResolveProject(call) => {
            respond(writer, call, |ResolveProject { name }| async move {
                let name = rho_agent_types::one_line(&name);
                anyhow::ensure!(!name.is_empty(), "a project needs a name");
                let read = services.db.read();
                Ok(read.project_named(&name).and_then(|root| {
                    Some(Project {
                        root,
                        name: read.thread(root)?.name,
                    })
                }))
            })
            .await
        }
        Request::CreateProject(call) => {
            respond(writer, call, |CreateProject { name, config }| async move {
                services.pool.create_project(name, config).await
            })
            .await
        }
        Request::ChangeProject(call) => {
            respond(
                writer,
                call,
                |ChangeProject { project, change }| async move {
                    services.pool.change_project(project, change).await
                },
            )
            .await
        }
        Request::PostIdea(call) => {
            respond(
                writer,
                call,
                |PostIdea {
                     client_id,
                     project,
                     runtime,
                     mut content,
                     host_label,
                 }| async move {
                    prepare_image_content(&mut content).await?;
                    let pool = services.pool.clone();
                    // The connection does not own the start: after a drop the
                    // outbox repeats the same client id and learns its receipt.
                    let result = tokio::spawn(async move {
                        pool.post_project_idea(ProjectIdea {
                            client_id,
                            project,
                            parent: project.0,
                            failed_start: None,
                            content,
                            host_label,
                            runtime,
                        })
                        .await
                    })
                    .await??;
                    Ok(match result {
                        ProjectIdeaResult::Posted(id) => PostIdeaReply::Posted(id),
                        ProjectIdeaResult::RuntimeRefused(reason) => {
                            PostIdeaReply::RuntimeRefused(reason)
                        }
                    })
                },
            )
            .await
        }
        Request::RetryIdea(call) => {
            respond(
                writer,
                call,
                |RetryIdea {
                     client_id,
                     failed_start,
                     mut content,
                     host_label,
                 }| async move {
                    let (parent, project) = {
                        let read = services.db.read();
                        let stop = read
                            .message(failed_start)
                            .filter(|message| message.from == rho_agent_types::Address::StartFailed)
                            .context("retry needs a failed start")?;
                        let parent = read.mail_thread(&stop);
                        let project = rho_agent_types::ProjectId(
                            read.root_of(parent).context("no such thread")?,
                        );
                        (parent, project)
                    };
                    prepare_image_content(&mut content).await?;
                    let pool = services.pool.clone();
                    let result = tokio::spawn(async move {
                        pool.post_project_idea(ProjectIdea {
                            client_id,
                            project,
                            parent,
                            failed_start: Some(failed_start),
                            content,
                            host_label,
                            runtime: None,
                        })
                        .await
                    })
                    .await??;
                    match result {
                        ProjectIdeaResult::Posted(id) => Ok(id),
                        ProjectIdeaResult::RuntimeRefused(reason) => anyhow::bail!("{reason}"),
                    }
                },
            )
            .await
        }
        Request::Post(call) => {
            respond(
                writer,
                call,
                |Post {
                     client_id,
                     parent,
                     settles,
                     mentions,
                     mut content,
                 }| async move {
                    prepare_image_content(&mut content).await?;
                    services
                        .pool
                        .post_user(client_id, parent, settles, mentions, content)
                        .await
                },
            )
            .await
        }
        Request::Name(call) => {
            respond(writer, call, |Name { thread, name }| async move {
                services.pool.name_user(thread, name).await
            })
            .await
        }
    }
}
