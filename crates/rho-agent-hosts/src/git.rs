//! The user's ssh login, carried for a host's Git remote helpers.
//!
//! An agent that pushes to `main` or `master`, or reaches a repository
//! through the user's ssh login, asks its host; the host asks every window
//! that holds the login and waits, for as long as it takes, for the user's
//! word (`product/agents.md`, `rule a push waits for the user`). This is a
//! window's end: one [`HostStream`] per host hears what waits and turns
//! each request into a [`GitAsk`] the window shows as a card. The user's
//! answer goes back on the ask; an approval runs OpenSSH here, with the
//! user's own login, and carries the agent's Git through it.
//!
//! Nothing here asks on its own and nothing times out. A request the host
//! no longer holds ends ([`GitChange::Ended`]), and when the stream to the
//! host goes, so does everything it asked ([`GitChange::Lost`]): the host
//! asks again when it is back.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::sync::Arc;

use anyhow::Context as _;
use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use rho_rpc::protocol::{read_frame, write_open};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;

use crate::protocol::{
    GitDecision, GitProvided, GitProviderFrame, GitService, GitTransportRequest, Open as HostOpen,
};
use crate::{Dialer, HostId, HostStream};

/// What happened to the Git requests of one host.
#[derive(Debug)]
pub struct GitEvent {
    pub host: HostId,
    pub change: GitChange,
}

#[derive(Debug)]
pub enum GitChange {
    /// A request waits for the user's word.
    Asked(GitAsk),
    /// The request no longer waits: answered here or on another device, or
    /// its Git went away.
    Ended { request_id: u64 },
    /// The stream to the host has gone, and with it everything the host
    /// asked on it.
    Lost,
    /// A request this window refused without asking the user: it is not
    /// one the user's ssh login may carry.
    Refused { request_id: u64, reason: String },
    /// The user's answer to `what` ([`GitAsk::what`]) could not be carried
    /// out, such as OpenSSH failing after an approval.
    Failed { what: String, error: String },
}

/// One request that waits for the user, with the way to answer it.
#[derive(Debug)]
pub struct GitAsk {
    pub request_id: u64,
    pub request: GitTransportRequest,
    /// When the agent's Git asked, on the host's clock.
    pub requested_at_ms: u64,
    answer: oneshot::Sender<GitDecision>,
}

impl GitAsk {
    /// Hands the user's word to the host. `Err` gives the decision back
    /// when nothing waits for it any more: the stream to the host has gone.
    pub fn answer(self, decision: GitDecision) -> Result<(), GitDecision> {
        self.answer.send(decision)
    }

    /// An ask as a test builds it, with the end the answer arrives at.
    #[cfg(feature = "test-support")]
    pub fn for_test(
        request_id: u64,
        request: GitTransportRequest,
        requested_at_ms: u64,
    ) -> (Self, GitAnswered) {
        let (answer, answered) = oneshot::channel();
        (
            Self {
                request_id,
                request,
                requested_at_ms,
                answer,
            },
            answered,
        )
    }

    pub fn is_push(&self) -> bool {
        self.request.service == GitService::ReceivePack
    }

    /// Where it goes: `ssh://github.com:22/acme/repo`.
    pub fn destination(&self) -> String {
        destination(&self.request)
    }

    /// Every branch and tag a push would change, in the user's words:
    /// `branch main`, `tag v1`, or the ref itself for anything else. A
    /// fetch changes none.
    pub fn refs(&self) -> Vec<String> {
        self.request
            .planned_refs
            .iter()
            .flatten()
            .map(|reference| {
                let reference = display_field(reference);
                reference
                    .strip_prefix("refs/heads/")
                    .map(|name| format!("branch {name}"))
                    .or_else(|| {
                        reference
                            .strip_prefix("refs/tags/")
                            .map(|name| format!("tag {name}"))
                    })
                    .unwrap_or(reference)
            })
            .collect()
    }

    /// What it asks for, on one line: where it goes and, for a push, every
    /// branch and tag it would change.
    pub fn what(&self) -> String {
        match self.is_push() {
            true => format!("push to {}: {}", self.destination(), self.refs().join(", ")),
            false => format!("fetch from {}", self.destination()),
        }
    }
}

