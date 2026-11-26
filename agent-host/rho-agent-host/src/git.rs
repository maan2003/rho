//! Git over the user's ssh login, on the host's side: a remote helper's
//! request waits here, for as long as it takes, until a GUI brings the
//! user's answer (`product/agents.md`, `rule a push waits for the user`).
//!
//! Every GUI that holds the login registers as a provider and hears every
//! request that waits, including those that began before it connected. The
//! first answer from any of them is the answer: an approval makes that GUI
//! carry the transport, and a denial refuses the helper. A request also
//! ends when its helper goes away, such as when its agent is stopped.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use rho_agent_hosts::protocol::{GitDecision, GitIntent, GitProviderFrame, GitTransportRequest};
use tokio::sync::{mpsc, oneshot};

pub(crate) trait GitStream:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send
{
}
impl<T> GitStream for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
pub(crate) type BoxGitStream = Box<dyn GitStream>;

/// How many requests may wait for the user at once.
const MAX_WAITING: usize = 8;

type Response = oneshot::Sender<Result<BoxGitStream, String>>;

#[derive(Default)]
struct State {
    providers: HashMap<u64, mpsc::UnboundedSender<GitProviderFrame>>,
    waiting: HashMap<u64, Waiting>,
}

struct Waiting {
    request: GitTransportRequest,
    requested_at_ms: u64,
    response: Response,
}

impl Waiting {
    fn frame(&self, request_id: u64, provider_id: u64) -> GitProviderFrame {
        GitProviderFrame::Requested {
            request_id,
            provider_id,
            request: self.request.clone(),
            requested_at_ms: self.requested_at_ms,
        }
    }
}

#[derive(Default)]
pub(crate) struct GitTransportBroker {
    next_provider_id: AtomicU64,
    /// Never held across an await, so dropping an [`Asked`] can take it.
    state: Mutex<State>,
}

/// What a GUI's answer comes to.
pub(crate) enum GitProviderClaim {
    /// The GUI approved: hand the helper its stream.
    Selected(Response),
    /// Nothing for the GUI to carry: it denied, or the request no longer
    /// waits.
    Done,
}

impl GitTransportBroker {
    /// Registers a GUI for as long as `provider` is open, and tells it
    /// every request that already waits.
    pub(crate) fn register(&self, provider: mpsc::UnboundedSender<GitProviderFrame>) {
        let provider_id = self.next_provider_id.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().unwrap();
        state.providers.retain(|_, provider| !provider.is_closed());
        for (&request_id, waiting) in &state.waiting {
            if provider
                .send(waiting.frame(request_id, provider_id))
                .is_err()
            {
                return;
            }
        }
        state.providers.insert(provider_id, provider);
    }

    /// Asks every GUI, now and as they connect. The request waits until a
    /// GUI answers or the returned [`Asked`] is dropped, with no deadline:
    /// the user may be away.
    pub(crate) fn request(&self, request: GitTransportRequest) -> anyhow::Result<Asked<'_>> {
        let request_id = uuid::Uuid::new_v4().as_u64_pair().0;
        let (response, answer) = oneshot::channel();
        let requested_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        let waiting = Waiting {
            request,
            requested_at_ms,
            response,
        };
        let mut state = self.state.lock().unwrap();
        anyhow::ensure!(
            state.waiting.len() < MAX_WAITING,
            "{MAX_WAITING} Git transport requests already wait for the user"
        );
        state.providers.retain(|&provider_id, provider| {
            provider
                .send(waiting.frame(request_id, provider_id))
                .is_ok()
        });
        state.waiting.insert(request_id, waiting);
        Ok(Asked {
            broker: self,
            request_id,
            answer,
        })
    }

    /// A GUI's answer to `request_id`. The first answer takes the request;
    /// every other GUI hears that it is done.
    pub(crate) fn decide(
        &self,
        request_id: u64,
        provider_id: u64,
        decision: GitDecision,
    ) -> GitProviderClaim {
        let mut state = self.state.lock().unwrap();
        let Some(waiting) = state.waiting.remove(&request_id) else {
            return GitProviderClaim::Done;
        };
        Self::notify_done(&state.providers, request_id, Some(provider_id));
        match decision {
            GitDecision::Approve => GitProviderClaim::Selected(waiting.response),
            GitDecision::Deny => {
                let _ = waiting.response.send(Err(denied(&waiting.request)));
                GitProviderClaim::Done
            }
        }
    }

    /// The helper went away before an answer: the request no longer waits.
    fn withdraw(&self, request_id: u64) {
        let mut state = self.state.lock().unwrap();
        if state.waiting.remove(&request_id).is_some() {
            Self::notify_done(&state.providers, request_id, None);
        }
    }

    fn notify_done(
        providers: &HashMap<u64, mpsc::UnboundedSender<GitProviderFrame>>,
        request_id: u64,
        except: Option<u64>,
    ) {
        for (&provider_id, provider) in providers {
            if Some(provider_id) != except {
                let _ = provider.send(GitProviderFrame::Done { request_id });
            }
        }
    }
}

