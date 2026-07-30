//! The Rho GPUI client views and native application integration.

pub mod agent_view;
#[cfg(feature = "native")]
pub(crate) mod chime;
#[cfg(feature = "native")]
pub(crate) mod commands;
#[cfg(feature = "native")]
pub(crate) mod connection;
pub mod dashboard;
#[cfg(feature = "native")]
pub(crate) mod diff_view;
pub mod draft_view;
pub mod editor_config;
pub mod highlights;
pub mod minibuffer;
#[cfg(feature = "native")]
pub(crate) mod native_realtime;
pub mod pane;
pub mod render;
pub mod rho_assets;
#[cfg(all(test, feature = "native"))]
mod sampler;
#[cfg(feature = "native")]
pub(crate) mod shell_view;
pub mod style;
#[cfg(feature = "native")]
pub(crate) mod terminal_view;
pub mod transcript;
#[cfg(feature = "native")]
pub mod transient;
#[cfg(feature = "native")]
pub(crate) mod visualization;
#[cfg(feature = "native")]
pub mod workspace;
#[cfg(not(feature = "native"))]
#[path = "workspace_web.rs"]
pub mod workspace;
#[cfg(feature = "native")]
pub(crate) mod zed_remote;

// The registry and per-agent frame store live in a shared crate. These aliases
// preserve the existing module paths in the client views.
use gpui::actions;
pub use rho_registry as registry;
pub use rho_registry::store;

actions!(
    rho_gui,
    [
        SubmitPrompt,
        PastePrompt,
        AgentPrevious,
        AgentNext,
        AgentNew,
        AgentJumpAttention,
        AgentDone,
        AgentHide,
        DashboardNewAgent,
        DashboardReply,
        DashboardToggleSubagents,
        RoleCycle,
        RoleCycleGroup,
        TaskBoard,
        FileSave,
        PaneSplitRight,
        PaneSplitDown,
        PaneClose,
        PaneFocusNext,
        PaneBack,
        RailFocus,
        RailOpen,
        RootTransient,
        MinibufferConfirm,
        MinibufferCancel,
        MinibufferNext,
        MinibufferPrevious,
        MinibufferComplete,
        GitApprovalAllow,
        GitApprovalDeny,
        TerminalPaste,
        TerminalNormalMode,
        TerminalRawMode,
        TerminalScrollLineUp,
        TerminalScrollLineDown,
        TerminalScrollHalfPageUp,
        TerminalScrollHalfPageDown,
        TerminalScrollTop,
        TerminalScrollBottom,
        ShellInterrupt,
        ShellEof,
        ShellPagerMore,
        ShellPagerAll,
        ShellPagerQuit,
        VoiceToggle
    ]
);

#[cfg(all(test, feature = "native"))]
mod tests;