fn destination(request: &GitTransportRequest) -> String {
    format!(
        "ssh://{}:{}/{}",
        display_field(&request.host),
        request.port,
        display_field(&request.repository)
    )
}

/// Where a test's [`GitAsk::for_test`] hears the answer.
#[cfg(feature = "test-support")]
pub type GitAnswered = oneshot::Receiver<GitDecision>;

/// Every host's Git provider streams, and the one channel they report to.
pub struct GitStreams {
    events: futures_mpsc::UnboundedSender<GitEvent>,
    /// The OpenSSH client an approval runs.
    ssh: Arc<OsString>,
}

impl GitStreams {
    /// No host yet; the receiver hears every host's requests.
    pub fn new() -> (Self, futures_mpsc::UnboundedReceiver<GitEvent>) {
        Self::running("ssh".into())
    }

    /// As [`GitStreams::new`], with `ssh` run in OpenSSH's place.
    #[cfg(feature = "test-support")]
    pub fn with_ssh(ssh: impl Into<OsString>) -> (Self, futures_mpsc::UnboundedReceiver<GitEvent>) {
        Self::running(ssh.into())
    }

    fn running(ssh: OsString) -> (Self, futures_mpsc::UnboundedReceiver<GitEvent>) {
        let (events, received) = futures_mpsc::unbounded();
        (
            Self {
                events,
                ssh: Arc::new(ssh),
            },
            received,
        )
    }

    /// The provider stream for a host just attached, opened on every
    /// connection.
    pub fn stream(&self, host: HostId) -> Arc<dyn HostStream> {
        Arc::new(GitProvider {
            host,
            events: self.events.clone(),
            ssh: self.ssh.clone(),
        })
    }
}

struct GitProvider {
    host: HostId,
    events: futures_mpsc::UnboundedSender<GitEvent>,
    ssh: Arc<OsString>,
}

/// Says [`GitChange::Lost`] when the stream's future goes, however it goes.
struct Lost {
    host: HostId,
    events: futures_mpsc::UnboundedSender<GitEvent>,
}

impl Drop for Lost {
    fn drop(&mut self) {
        let _ = self.events.unbounded_send(GitEvent {
            host: self.host,
            change: GitChange::Lost,
        });
    }
}

impl HostStream for GitProvider {
    fn name(&self) -> &'static str {
        "Git provider"
    }

    fn run(&self, dialer: Dialer) -> BoxFuture<'static, anyhow::Result<()>> {
        let host = self.host;
        let events = self.events.clone();
        let ssh = self.ssh.clone();
        Box::pin(async move {
            let _lost = Lost {
                host,
                events: events.clone(),
            };
            let send = |change| events.unbounded_send(GitEvent { host, change }).is_ok();
            let mut stream = dialer.open(Some(1)).await?;
            write_open(&mut stream, &HostOpen::GitProvider).await?;
            // Dropped with this future, which ends every answer and
            // transport in flight.
            let mut answers = tokio::task::JoinSet::new();
            loop {
                let frame = read_frame(&mut stream).await?;
                while answers.try_join_next().is_some() {}
                let sent = match frame {
                    GitProviderFrame::Requested {
                        request_id,
                        provider_id,
                        request,
                        requested_at_ms,
                    } => {
                        let at = Decide {
                            dialer: dialer.clone(),
                            request_id,
                            provider_id,
                        };
                        match validate_git_transport_request(&request) {
                            Err(error) => {
                                answers.spawn(async move {
                                    let _ = at.decide(GitDecision::Deny).await;
                                });
                                send(GitChange::Refused {
                                    request_id,
                                    reason: format!("{error:#}"),
                                })
                            }
                            Ok(()) => {
                                let (answer, answered) = oneshot::channel();
                                let ask = GitAsk {
                                    request_id,
                                    request,
                                    requested_at_ms,
                                    answer,
                                };
                                answers.spawn(carry_answer(
                                    at,
                                    ask.request.clone(),
                                    ask.what(),
                                    answered,
                                    ssh.clone(),
                                    events.clone(),
                                    host,
                                ));
                                send(GitChange::Asked(ask))
                            }
                        }
                    }
                    GitProviderFrame::Done { request_id } => send(GitChange::Ended { request_id }),
                };
                // Nobody listens: the window has gone.
                if !sent {
                    return Ok(());
                }
            }
        })
    }
}

