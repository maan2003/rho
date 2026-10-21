//! The agent host on this machine, over its Unix socket: started when it is
//! not running, and stopped on the way out if this CLI started it and
//! nobody else uses it.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use rho_agents_client::protocol::{self as agents, ClientFrame, ServerFrame};
use rho_rpc::protocol::Call;
use rho_rpc::protocol::client::Client;

pub struct Host {
    socket: PathBuf,
    /// Whether this CLI started the host, and so may stop it.
    pub started: bool,
}

impl Host {
    /// The host behind `socket`, taken as running and not ours to stop.
    pub fn at(socket: PathBuf) -> Self {
        Self {
            socket,
            started: false,
        }
    }

    /// The host behind the socket, started beside this binary when nothing
    /// answers.
    pub async fn connect_or_start(socket: Option<PathBuf>) -> Result<Self> {
        let socket = rho_rpc::protocol::RuntimePaths::resolve(socket)?
            .socket()
            .to_owned();
        if let Ok(mut probe) = Client::connect(&socket).await {
            let _ = probe.shutdown().await;
            return Ok(Self {
                socket,
                started: false,
            });
        }
        let program = std::env::current_exe()?.with_file_name("rho-agent-host");
        // The terminal is ours, so what the host says goes to a file beside
        // its socket, read back if it does not come up.
        let log_path = socket.with_file_name("rho-agent-host.log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("create {}", log_path.display()))?;
        let mut child = std::process::Command::new(&program)
            .arg("--socket-path")
            .arg(&socket)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("start {}", program.display()))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(&socket).await {
                Ok(mut probe) => {
                    let _ = probe.shutdown().await;
                    return Ok(Self {
                        socket,
                        started: true,
                    });
                }
                Err(error) => {
                    let exited = child.try_wait()?;
                    if exited.is_none() && tokio::time::Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    let how = match exited {
                        Some(status) => format!("the agent host exited with {status}"),
                        None => "the agent host did not come up in 10 s".to_owned(),
                    };
                    return Err(error.context(format!(
                        "{how}; the end of {}:\n{}",
                        log_path.display(),
                        log_tail(&log_path)
                    )));
                }
            }
        }
    }

    /// One call on a stream of its own. A refusal is an error.
    pub async fn call<C: Call>(&self, call: C) -> Result<C::Reply> {
        rho_rpc::protocol::client::call(&self.socket, call).await
    }

    /// The agents session: the journal from `since`, then every append and
    /// the live tails of the focused agents. Frames arrive on the returned
    /// channel; what is sent on the other goes to the host. The task ends
    /// once the sender is dropped and the host has been told the session
    /// is over.
    pub async fn session(
        &self,
    ) -> Result<(
        tokio::sync::mpsc::UnboundedSender<ClientFrame>,
        tokio::sync::mpsc::UnboundedReceiver<Result<ServerFrame>>,
        tokio::task::JoinHandle<()>,
    )> {
        let mut client = Client::connect(&self.socket).await?;
        client.open(&agents::Open::Session).await?;
        let (mut reader, mut writer) = client.into_stream().into_split();
        let (to_host, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<ClientFrame>();
        let (incoming, from_host) = tokio::sync::mpsc::unbounded_channel();
        let closed = tokio::spawn(async move {
            while let Some(frame) = outgoing.recv().await {
                if rho_rpc::protocol::write_frame(&mut writer, &frame)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            // A clean end the host reads as the session leaving.
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
        });
        tokio::spawn(async move {
            loop {
                let frame = rho_rpc::protocol::read_frame::<_, ServerFrame>(&mut reader).await;
                let failed = frame.is_err();
                if incoming.send(frame).is_err() || failed {
                    break;
                }
            }
        });
        Ok((to_host, from_host, closed))
    }

    /// Stops the host if this CLI started it and nothing else uses it.
    /// Says why it stays, if it does.
    pub async fn leave(&self) -> Option<String> {
        if !self.started {
            return None;
        }
        // The session just closed; the host may not have counted it out
        // yet, so a refusal for an attached client is asked again briefly.
        let mut refusal = None;
        for _ in 0..20 {
            match self.call(rho_agent_hosts::protocol::ShutdownIfIdle).await {
                Ok(()) => return None,
                Err(error) => {
                    let text = format!("{error:#}");
                    let retry = text.contains("attached");
                    refusal = Some(text);
                    if !retry {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        refusal
    }
}

/// The last lines of a log, to show where it stopped.
fn log_tail(path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}
