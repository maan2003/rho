//! The chat part of the agent host ([`rho_chat::protocol`]): a session
//! carries the host's log from where a client holds it; what the user does
//! in the chat is a call on a stream of its own.

use std::sync::Arc;

use rho_agent::store::{Appended, ChatRead as _};
use rho_agent_types::ChatSeq;
use rho_chat::protocol::{ClientFrame, Leave, Name, Open, Post, Reparent, Request, ServerFrame};
use rho_db::RhoDb;
use rho_rpc::protocol::{Answer, Call, write_frame};
use tokio::sync::{broadcast, mpsc};

use crate::{Services, prepare_image_content};

/// How many events travel in one frame while a client is catching up, so
/// a cold copy goes in pages the connection can interleave.
const LOG_PAGE: usize = 512;

/// Serves one chat stream, whichever kind its opening frame asked for.
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
    let head = services.db.read().chat_head();
    let _ = outgoing_tx.send(ServerFrame::Hello { head });
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
    loop {
        if let Some(sent) = sent.as_mut()
            && !send_from(&services.db, &outgoing_tx, sent).await
        {
            return;
        }
        tokio::select! {
            appended = feed.recv() => match appended {
                Ok(Appended::Chat(_)) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Ok(Appended::Row { .. } | Appended::WorksetDeleted(_)) => {}
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
        Request::Post(call) => {
            respond(
                writer,
                call,
                |Post {
                     client_id,
                     parent,
                     mentions,
                     mut content,
                 }| async move {
                    prepare_image_content(&mut content).await?;
                    services
                        .pool
                        .post_user(client_id, parent, mentions, content)
                        .await
                },
            )
            .await
        }
        Request::Reparent(call) => {
            respond(writer, call, |Reparent { message, parent }| async move {
                services.pool.reparent_user(message, parent).await
            })
            .await
        }
        Request::Leave(call) => {
            respond(writer, call, |Leave { thread, agent }| async move {
                services.pool.leave_user(thread, agent).await
            })
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
