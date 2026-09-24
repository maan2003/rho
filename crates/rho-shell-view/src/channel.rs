//! Dialing a shell on a host.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use futures::SinkExt as _;
use futures::channel::mpsc as futures_mpsc;
use rho_rpc::parts::{Call, Opened, read_frame, write_frame, write_open};

use crate::protocol::{
    Open, ShellClientFrame, ShellClose, ShellList, ShellServerFrame, ShellStart,
};

/// Starts the agent's shell on the host `link` reaches when none runs,
/// then attaches.
pub fn open(
    link: &rho_hosts::Link,
    agent: String,
) -> impl Future<Output = anyhow::Result<ShellChannel>> + Send + 'static {
    link.run(|dialer| start_and_dial_shell(dialer, agent))
}

/// Gracefully closes the agent's persistent shell.
pub fn close(
    link: &rho_hosts::Link,
    agent: String,
) -> impl Future<Output = anyhow::Result<()>> + Send + 'static {
    link.run(|dialer| async move { call(&dialer, ShellClose { agent }).await })
}

/// One call on a stream of its own. A refusal is an error.
async fn call<C: Call>(dialer: &rho_hosts::Dialer, call: C) -> anyhow::Result<C::Reply> {
    let mut stream = dialer.open(C::PRIORITY).await?;
    rho_rpc::parts::call(&mut stream, call).await
}

/// One attachment to an agent's daemon-owned Comint-style shell. Dropping
/// `input` detaches this GUI but does not stop the shell process.
pub struct ShellChannel {
    pub frames: futures_mpsc::Receiver<ShellServerFrame>,
    pub submit: tokio::sync::mpsc::Sender<ShellSubmission>,
    pub control: tokio::sync::mpsc::Sender<ShellClientFrame>,
}

pub struct ShellSubmission {
    pub command: String,
    pub accepted: tokio::sync::oneshot::Sender<u64>,
}

/// Starts the agent's shell when none runs, then attaches.
async fn start_and_dial_shell(
    dialer: rho_hosts::Dialer,
    agent: String,
) -> anyhow::Result<ShellChannel> {
    let list = ShellList {
        agent: Some(agent.clone()),
    };
    if call(&dialer, list).await?.is_empty() {
        let start = ShellStart {
            agent: agent.clone(),
        };
        call(&dialer, start).await?;
    }
    dial_shell(dialer, agent).await
}

async fn dial_shell(dialer: rho_hosts::Dialer, agent: String) -> anyhow::Result<ShellChannel> {
    // Interactive streams outrank calls and sessions (priority 1 and below).
    let mut stream = dialer.open(Some(50)).await?;
    write_open(&mut stream, &Open::Attach { agent }).await?;
    if let Opened::Refused { reason } = read_frame(&mut stream).await? {
        anyhow::bail!("{reason}")
    }

    let (mut reader, mut writer) = tokio::io::split(stream);
    let (mut frames_tx, frames_rx) = futures_mpsc::channel(32);
    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::channel::<ShellSubmission>(8);
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<ShellClientFrame>(8);
    let pending = Arc::new(Mutex::new(
        HashMap::<u64, tokio::sync::oneshot::Sender<u64>>::new(),
    ));
    let reader_pending = Arc::clone(&pending);
    tokio::spawn(async move {
        while let Ok(frame) = read_frame::<_, ShellServerFrame>(&mut reader).await {
            match frame {
                ShellServerFrame::Accepted {
                    submission,
                    execution,
                } => {
                    let accepted = reader_pending.lock().unwrap().remove(&submission);
                    if let Some(accepted) = accepted {
                        let _ = accepted.send(execution);
                    }
                }
                frame => {
                    if frames_tx.send(frame).await.is_err() {
                        break;
                    }
                }
            }
        }
        reader_pending.lock().unwrap().clear();
    });
    tokio::spawn(async move {
        let mut next_submission = 1_u64;
        loop {
            let result = tokio::select! {
                biased;
                Some(frame) = control_rx.recv() => write_frame(&mut writer, &frame).await,
                Some(submission) = submit_rx.recv() => {
                    let submission_id = next_submission;
                    next_submission = next_submission.wrapping_add(1).max(1);
                    pending.lock().unwrap().insert(submission_id, submission.accepted);
                    let result = write_frame(
                        &mut writer,
                        &ShellClientFrame::Submit {
                            submission: submission_id,
                            command: submission.command,
                        },
                    )
                    .await;
                    if result.is_err() {
                        pending.lock().unwrap().remove(&submission_id);
                    }
                    result
                }
                else => break,
            };
            if result.is_err() {
                break;
            }
        }
        pending.lock().unwrap().clear();
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
    });
    Ok(ShellChannel {
        frames: frames_rx,
        submit: submit_tx,
        control: control_tx,
    })
}