/// Where an answer to one request goes.
struct Decide {
    dialer: Dialer,
    request_id: u64,
    provider_id: u64,
}

impl Decide {
    /// Tells the host; an approval that the host still holds comes back
    /// as the stream to carry the transport on.
    async fn decide(self, decision: GitDecision) -> anyhow::Result<Option<rho_rpc::Stream>> {
        // Interactive streams outrank the sessions (priority 1 and below).
        let mut stream = self.dialer.open(Some(50)).await?;
        write_open(
            &mut stream,
            &HostOpen::GitProvide {
                request_id: self.request_id,
                provider_id: self.provider_id,
                decision,
            },
        )
        .await?;
        match read_frame(&mut stream).await? {
            GitProvided::Ready => Ok(Some(stream)),
            GitProvided::Done => Ok(None),
        }
    }
}

/// Waits for the user's word on one request and carries it out. A request
/// that ends unanswered drops its ask, and this with it.
async fn carry_answer(
    at: Decide,
    request: GitTransportRequest,
    what: String,
    answered: oneshot::Receiver<GitDecision>,
    ssh: Arc<OsString>,
    events: futures_mpsc::UnboundedSender<GitEvent>,
    host: HostId,
) {
    let Ok(decision) = answered.await else {
        return;
    };
    let carried = async {
        if let Some(stream) = at.decide(decision).await? {
            run_ssh(stream, &request, &ssh).await?;
        }
        anyhow::Ok(())
    };
    if let Err(error) = carried.await {
        let _ = events.unbounded_send(GitEvent {
            host,
            change: GitChange::Failed {
                what,
                error: format!("{error:#}"),
            },
        });
    }
}

