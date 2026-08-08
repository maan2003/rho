//! The Rho GPUI client views and native application integration.

pub mod agent_view;
#[cfg(feature = "native")]
pub(crate) mod chime;
pub(crate) mod commands;
#[cfg(feature = "native")]
pub(crate) mod connection;
#[cfg(all(target_family = "wasm", not(feature = "native")))]
#[path = "connection_web.rs"]
pub(crate) mod connection;
#[cfg(all(target_family = "wasm", not(feature = "native")))]
pub(crate) use connection as connection_web;
pub mod dashboard;
pub mod desk_view;
pub(crate) mod diff_view;
pub mod draft_view;
pub mod editor_config;
pub mod highlights;
pub mod hosts;
pub mod minibuffer;
pub mod pane;
pub(crate) mod realtime_client;
pub mod render;
pub mod rho_assets;
#[cfg(all(test, feature = "native"))]
mod sampler;
pub(crate) mod shell_view;
pub mod style;
pub(crate) mod terminal_view;
pub mod transcript;
pub mod transient;
pub(crate) mod visualization;
pub mod workspace;
pub(crate) mod zed_remote;

// The registry and per-agent frame store live in a shared crate. These aliases
// preserve the existing module paths in the client views.
use gpui::actions;
#[cfg(feature = "native")]
use gpui::{App, KeyBinding};
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
        DashboardNow,
        DashboardBack,
        DashboardJump,
        DashboardStaff,
        DashboardGoto,
        DashboardToggleSubagents,
        DashboardHeadingBelow,
        DashboardHeadingAbove,
        DashboardDemote,
        DashboardPromote,
        DashboardMoveUp,
        DashboardMoveDown,
        DashboardDeleteEmpty,
        DashboardUndo,
        DashboardMoveAgent,
        DashboardRenameTopic,
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
        VoiceToggle,
        ZulipOpenRow,
        ZulipNextUnread,
        ZulipLoadOlder,
        ZulipQuit
    ]
);

