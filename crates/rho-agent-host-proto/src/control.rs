//! The control stream: what a host pushes to one GUI.
//!
//! Opened by [`crate::host::Open::Control`], one per iroh connection. The
//! host speaks first with [`ServerFrame::Ready`] and then pushes whatever
//! changes about itself; nothing on this stream answers a request, because
//! requests have streams of their own ([`crate::host::Open::Request`]).

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::{AuthState, DesktopSession, GitTransportRequest};

/// What a GUI says on its control stream.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ClientFrame {
    /// This GUI holds SSH credentials and answers
    /// [`ServerFrame::GitTransportRequested`] for the host's Git remote
    /// helpers.
    ProvideGitTransport,
}

/// What a host pushes on a control stream.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum ServerFrame {
    /// The host as it stands: the first frame, and again whenever the
    /// agents it holds change.
    Ready {
        auth: AuthState,
        /// The daemon database's machine seed; clients need it to encode
        /// agent IDs (see [`crate::AgentIdDomain`]).
        machine_seed: u64,
        /// Last allocated agent-id counter; clients use it for uniform
        /// short-prefix rendering.
        agent_counter: u64,
    },
    /// The host's active/default auth changed, or its available namespaces
    /// were refreshed.
    AuthState {
        auth: AuthState,
    },
    DesktopSessions {
        sessions: Vec<DesktopSession>,
    },
    /// A Git remote helper wants a transport. Fanned out to every GUI that
    /// provides one; each answers with [`crate::host::Open::GitProvide`].
    GitTransportRequested {
        request_id: u64,
        provider_id: u64,
        request: GitTransportRequest,
    },
    /// An approval race completed or expired. Deliberately carries no result
    /// or winner information.
    GitTransportDone {
        request_id: u64,
    },
}