/// Runs OpenSSH with the user's own login and carries the agent's Git
/// through it. A push carries exactly the refs the user approved.
async fn run_ssh(
    mut stream: rho_rpc::Stream,
    request: &GitTransportRequest,
    ssh: &OsString,
) -> anyhow::Result<()> {
    let remote_command = format!(
        "{} '{}'",
        match request.service {
            GitService::UploadPack => "git-upload-pack",
            GitService::ReceivePack => "git-receive-pack",
        },
        request.repository
    );
    let mut child = tokio::process::Command::new(ssh)
        .args(["-o", "BatchMode=yes"])
        .args(["-o", "ClearAllForwardings=yes"])
        .args(["-o", "PermitLocalCommand=no"])
        .args(["-o", "ControlMaster=no"])
        .arg("-p")
        .arg(request.port.to_string())
        .arg("-l")
        .arg(&request.user)
        .arg("--")
        .arg(&request.host)
        .arg(remote_command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("launch local OpenSSH")?;
    let mut ssh_stdin = child.stdin.take().context("OpenSSH stdin unavailable")?;
    let mut ssh_stdout = child.stdout.take().context("OpenSSH stdout unavailable")?;
    let ssh_stderr = child.stderr.take().context("OpenSSH stderr unavailable")?;
    let (mut transport_read, mut transport_write) = tokio::io::split(&mut stream);

    let input = async {
        if request.service == GitService::ReceivePack {
            copy_planned_receive_pack(
                &mut transport_read,
                &mut ssh_stdin,
                request
                    .planned_refs
                    .as_deref()
                    .context("SSH Git push is missing its destination ref plan")?,
            )
            .await?;
        } else {
            rho_rpc::copy_flush(&mut transport_read, &mut ssh_stdin).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let output = async {
        rho_rpc::copy_flush(&mut ssh_stdout, &mut transport_write).await?;
        Ok::<(), anyhow::Error>(())
    };
    let stderr = async {
        const MAX_STDERR: usize = 64 * 1024;
        let mut bytes = Vec::new();
        ssh_stderr
            .take(MAX_STDERR as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > MAX_STDERR {
            bytes.truncate(MAX_STDERR);
            bytes.extend_from_slice(b"\n[SSH stderr truncated]");
        }
        Ok::<Vec<u8>, anyhow::Error>(bytes)
    };
    let ((), (), stderr) = tokio::try_join!(input, output, stderr)?;
    let status = child.wait().await.context("wait for local OpenSSH")?;
    anyhow::ensure!(
        status.success(),
        "OpenSSH exited with {status}: {}",
        String::from_utf8_lossy(&stderr)
    );
    Ok(())
}

async fn copy_planned_receive_pack<R, W>(
    reader: &mut R,
    writer: &mut W,
    planned_refs: &[String],
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (prefix, commands) = read_receive_pack_prefix(reader).await?;
    anyhow::ensure!(
        receive_pack_refs_match(planned_refs, &commands),
        "Git receive-pack destination refs differ from the approved plan"
    );
    writer.write_all(&prefix).await?;
    rho_rpc::copy_flush(reader, writer).await?;
    Ok(())
}

fn receive_pack_refs_match(
    planned_refs: &[String],
    commands: &octo_types::ReceivePackCommands,
) -> bool {
    if planned_refs.len() != commands.updates.len() {
        return false;
    }
    let planned = planned_refs
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let actual = commands
        .updates
        .iter()
        .map(|update| update.reference.as_str())
        .collect::<BTreeSet<_>>();
    planned.len() == planned_refs.len() && planned == actual
}

async fn read_receive_pack_prefix<R>(
    reader: &mut R,
) -> anyhow::Result<(Vec<u8>, octo_types::ReceivePackCommands)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut prefix = Vec::new();
    loop {
        let mut chunk = [0_u8; 8192];
        let read = reader.read(&mut chunk).await?;
        anyhow::ensure!(read != 0, "truncated Git receive-pack command list");
        prefix.extend_from_slice(&chunk[..read]);
        match octo_types::parse_receive_pack_commands(&prefix) {
            Ok(Some(commands)) => return Ok((prefix, commands)),
            Ok(None) => {}
            Err(error) => anyhow::bail!(error),
        }
    }
}

fn validate_git_transport_request(request: &GitTransportRequest) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(request.host.as_str(), "github.com" | "git.sr.ht"),
        "invalid SSH Git host"
    );
    anyhow::ensure!(request.port != 0, "invalid SSH Git port");
    anyhow::ensure!(request.user == "git", "invalid SSH Git user");
    anyhow::ensure!(
        octo_types::valid_ssh_repository(&request.host, &request.repository),
        "invalid SSH Git repository path"
    );
    match (&request.service, &request.planned_refs) {
        (GitService::UploadPack, None) => {}
        (GitService::ReceivePack, Some(planned_refs)) => {
            anyhow::ensure!(
                !planned_refs.is_empty(),
                "SSH Git push has an empty ref plan"
            );
            anyhow::ensure!(
                planned_refs.iter().map(String::len).sum::<usize>()
                    <= octo_types::MAX_RECEIVE_PACK_COMMAND_BYTES,
                "SSH Git push ref plan is too large"
            );
            let mut unique = HashSet::new();
            anyhow::ensure!(
                planned_refs.iter().all(|reference| {
                    octo_types::valid_git_ref(reference) && unique.insert(reference)
                }),
                "SSH Git push ref plan is invalid"
            );
        }
        _ => anyhow::bail!("SSH Git transport has an invalid ref plan"),
    }
    Ok(())
}

fn display_field(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
            {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use octo_types::{ReceivePackCommands, RefUpdate};

    use super::*;

    fn ask(request: GitTransportRequest) -> (GitAsk, oneshot::Receiver<GitDecision>) {
        let (answer, answered) = oneshot::channel();
        (
            GitAsk {
                request_id: 1,
                request,
                requested_at_ms: 0,
                answer,
            },
            answered,
        )
    }

    fn receive_pack_input(reference: &str, old: &str, new: &str, tail: &[u8]) -> Vec<u8> {
        let command = format!("{old} {new} {reference}\0report-status\n");
        let mut input = format!("{:04x}{command}", command.len() + 4).into_bytes();
        input.extend_from_slice(b"0000");
        input.extend_from_slice(tail);
        input
    }

    #[test]
    fn client_rejects_unsafe_git_transport_fields() {
        let valid = GitTransportRequest {
            host: "github.com".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "team/repo".to_owned(),
            service: GitService::ReceivePack,
            planned_refs: Some(vec!["refs/heads/main".to_owned()]),
            agent: None,
        };
        assert!(validate_git_transport_request(&valid).is_ok());
        let mut sourcehut = valid.clone();
        sourcehut.host = "git.sr.ht".to_owned();
        sourcehut.repository = "~alice/project".to_owned();
        assert!(validate_git_transport_request(&sourcehut).is_ok());
        for host in ["github.com", "git.sr.ht"] {
            let mut request = valid.clone();
            request.host = host.to_owned();
            request.user = "root".to_owned();
            assert!(validate_git_transport_request(&request).is_err());
        }
        let mut unknown_host = valid.clone();
        unknown_host.host = "git.example".to_owned();
        assert!(validate_git_transport_request(&unknown_host).is_err());
        for repository in ["team/repo-name", "team/repo.git"] {
            let mut request = valid.clone();
            request.repository = repository.to_owned();
            assert!(validate_git_transport_request(&request).is_ok());
        }
        for repository in ["../repo", "team//repo"] {
            let mut request = valid.clone();
            request.repository = repository.to_owned();
            assert!(validate_git_transport_request(&request).is_err());
        }
        for planned_refs in [
            None,
            Some(Vec::new()),
            Some(vec![
                "refs/heads/main".to_owned(),
                "refs/heads/main".to_owned(),
            ]),
            Some(vec!["refs/heads/../main".to_owned()]),
        ] {
            let mut request = valid.clone();
            request.planned_refs = planned_refs;
            assert!(validate_git_transport_request(&request).is_err());
        }
        let mut fetch = valid;
        fetch.service = GitService::UploadPack;
        assert!(validate_git_transport_request(&fetch).is_err());
        fetch.planned_refs = None;
        assert!(validate_git_transport_request(&fetch).is_ok());
    }

    #[test]
    fn git_prompt_fields_replace_bidi_controls() {
        assert_eq!(display_field("main\u{202e}txt"), "main\u{fffd}txt");
    }

    #[test]
    fn push_prompt_names_destination_refs() {
        let request = GitTransportRequest {
            host: "github.com".to_owned(),
            port: 2222,
            user: "git".to_owned(),
            repository: "acme/repo".to_owned(),
            service: GitService::ReceivePack,
            planned_refs: Some(vec![
                "refs/heads/main".to_owned(),
                "refs/tags/v1".to_owned(),
                "refs/heads/rho/test".to_owned(),
                "refs/notes/review".to_owned(),
            ]),
            agent: None,
        };
        let (push, _) = ask(request.clone());
        assert_eq!(
            push.what(),
            "push to ssh://github.com:2222/acme/repo: branch main, tag v1, branch rho/test, \
             refs/notes/review"
        );
        let (fetch, _) = ask(GitTransportRequest {
            service: GitService::UploadPack,
            planned_refs: None,
            ..request
        });
        assert_eq!(fetch.what(), "fetch from ssh://github.com:2222/acme/repo");
        assert!(fetch.refs().is_empty());
    }

    #[test]
    fn receive_pack_plan_comparison_ignores_order_and_object_ids() {
        let commands = ReceivePackCommands {
            end: 0,
            updates: vec![
                RefUpdate {
                    old: "1".repeat(40),
                    new: "2".repeat(40),
                    reference: "refs/tags/v1".to_owned(),
                },
                RefUpdate {
                    old: "3".repeat(40),
                    new: "4".repeat(40),
                    reference: "refs/heads/main".to_owned(),
                },
            ],
        };
        assert!(receive_pack_refs_match(
            &["refs/heads/main".to_owned(), "refs/tags/v1".to_owned()],
            &commands
        ));
        assert!(!receive_pack_refs_match(
            &["refs/heads/main".to_owned()],
            &commands
        ));
    }

    #[test]
    fn matching_receive_pack_plan_forwards_exact_bytes() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let old = "1".repeat(40);
                let new = "2".repeat(40);
                let input = receive_pack_input("refs/heads/main", &old, &new, b"PACK tail");
                let (mut client, mut transport) = tokio::io::duplex(4096);
                let (mut ssh, mut remote) = tokio::io::duplex(4096);
                client.write_all(&input).await.unwrap();
                client.shutdown().await.unwrap();
                let copy = tokio::spawn(async move {
                    copy_planned_receive_pack(
                        &mut transport,
                        &mut ssh,
                        &["refs/heads/main".to_owned()],
                    )
                    .await
                });
                let mut received = Vec::new();
                remote.read_to_end(&mut received).await.unwrap();
                copy.await.unwrap().unwrap();
                assert_eq!(received, input);
            });
    }

    #[test]
    fn mismatched_receive_pack_plan_writes_nothing() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let input = receive_pack_input(
                    "refs/heads/rho/test",
                    &"1".repeat(40),
                    &"2".repeat(40),
                    b"PACK tail",
                );
                let (mut client, mut transport) = tokio::io::duplex(4096);
                let (mut ssh, mut remote) = tokio::io::duplex(4096);
                client.write_all(&input).await.unwrap();
                client.shutdown().await.unwrap();
                let result = copy_planned_receive_pack(
                    &mut transport,
                    &mut ssh,
                    &["refs/heads/main".to_owned()],
                )
                .await;
                assert!(result.is_err());
                drop(ssh);
                let mut received = Vec::new();
                remote.read_to_end(&mut received).await.unwrap();
                assert!(received.is_empty());
            });
    }

    #[test]
    fn malformed_receive_pack_writes_nothing() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (mut client, mut transport) = tokio::io::duplex(64);
                let (mut ssh, mut remote) = tokio::io::duplex(64);
                client.write_all(b"0003").await.unwrap();
                client.shutdown().await.unwrap();
                let result = copy_planned_receive_pack(
                    &mut transport,
                    &mut ssh,
                    &["refs/heads/main".to_owned()],
                )
                .await;
                assert!(result.is_err());
                drop(ssh);
                let mut received = Vec::new();
                remote.read_to_end(&mut received).await.unwrap();
                assert!(received.is_empty());
            });
    }

    /// The host, in this process, as the window's Git provider meets it.
    struct Host {
        opened: tokio::sync::mpsc::UnboundedReceiver<rho_rpc::Stream>,
        provider: rho_rpc::Stream,
        events: futures_mpsc::UnboundedReceiver<GitEvent>,
        running: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Host {
        /// The window registers as a provider, running `ssh` for OpenSSH.
        async fn connect(ssh: OsString) -> Self {
            let (streams, events) = GitStreams::running(ssh);
            let (dial, mut opened) = tokio::sync::mpsc::unbounded_channel();
            let running = tokio::spawn(streams.stream(HostId(3)).run(Dialer::InProcess(dial)));
            let mut provider = opened.recv().await.expect("the provider dialed");
            assert_eq!(opening(&mut provider).await, HostOpen::GitProvider);
            Self {
                opened,
                provider,
                events,
                running,
            }
        }

        async fn ask(&mut self, request_id: u64, request: GitTransportRequest) {
            rho_rpc::protocol::write_frame(
                &mut self.provider,
                &GitProviderFrame::Requested {
                    request_id,
                    provider_id: 2,
                    request,
                    requested_at_ms: 1_000,
                },
            )
            .await
            .unwrap();
        }

        async fn change(&mut self) -> GitChange {
            use futures::StreamExt as _;
            let event = self.events.next().await.expect("an event");
            assert_eq!(event.host, HostId(3));
            event.change
        }

        /// The next stream the window opens, and what it opened it for.
        async fn answer(&mut self) -> (rho_rpc::Stream, HostOpen) {
            let mut stream = self.opened.recv().await.expect("the window answered");
            let open = opening(&mut stream).await;
            (stream, open)
        }
    }

    async fn opening(stream: &mut rho_rpc::Stream) -> HostOpen {
        let open: rho_rpc::protocol::Open = read_frame(stream).await.unwrap();
        open.unpack::<HostOpen>().unwrap()
    }

    fn fetch() -> GitTransportRequest {
        GitTransportRequest {
            host: "github.com".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "acme/repo".to_owned(),
            service: GitService::UploadPack,
            planned_refs: None,
            agent: None,
        }
    }

    /// An OpenSSH that writes its arguments beside itself, answers the
    /// first line it reads, and exits.
    fn fake_ssh(directory: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = directory.join("ssh");
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nread line\necho \"carried $line\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Nothing happens until the user answers; the approval tells the host,
    /// and the Git the host hands over goes through OpenSSH, run with the
    /// user's login against the repository asked for.
    #[tokio::test]
    async fn an_approval_carries_the_git_through_the_users_ssh() {
        let directory = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(directory.path());
        let mut host = Host::connect(ssh.clone().into()).await;
        host.ask(7, fetch()).await;
        let GitChange::Asked(ask) = host.change().await else {
            panic!("asked nothing");
        };
        assert_eq!((ask.request_id, ask.requested_at_ms), (7, 1_000));
        assert_eq!(ask.what(), "fetch from ssh://github.com:22/acme/repo");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(host.opened.try_recv().is_err(), "nothing before the answer");
        assert!(!ssh.with_extension("args").exists(), "and no OpenSSH");

        ask.answer(GitDecision::Approve).unwrap();
        let (mut transport, open) = host.answer().await;
        assert_eq!(
            open,
            HostOpen::GitProvide {
                request_id: 7,
                provider_id: 2,
                decision: GitDecision::Approve,
            }
        );
        rho_rpc::protocol::write_frame(&mut transport, &GitProvided::Ready)
            .await
            .unwrap();
        transport.write_all(b"want 1234\n").await.unwrap();
        transport.shutdown().await.unwrap();
        let mut carried = String::new();
        transport.read_to_string(&mut carried).await.unwrap();
        assert_eq!(carried, "carried want 1234\n");
        let args = std::fs::read_to_string(ssh.with_extension("args")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(
            args[args.len() - 7..],
            [
                "-p",
                "22",
                "-l",
                "git",
                "--",
                "github.com",
                "git-upload-pack 'acme/repo'"
            ]
        );
        assert!(args.contains(&"BatchMode=yes"));
    }

    /// A denial tells the host and runs nothing; the host's answer to it
    /// is that the request is done.
    #[tokio::test]
    async fn a_denial_tells_the_host_and_runs_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(directory.path());
        let mut host = Host::connect(ssh.clone().into()).await;
        host.ask(8, fetch()).await;
        let GitChange::Asked(ask) = host.change().await else {
            panic!("asked nothing");
        };
        ask.answer(GitDecision::Deny).unwrap();
        let (mut answer, open) = host.answer().await;
        assert_eq!(
            open,
            HostOpen::GitProvide {
                request_id: 8,
                provider_id: 2,
                decision: GitDecision::Deny,
            }
        );
        rho_rpc::protocol::write_frame(&mut answer, &GitProvided::Done)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!ssh.with_extension("args").exists());
        assert!(host.events.try_recv().is_err(), "no failure to report");
    }

    /// A request the user's login may not carry is refused without asking;
    /// one the host ends is said to have ended, and its ask then answers
    /// nothing; and when the stream goes, everything it asked goes too.
    #[tokio::test]
    async fn refusals_ends_and_a_lost_stream() {
        let directory = tempfile::tempdir().unwrap();
        let mut host = Host::connect(fake_ssh(directory.path()).into()).await;
        host.ask(
            1,
            GitTransportRequest {
                host: "git.example".to_owned(),
                ..fetch()
            },
        )
        .await;
        let GitChange::Refused {
            request_id: 1,
            reason,
        } = host.change().await
        else {
            panic!("not refused");
        };
        assert_eq!(reason, "invalid SSH Git host");
        let (_, open) = host.answer().await;
        assert!(matches!(
            open,
            HostOpen::GitProvide {
                request_id: 1,
                decision: GitDecision::Deny,
                ..
            }
        ));

        host.ask(2, fetch()).await;
        let GitChange::Asked(ask) = host.change().await else {
            panic!("asked nothing");
        };
        rho_rpc::protocol::write_frame(
            &mut host.provider,
            &GitProviderFrame::Done { request_id: 2 },
        )
        .await
        .unwrap();
        assert!(matches!(
            host.change().await,
            GitChange::Ended { request_id: 2 }
        ));
        host.ask(3, fetch()).await;
        assert!(matches!(host.change().await, GitChange::Asked(_)));
        host.running.abort();
        assert!(matches!(host.change().await, GitChange::Lost));
        assert_eq!(
            ask.answer(GitDecision::Approve),
            Err(GitDecision::Approve),
            "nothing waits for an answer once the stream has gone"
        );
    }
}