#[cfg(feature = "native")]
#[doc(hidden)]
#[derive(serde::Serialize)]
pub struct Distribution {
    count: usize,
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

#[cfg(feature = "native")]
#[doc(hidden)]
pub fn distribution(values: impl IntoIterator<Item = u64>, scale: f64) -> Distribution {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_unstable();
    let count = values.len();
    if count == 0 {
        return Distribution {
            count,
            mean: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            max: 0.0,
        };
    }
    let percentile = |percent: usize| {
        let index = (count * percent).div_ceil(100).saturating_sub(1);
        values[index] as f64 / scale
    };
    Distribution {
        count,
        mean: values.iter().map(|value| *value as f64).sum::<f64>() / count as f64 / scale,
        p50: percentile(50),
        p95: percentile(95),
        p99: percentile(99),
        max: values[count - 1] as f64 / scale,
    }
}

#[cfg(feature = "native")]
#[doc(hidden)]
pub fn bind_rho_key_overrides(cx: &mut App) {
    // Keep draft field navigation available in vim normal mode. The bundled
    // vim keymap only binds the rho prompt keys for insert mode, while the
    // default keymap's Tab binding can lose to vim's normal-mode handling.
    cx.bind_keys([
        // Attention triage: jump to the most urgent agent, clear the current
        // one. The bundled zed keymaps don't know these actions, so they are
        // bound here rather than in an asset. The context must be at least as
        // deep as `Editor`: the bundled keymap binds these keys under plain
        // `Editor` (JoinLines, git::Diff), and gpui prefers the deeper match,
        // so a root-level `RhoGui` binding would lose while typing.
        KeyBinding::new("ctrl-shift-j", AgentJumpAttention, Some("RhoGui > Editor")),
        KeyBinding::new("ctrl-shift-d", AgentDone, Some("RhoGui > Editor")),
        KeyBinding::new(
            "tab",
            RoleCycle,
            Some("RhoGui > Editor && !showing_completions"),
        ),
        KeyBinding::new(
            "shift-tab",
            RoleCycleGroup,
            Some("RhoGui > Editor && !showing_completions"),
        ),
        KeyBinding::new("ctrl-s", FileSave, Some("RhoFileView")),
        KeyBinding::new("ctrl-s", FileSave, Some("RhoDiffView")),
        // Preserve Vim's normal-mode Ctrl-V (visual block). Clipboard paste
        // is intercepted only while editing a prompt.
        KeyBinding::new(
            "ctrl-v",
            PastePrompt,
            Some("RhoGui > Editor && vim_mode == insert"),
        ),
        KeyBinding::new("ctrl-shift-v", PastePrompt, Some("RhoGui > Editor")),
        // A Comint-style shell submits complete input lines to the daemon;
        // its transcript remains an ordinary Vim-navigable editor buffer.
        KeyBinding::new(
            "enter",
            SubmitPrompt,
            Some("RhoShell > Editor && vim_mode == insert"),
        ),
        KeyBinding::new(
            "ctrl-c",
            ShellInterrupt,
            Some("RhoShell > Editor && vim_mode == insert"),
        ),
        KeyBinding::new(
            "ctrl-d",
            ShellEof,
            Some("RhoShell > Editor && vim_mode == insert"),
        ),
        KeyBinding::new("alt-enter", ShellPagerMore, Some("RhoShell > Editor")),
        KeyBinding::new("alt-a", ShellPagerAll, Some("RhoShell > Editor")),
        KeyBinding::new("alt-q", ShellPagerQuit, Some("RhoShell > Editor")),
        // Terminal surface, raw mode: every unbound key becomes terminal
        // input, so its few chrome bindings use chords shells don't see
        // anyway. `ctrl-\ ctrl-n` is vim's terminal escape; `ctrl-shift-n`
        // is the discoverable chord for the same thing.
        KeyBinding::new("ctrl-shift-v", TerminalPaste, Some("RhoTerminal")),
        KeyBinding::new("ctrl-shift-;", RootTransient, Some("RhoTerminal")),
        KeyBinding::new("ctrl-\\ ctrl-n", TerminalNormalMode, Some("RhoTerminal")),
        KeyBinding::new("ctrl-shift-n", TerminalNormalMode, Some("RhoTerminal")),
        // Terminal normal mode: the keyboard belongs to rho again. Insert
        // returns to raw; plain vim keys browse scrollback.
        KeyBinding::new("i", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("a", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("enter", TerminalRawMode, Some("RhoTerminalNormal")),
        KeyBinding::new("j", TerminalScrollLineDown, Some("RhoTerminalNormal")),
        KeyBinding::new("k", TerminalScrollLineUp, Some("RhoTerminalNormal")),
        KeyBinding::new("down", TerminalScrollLineDown, Some("RhoTerminalNormal")),
        KeyBinding::new("up", TerminalScrollLineUp, Some("RhoTerminalNormal")),
        KeyBinding::new(
            "ctrl-d",
            TerminalScrollHalfPageDown,
            Some("RhoTerminalNormal"),
        ),
        KeyBinding::new(
            "ctrl-u",
            TerminalScrollHalfPageUp,
            Some("RhoTerminalNormal"),
        ),
        KeyBinding::new("g g", TerminalScrollTop, Some("RhoTerminalNormal")),
        KeyBinding::new("shift-g", TerminalScrollBottom, Some("RhoTerminalNormal")),
    ]);
    // The space leader: one binding, opening the root transient at once
    // (invisible until the reveal delay). Every chord beneath it — panes
    // and agent verbs — is a transient item, so practiced
    // sequences run at full speed without the menu ever flashing. Bound for
    // normal-mode editors (vim or helix flavor — helix reports
    // `vim_mode == helix_normal`); the dashboard is an editor too, so the
    // same contexts cover it.
    for context in [
        "RhoTerminalNormal",
        "RhoGui > Editor && vim_mode == normal",
        "RhoGui > Editor && vim_mode == helix_normal",
    ] {
        cx.bind_keys([KeyBinding::new("space", RootTransient, Some(context))]);
    }
    // Minibuffer keys. The input is a single-line editor (vim skips those),
    // but enter/escape/tab still need to beat the editor's own bindings, so
    // they are scoped under the minibuffer context and loaded last.
    cx.bind_keys([
        KeyBinding::new("enter", MinibufferConfirm, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("escape", MinibufferCancel, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("tab", MinibufferComplete, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-n", MinibufferNext, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("ctrl-p", MinibufferPrevious, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("down", MinibufferNext, Some("RhoMinibuffer > Editor")),
        KeyBinding::new("up", MinibufferPrevious, Some("RhoMinibuffer > Editor")),
    ]);
    // The Zulip surfaces read like Gnus: the inbox is a group buffer whose
    // rows are acted on by single normal-mode keys, and `n` walks to the
    // next unread conversation from anywhere in the client, marking the one
    // you leave as read. `enter` in a conversation's compose region sends,
    // matching the shell and transcript prompts.
    cx.bind_keys([
        KeyBinding::new("enter", ZulipOpenRow, Some("RhoZulipInbox > Editor")),
        KeyBinding::new(
            "enter",
            SubmitPrompt,
            Some("RhoZulipNarrow > Editor && vim_mode == insert"),
        ),
    ]);
    for context in [
        "RhoZulipInbox > Editor && vim_mode == normal",
        "RhoZulipInbox > Editor && vim_mode == helix_normal",
        "RhoZulipNarrow > Editor && vim_mode == normal",
        "RhoZulipNarrow > Editor && vim_mode == helix_normal",
    ] {
        cx.bind_keys([
            KeyBinding::new("n", ZulipNextUnread, Some(context)),
            KeyBinding::new("shift-p", ZulipLoadOlder, Some(context)),
            KeyBinding::new("q", ZulipQuit, Some(context)),
        ]);
    }
    cx.bind_keys([
        KeyBinding::new("shift-y", GitApprovalAllow, Some("RhoGitApproval")),
        KeyBinding::new("n", GitApprovalDeny, Some("RhoGitApproval")),
        KeyBinding::new("enter", GitApprovalDeny, Some("RhoGitApproval")),
        KeyBinding::new("escape", GitApprovalDeny, Some("RhoGitApproval")),
    ]);
    // Desk verbs, vim-native: text editing stays pure vim everywhere. Verbs
    // bind only in normal mode; handlers for single-letter verbs act on a
    // node's heading line and propagate to vim's own binding anywhere else,
    // so `o`, `s`, `d`, `x`, and Tab keep their vim meaning in body text.
    // Navigation uses vim-idiomatic `g`-prefixed gotos and works anywhere.
    cx.bind_keys([KeyBinding::new(
        "ctrl-z",
        DashboardUndo,
        Some("RhoDashboard > Editor"),
    )]);
    for context in [
        "RhoDashboard > Editor && vim_mode == normal",
        "RhoDashboard > Editor && vim_mode == helix_normal",
    ] {
        cx.bind_keys([
            KeyBinding::new("enter", DashboardGoto, Some(context)),
            KeyBinding::new("r", DashboardReply, Some(context)),
            KeyBinding::new("o", DashboardStaff, Some(context)),
            KeyBinding::new("shift-o", DashboardHeadingBelow, Some(context)),
            KeyBinding::new("d", AgentDone, Some(context)),
            KeyBinding::new("x", AgentHide, Some(context)),
            KeyBinding::new("tab", DashboardToggleSubagents, Some(context)),
            KeyBinding::new("z a", DashboardToggleSubagents, Some(context)),
            KeyBinding::new("> >", DashboardDemote, Some(context)),
            KeyBinding::new("< <", DashboardPromote, Some(context)),
            KeyBinding::new("alt-up", DashboardMoveUp, Some(context)),
            KeyBinding::new("alt-down", DashboardMoveDown, Some(context)),
            KeyBinding::new("backspace", DashboardDeleteEmpty, Some(context)),
            KeyBinding::new("u", DashboardUndo, Some(context)),
            KeyBinding::new("g n", DashboardNow, Some(context)),
            KeyBinding::new("g b", DashboardBack, Some(context)),
            KeyBinding::new("g h", DashboardJump, Some(context)),
            KeyBinding::new("m", DashboardMoveAgent, Some(context)),
            KeyBinding::new("c r", DashboardRenameTopic, Some(context)),
        ]);
    }
}

#[cfg(all(test, feature = "native"))]
mod tests;