/// A request that waits for the user. Dropping it withdraws the request.
pub(crate) struct Asked<'a> {
    broker: &'a GitTransportBroker,
    request_id: u64,
    answer: oneshot::Receiver<Result<BoxGitStream, String>>,
}

impl Asked<'_> {
    /// The stream of the GUI that carries the transport, or why there is
    /// none, such as the user's denial in words the agent reads.
    pub(crate) async fn answer(&mut self) -> Result<BoxGitStream, String> {
        (&mut self.answer)
            .await
            .unwrap_or_else(|_| Err("the window that approved this went away".to_owned()))
    }
}

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        self.broker.withdraw(self.request_id);
    }
}

/// What the agent's git reads when the user says no.
fn denied(request: &GitTransportRequest) -> String {
    let destination = format!("{}:{}", request.host, request.repository);
    match &request.intent {
        GitIntent::Push(push) => {
            let refs: Vec<&str> = push
                .updates
                .iter()
                .map(|update| update.reference.as_str())
                .collect();
            format!(
                "the user denied this push to {destination} ({})",
                refs.join(", ")
            )
        }
        GitIntent::Fetch => format!("the user denied this fetch from {destination}"),
    }
}

/// Resolves once the helper behind `reader` has gone. A helper writes
/// nothing before its stream is ready, so the first byte, if one comes,
/// stays in the buffer for the transport.
pub(crate) async fn requester_left<R>(reader: &mut R)
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt as _;
    match reader.fill_buf().await {
        Ok([]) | Err(_) => {}
        Ok(_) => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rho_agent_hosts::protocol::{
        GitDecision, GitIntent, GitProviderFrame, GitPush, GitRefChange, GitRefUpdate,
        GitTransportRequest,
    };
    use tokio::sync::mpsc;

    use super::{GitProviderClaim, GitTransportBroker, requester_left};

    fn push(branch: &str) -> GitTransportRequest {
        GitTransportRequest {
            host: "github.com".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "acme/repo".to_owned(),
            intent: GitIntent::Push(GitPush {
                updates: vec![GitRefUpdate {
                    reference: format!("refs/heads/{branch}"),
                    change: GitRefChange::Create {
                        new: "1".repeat(40),
                    },
                }],
                commits: Vec::new(),
                commit_count: 0,
                diffs: Vec::new(),
                diff_cut: false,
            }),
            agent: None,
        }
    }

    fn pushed_ref(request: GitTransportRequest) -> String {
        match request.intent {
            GitIntent::Push(push) => push.updates[0].reference.clone(),
            GitIntent::Fetch => panic!("a fetch"),
        }
    }

    fn requested(frame: Option<GitProviderFrame>) -> (u64, u64, GitTransportRequest) {
        match frame {
            Some(GitProviderFrame::Requested {
                request_id,
                provider_id,
                request,
                ..
            }) => (request_id, provider_id, request),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    fn provider(broker: &GitTransportBroker) -> mpsc::UnboundedReceiver<GitProviderFrame> {
        let (frames, received) = mpsc::unbounded_channel();
        broker.register(frames);
        received
    }

    /// Nobody answers for ten minutes, and the request still waits: there
    /// is no deadline. The approval that finally comes takes it.
    #[tokio::test(start_paused = true)]
    async fn a_request_waits_as_long_as_the_user_takes() {
        let broker = Arc::new(GitTransportBroker::default());
        let mut window = provider(&broker);
        let waiting = {
            let broker = broker.clone();
            tokio::spawn(async move {
                let mut asked = broker.request(push("main")).unwrap();
                asked.answer().await.map(|_| ())
            })
        };
        let (request_id, provider_id, _) = requested(window.recv().await);
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        assert!(!waiting.is_finished(), "still waiting after ten minutes");
        let GitProviderClaim::Selected(response) =
            broker.decide(request_id, provider_id, GitDecision::Approve)
        else {
            panic!("the approval did not take the request");
        };
        let (stream, _peer) = tokio::io::duplex(64);
        assert!(response.send(Ok(Box::new(stream))).is_ok());
        assert_eq!(waiting.await.unwrap(), Ok(()));
    }

    /// A push asked while no window is open waits, and the window that
    /// connects later hears it, together with every other that waits.
    #[tokio::test]
    async fn a_window_that_connects_later_hears_what_waits() {
        let broker = GitTransportBroker::default();
        let _main = broker.request(push("main")).unwrap();
        let _master = broker.request(push("master")).unwrap();
        let mut window = provider(&broker);
        let mut heard = vec![
            pushed_ref(requested(window.recv().await).2),
            pushed_ref(requested(window.recv().await).2),
        ];
        heard.sort();
        assert_eq!(heard, ["refs/heads/main", "refs/heads/master"]);
        assert!(window.try_recv().is_err(), "each once");
    }

    /// A denial from one window refuses the helper in words that say what
    /// was refused, and the other window hears the request is done.
    #[tokio::test]
    async fn a_denial_refuses_the_helper_and_ends_the_request_everywhere() {
        let broker = GitTransportBroker::default();
        let mut laptop = provider(&broker);
        let mut phone = provider(&broker);
        let mut asked = broker.request(push("main")).unwrap();
        let (request_id, laptop_id, _) = requested(laptop.recv().await);
        requested(phone.recv().await);
        assert!(matches!(
            broker.decide(request_id, laptop_id, GitDecision::Deny),
            GitProviderClaim::Done
        ));
        let refused = asked.answer().await.err().expect("refused");
        assert_eq!(
            refused,
            "the user denied this push to github.com:acme/repo (refs/heads/main)"
        );
        assert_eq!(
            phone.recv().await,
            Some(GitProviderFrame::Done { request_id })
        );
        // A late approval from the phone finds nothing to carry.
        assert!(matches!(
            broker.decide(request_id, 1, GitDecision::Approve),
            GitProviderClaim::Done
        ));
    }

    /// When the agent's git goes away, every window hears the request is
    /// done, and an answer after that carries nothing.
    #[tokio::test]
    async fn a_helper_that_goes_away_ends_its_request() {
        let broker = GitTransportBroker::default();
        let mut window = provider(&broker);
        let asked = broker.request(push("main")).unwrap();
        let (request_id, provider_id, _) = requested(window.recv().await);
        drop(asked);
        assert_eq!(
            window.recv().await,
            Some(GitProviderFrame::Done { request_id })
        );
        assert!(matches!(
            broker.decide(request_id, provider_id, GitDecision::Approve),
            GitProviderClaim::Done
        ));
        let mut later = provider(&broker);
        assert!(later.try_recv().is_err(), "nothing waits any more");
    }

    /// Two requests wait side by side, and answering one leaves the other.
    #[tokio::test]
    async fn two_requests_wait_side_by_side() {
        let broker = GitTransportBroker::default();
        let mut window = provider(&broker);
        let mut first = broker.request(push("main")).unwrap();
        let _second = broker.request(push("master")).unwrap();
        let (first_id, provider_id, _) = requested(window.recv().await);
        let (second_id, ..) = requested(window.recv().await);
        assert_ne!(first_id, second_id);
        broker.decide(first_id, provider_id, GitDecision::Deny);
        assert!(first.answer().await.is_err());
        assert!(
            window.try_recv().is_err(),
            "the window that answered hears nothing more"
        );
        let mut later = provider(&broker);
        assert_eq!(requested(later.recv().await).0, second_id);
    }

    /// The helper's end of the stream closing is what `requester_left`
    /// waits for; a byte it sends early stays for the transport.
    #[tokio::test]
    async fn requester_left_waits_for_the_end_and_keeps_early_bytes() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (mut helper, host) = tokio::io::duplex(64);
        let mut reader = tokio::io::BufReader::new(host);
        let waiting = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            requester_left(&mut reader),
        );
        assert!(waiting.await.is_err(), "the helper is still there");
        helper.write_all(b"0").await.unwrap();
        let waiting = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            requester_left(&mut reader),
        );
        assert!(waiting.await.is_err(), "a byte is not an end");
        let mut byte = [0; 1];
        reader.read_exact(&mut byte).await.unwrap();
        assert_eq!(&byte, b"0");
        drop(helper);
        requester_left(&mut reader).await;
    }
}
