//! In-process zed host: one headless gpui app on a dedicated thread, hosting
//! one [`remote_server::HeadlessProject`] per open channel session.
//!
//! The daemon talks to this subsystem only through [`ZedHost`]'s message
//! channels; all gpui/zed state lives on the host thread and is a rebuildable
//! projection of daemon truth. Each session serves exactly one client channel
//! (one workspace × one GUI connection) and owns its own stores — sessions
//! never share state, so the same path can be open in two sessions without
//! coordination.
//!
//! Language servers: the LSP stores exist and speak the full protocol, but
//! nothing spawns — the host's settings disable language servers globally.
//! File IO is plain [`fs::RealFs`] against workspace checkout paths as the
//! daemon sees them (managed checkout paths); no namespace entry happens here.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{AppContext as _, BorrowAppContext as _};
use prost::Message as _;
use remote_server::{HeadlessAppState, HeadlessProject};

/// Handle to the host thread. Cheap to share; the thread runs for the
/// daemon's lifetime once spawned.
pub struct ZedHost {
    commands: mpsc::UnboundedSender<HostCmd>,
    next_session: AtomicU64,
}

/// Wire streams for one session, carrying prost-encoded `proto::Envelope`s.
/// The daemon owns framing (ui-proto channel messages); the host owns
/// envelope (de)serialization.
pub struct SessionStreams {
    pub incoming: mpsc::UnboundedReceiver<Vec<u8>>,
    pub outgoing: mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(u64);

enum HostCmd {
    Open {
        session_id: SessionId,
        streams: SessionStreams,
    },
    Close {
        session_id: SessionId,
    },
}

impl ZedHost {
    /// Starts the host thread. Call once per daemon, lazily on first session.
    pub fn spawn() -> Self {
        let (commands, commands_rx) = mpsc::unbounded();
        std::thread::Builder::new()
            .name("rho-zed-host".to_owned())
            .spawn(move || run_app(commands_rx))
            .expect("failed to spawn rho-zed-host thread");
        Self {
            commands,
            next_session: AtomicU64::new(1),
        }
    }

    /// Binds a headless project to the stream pair. Infallible by design:
    /// failures surface to the client through the session's proto traffic
    /// (or its silence) rather than through the daemon.
    pub fn open_session(&self, streams: SessionStreams) -> SessionId {
        let session_id = SessionId(self.next_session.fetch_add(1, Ordering::Relaxed));
        let _ = self.commands.unbounded_send(HostCmd::Open {
            session_id,
            streams,
        });
        session_id
    }

    /// Tears the session down: drops the project (stores, watchers) and the
    /// stream pumps. Idempotent.
    pub fn close_session(&self, session_id: SessionId) {
        let _ = self.commands.unbounded_send(HostCmd::Close { session_id });
    }
}

/// Everything held per live session. Dropping it is the whole teardown:
/// the project entity releases its stores and worktrees, and the pump tasks
/// cancel with their channels.
struct Session {
    _project: gpui::Entity<HeadlessProject>,
    _pumps: [gpui::Task<()>; 2],
}

fn run_app(mut commands: mpsc::UnboundedReceiver<HostCmd>) {
    let app = gpui_platform::headless();
    app.run(move |cx| {
        release_channel::init(semver::Version::new(0, 1, 0), cx);
        gpui_tokio::init(cx);
        worktree::set_file_size_limit(8 * 1024 * 1024);
        HeadlessProject::init(cx);
        disable_language_servers(cx);

        cx.spawn(async move |cx| {
            let mut sessions: HashMap<SessionId, Session> = HashMap::new();
            while let Some(command) = commands.next().await {
                match command {
                    HostCmd::Open {
                        session_id,
                        streams,
                    } => {
                        let session = cx.update(|cx| open_session(streams, cx));
                        sessions.insert(session_id, session);
                    }
                    HostCmd::Close { session_id } => {
                        sessions.remove(&session_id);
                    }
                }
            }
        })
        .detach();
    });
}

/// LSP infrastructure stays wired (stores, proto handlers), but no language
/// server processes may spawn from this host yet.
fn disable_language_servers(cx: &mut gpui::App) {
    cx.update_global::<settings::SettingsStore, _>(|store, cx| {
        store
            .set_user_settings(r#"{ "enable_language_server": false }"#, cx)
            .result()
            .expect("host user settings must parse");
    });
}

fn open_session(streams: SessionStreams, cx: &mut gpui::App) -> Session {
    let SessionStreams {
        mut incoming,
        outgoing,
    } = streams;
    let (envelope_in_tx, envelope_in_rx) = mpsc::unbounded();
    let (envelope_out_tx, mut envelope_out_rx) = mpsc::unbounded();
    let session = remote::RemoteClient::proto_client_from_channels(
        envelope_in_rx,
        envelope_out_tx,
        cx,
        "rho-zed-host",
        false,
    );

    let decode = cx.background_spawn(async move {
        while let Some(bytes) = incoming.next().await {
            match rpc::proto::Envelope::decode(bytes.as_slice()) {
                Ok(envelope) => {
                    if envelope_in_tx.unbounded_send(envelope).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    log::error!("bad envelope from client: {error:#}");
                    break;
                }
            }
        }
    });
    let encode = cx.background_spawn(async move {
        while let Some(envelope) = envelope_out_rx.next().await {
            if outgoing
                .unbounded_send(prost::Message::encode_to_vec(&envelope))
                .is_err()
            {
                break;
            }
        }
    });

    let project = cx.new(|cx| {
        HeadlessProject::new(
            HeadlessAppState {
                session,
                fs: Arc::new(fs::RealFs::new(None, cx.background_executor().clone())),
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(
                    cx.background_executor().clone(),
                )),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    Session {
        _project: project,
        _pumps: [decode, encode],
    }
}
