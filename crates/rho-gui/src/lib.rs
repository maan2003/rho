//! The Rho GPUI client views and native application integration.

pub(crate) mod attention;
pub(crate) mod browser;
pub(crate) mod chime;
pub(crate) mod commands;
pub(crate) mod create;
pub(crate) mod find;
pub(crate) mod git_approval;
pub mod home;
pub mod minibuffer;
pub mod note_view;
pub(crate) mod overlay;
pub mod pane;
pub mod rho_assets;
#[cfg(test)]
mod sampler;
pub(crate) mod search;
mod selection;
pub mod slack;
mod slack_navigation;
pub mod telemetry;
#[doc(hidden)]
pub mod transient;
pub(crate) mod usage;
pub(crate) mod voice;
#[cfg(feature = "walk-support")]
pub mod walk;
pub mod workspace;

use gpui::{App, KeyBinding, actions};
use rho_agents_view::{DraftFieldClear, DraftFieldSubmit, DraftValueCycle, RoleCycleGroup};
pub use rho_files::FileSave;
pub use rho_shell_view::{ShellEof, ShellInterrupt, ShellPagerAll, ShellPagerMore, ShellPagerQuit};
use rho_terminal::{
    TerminalNormalMode, TerminalPaste, TerminalRawMode, TerminalScrollBottom,
    TerminalScrollHalfPageDown, TerminalScrollHalfPageUp, TerminalScrollLineDown,
    TerminalScrollLineUp, TerminalScrollTop,
};

actions!(
    rho_gui,
    [
        SubmitPrompt,
        TranscriptTop,
        SearchRepeat,
        SearchRepeatReverse,
        PastePrompt,
        AgentPrevious,
        AgentNext,
        AgentNew,
        DealExit,
        DealNext,
        DealDone,
        DealMute,
        DealSnooze,
        DealSnoozeMinutes,
        DealSnoozeHours,
        DealSnoozeWeeks,
        DealRoomSnooze,
        DealTodo,
        UndoVerdict,
        DealReply,
        DealRefresh,
        DealFile,
        TaskBoard,
        BrowserExit,
        RootTransient,
        MinibufferConfirm,
        MinibufferCancel,
        MinibufferNext,
        MinibufferPrevious,
        MinibufferComplete,
        GitApprovalAllow,
        GitApprovalDeny,
        VoiceToggle,
        UploadGuiTelemetry,
        SurfaceBack,
        DealOpen,
        DealCloseAndNext,
        OverviewToggle,
        VerdictMenu,
        SurfaceClose,
        SlackQuickSwitch,
        SlackSidebarFocus,
        SlackConversationFocus,
        SlackNewMessage,
        SlackBrowseChannels,
        SlackOpenRow,
        SlackCompose,
        SlackSearch,
        SlackFindMessage,
        SlackFindFile,
        SlackOpenFound,
        SlackSearchPreviousPage,
        SlackSearchNextPage,
        SlackMarkReadBefore,
        SlackMarkUnread,
        SlackSaveForLater,
        SlackNextUnread,
        SlackEditMessage,
        SlackReactTo,
        SlackEditLast,
        SlackCancelEdit,
        FindNode,
        NotesForThis,
        NoteOpenRow,
        MessagesOpen,
        HomeOpenRow,
    ]
);

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

#[doc(hidden)]
pub fn bind_rho_key_overrides(cx: &mut App) {
    // Keep draft field navigation available in vim normal mode. The bundled
    // vim keymap only binds the rho prompt keys for insert mode, while the
    // default keymap's Tab binding can lose to vim's normal-mode handling.
    cx.bind_keys([
        KeyBinding::new("ctrl-k", SurfaceBack, Some("RhoGui")),
        KeyBinding::new("ctrl-j", DealOpen, Some("RhoGui")),
        KeyBinding::new("f20", DealOpen, Some("RhoGui")),
        KeyBinding::new("f21", SurfaceBack, Some("RhoGui")),
        KeyBinding::new("f24", OverviewToggle, Some("RhoGui")),
        KeyBinding::new("ctrl-shift-backspace", SurfaceClose, Some("RhoGui")),
        KeyBinding::new("ctrl-shift-j", DealCloseAndNext, Some("RhoGui")),
        KeyBinding::new("f16", DealCloseAndNext, Some("RhoGui")),
        KeyBinding::new("ctrl-k", SurfaceBack, Some("RhoGui > Editor")),
        KeyBinding::new("ctrl-j", DealOpen, Some("RhoGui > Editor")),
        KeyBinding::new("f20", DealOpen, Some("RhoGui > Editor")),
        KeyBinding::new(
            "ctrl-shift-backspace",
            SurfaceClose,
            Some("RhoGui > Editor"),
        ),
        KeyBinding::new("ctrl-shift-j", DealCloseAndNext, Some("RhoGui > Editor")),
        KeyBinding::new("f16", DealCloseAndNext, Some("RhoGui > Editor")),
        // Find, the node finder. Bound at both depths for the same reason
        // as the keys above: the bundled keymap binds `ctrl-shift-f` to
        // search under plain `Editor`, and gpui prefers the deeper match.
        KeyBinding::new("ctrl-shift-f", FindNode, Some("RhoGui")),
        KeyBinding::new("ctrl-shift-f", FindNode, Some("RhoGui > Editor")),
        // Notes for whatever is on screen. The terminal keeps `ctrl-shift-n`
        // for its own escape, which is bound deeper and so still wins there.
        KeyBinding::new("ctrl-shift-n", NotesForThis, Some("RhoGui")),
        KeyBinding::new("ctrl-shift-n", NotesForThis, Some("RhoGui > Editor")),
        // Attention triage: jump to the most urgent agent, clear the current
        // one. The bundled zed keymaps don't know these actions, so they are
        // bound here rather than in an asset. The context must be at least as
        // deep as `Editor`: the bundled keymap binds these keys under plain
        // `Editor` (JoinLines, git::Diff), and gpui prefers the deeper match,
        // so a root-level `RhoGui` binding would lose while typing.
        // `tab` is the verdicts over a card and the fields on the draft: one
        // action, and the handler asks the draft first.
        KeyBinding::new(
            "tab",
            VerdictMenu,
            Some("RhoGui > Editor && !showing_completions"),
        ),
        KeyBinding::new(
            "shift-tab",
            RoleCycleGroup,
            Some("RhoGui > Editor && !showing_completions && !VimDeal"),
        ),
        // The values the header rows hold, cycled in place: the role, and
        // whether the agent starts on top of the target, joins it, or is
        // sandboxed. Shift-tab used to do this, which cost the draft its
        // way back through the rows.
        KeyBinding::new(
            "ctrl-tab",
            DraftValueCycle,
            Some("RhoGui > Editor && !showing_completions && !VimDeal"),
        ),
        // A draft is one message however many rows it has, so enter in the
        // workdir, role, or start row sends it like enter in the body does.
        // The prompt's own enter is insert-only, and tab leaves the cursor
        // in normal mode, so from a field the key used to mean nothing at
        // all. In the body it is still vim's own motion; the handler passes
        // it on.
        KeyBinding::new(
            "enter",
            DraftFieldSubmit,
            Some("RhoDraft > Editor && !showing_completions && vim_mode != insert"),
        ),
        // `ctrl-u` empties the header row the cursor is on, in either mode,
        // the way it clears a line everywhere else. Each row of the draft
        // is its own buffer in one multibuffer, and vim's own `cc` stops at
        // that boundary: it clears nothing and leaves the cursor on the
        // seam, so the next character typed lands in the row below. Vim
        // reads the operator before any keymap does, so the row needs a key
        // of its own. In the body this is vim's scroll and stays vim's.
        KeyBinding::new(
            "ctrl-u",
            DraftFieldClear,
            Some("RhoDraft > Editor && !showing_completions"),
        ),
        KeyBinding::new("ctrl-s", FileSave, Some("RhoFileView")),
        // Preserve Vim's normal-mode Ctrl-V (visual block). Clipboard paste
        // is intercepted only while editing a prompt.
        KeyBinding::new(
            "ctrl-v",
            PastePrompt,
            Some("RhoGui > Editor && vim_mode == insert"),
        ),
        KeyBinding::new("ctrl-shift-v", PastePrompt, Some("RhoGui > Editor")),
        // Shift-Escape belongs to the VimFx-style browser layer: it leaves
        // Ignore mode. Keep one explicit Rho escape hatch outside that
        // vocabulary for returning to rho from any website.
        KeyBinding::new(
            "ctrl-shift-escape",
            BrowserExit,
            Some("RhoGui > RhoBrowser"),
        ),
        // A picture surface has nothing to type into: the two keys that mean
        // "I am done looking" both close it.
        KeyBinding::new("q", SurfaceClose, Some("RhoGui > RhoImage")),
        KeyBinding::new("escape", SurfaceClose, Some("RhoGui > RhoImage")),
        KeyBinding::new("ctrl-alt-shift-p", UploadGuiTelemetry, Some("RhoGui")),
        // Capture is global and modal: one chord, type, enter, and focus is
        // restored to the exact surface that owned it.
        // A Comint-style shell submits complete input lines to the agent host;
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
        KeyBinding::new("q", SurfaceClose, Some("RhoTerminalNormal")),
    ]);
    // The space leader: one binding, opening the root transient at once
    // (invisible until the reveal delay). Every chord beneath it is a
    // transient item, so practiced
    // sequences run at full speed without the menu ever flashing. Bound for
    // normal-mode editors (vim or helix flavor — helix reports
    // `vim_mode == helix_normal`); Home is an editor too, so the same
    // contexts cover it.
    for context in [
        "RhoTerminalNormal",
        "RhoGui > Editor && vim_mode == normal",
        "RhoGui > Editor && vim_mode == helix_normal",
    ] {
        cx.bind_keys([
            KeyBinding::new("space", RootTransient, Some(context)),
            KeyBinding::new("q", SurfaceClose, Some(context)),
        ]);
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
    // `n` and `N` repeat the last search. Vim's own do nothing in this app:
    // they go through a pane's search bar and there is no pane, which is why
    // `/` is the host's in the first place. Bound in the context that
    // has a buffer search and nowhere else — a key means one thing per
    // context, and the Slack rooms keep their own `shift-n` for the next
    // unread by their own binding, not by being loaded after this one.
    for mode in ["normal", "helix_normal"] {
        let context =
            format!("RhoTranscript > Editor && vim_mode == {mode} && vim_operator == none");
        cx.bind_keys([
            KeyBinding::new("n", SearchRepeat, Some(&context)),
            KeyBinding::new("shift-n", SearchRepeatReverse, Some(&context)),
        ]);
    }
    // Slack reads the same way: `enter` opens the row or the thread the
    // cursor is on, `i` goes to the composer and `enter` there sends, `q`
    // closes, and `ctrl-k` (the surface-back key everywhere else) walks out
    // of a thread back to the channel it was opened from.
    // Rewriting what was sent: `up` on an empty composer is Slack's own
    // habit, and `escape` puts back whatever the composer held. Both fall
    // through to the editor's usual answer when no edit is open.
    // Each is qualified with `!showing_completions`, the way the agent
    // prompt's own `enter` is: with the completion menu open these keys
    // belong to it, so `up` walks the list, `escape` closes it, and `enter`
    // takes the name instead of posting half of it.
    cx.bind_keys([
        KeyBinding::new(
            "up",
            SlackEditLast,
            Some("RhoSlackConversation > Editor && vim_mode == insert && !showing_completions"),
        ),
        KeyBinding::new(
            "escape",
            SlackCancelEdit,
            Some("RhoSlackConversation > Editor && vim_mode == insert && !showing_completions"),
        ),
    ]);
    cx.bind_keys([
        // The results of a search: one place per hit. `enter` goes there,
        // `escape` and `q` leave the way any other surface is left, and
        // `shift-s` asks again without going back to the list first.
        KeyBinding::new("enter", SlackOpenFound, Some("RhoSlackResults > Editor")),
        KeyBinding::new(
            "[",
            SlackSearchPreviousPage,
            Some("RhoSlackResults > Editor"),
        ),
        KeyBinding::new("]", SlackSearchNextPage, Some("RhoSlackResults > Editor")),
        KeyBinding::new("escape", SurfaceClose, Some("RhoSlackResults > Editor")),
        KeyBinding::new("q", SurfaceClose, Some("RhoSlackResults > Editor")),
        KeyBinding::new(
            "shift-s",
            SlackFindMessage,
            Some("RhoSlackResults > Editor"),
        ),
        KeyBinding::new(
            "ctrl-shift-s",
            SlackFindFile,
            Some("RhoSlackResults > Editor"),
        ),
        KeyBinding::new("enter", SlackOpenRow, Some("RhoSlackList > Editor")),
        KeyBinding::new("enter", HomeOpenRow, Some("RhoHome > Editor")),
        KeyBinding::new(
            "enter",
            SubmitPrompt,
            Some("RhoSlackConversation > Editor && vim_mode == insert && !showing_completions"),
        ),
        // A message with a second line is written, not sent twice: the
        // newline is bound explicitly because the prompt's `RhoGui > Editor`
        // SubmitPrompt binding would otherwise swallow the key.
        KeyBinding::new(
            "shift-enter",
            editor::actions::Newline,
            Some("RhoSlackConversation > Editor && vim_mode == insert && !showing_completions"),
        ),
    ]);
    // Deal mode reports itself as normal (plus `VimDeal`), and these
    // contexts are deeper than the deal keys', so without the exclusion a
    // dealt conversation would answer `i` with the composer and `s` with
    // search instead of insert and snooze.
    for context in [
        "RhoSlackList > Editor && vim_mode == normal && !VimDeal",
        "RhoSlackList > Editor && vim_mode == helix_normal && !VimDeal",
        "RhoSlackConversation > Editor && vim_mode == normal && !VimDeal",
        "RhoSlackConversation > Editor && vim_mode == helix_normal && !VimDeal",
    ] {
        cx.bind_keys([
            KeyBinding::new("q", SurfaceClose, Some(context)),
            KeyBinding::new("s", SlackSearch, Some(context)),
            // `s` narrows the list by name; `shift-s` asks Slack what
            // people said. Two different questions, and the second one is
            // a request over a network rather than an index in memory.
            KeyBinding::new("shift-s", SlackFindMessage, Some(context)),
            KeyBinding::new("ctrl-shift-s", SlackFindFile, Some(context)),
            // The next conversation with something in it. `shift-n` and
            // not `n`, because `n` in a transcript is the search the reader
            // just ran.
            KeyBinding::new("ctrl-p", SlackQuickSwitch, Some(context)),
            KeyBinding::new("ctrl-n", SlackNewMessage, Some(context)),
            KeyBinding::new("shift-n", SlackNextUnread, Some(context)),
        ]);
    }
    for context in [
        "RhoSlackResults > Editor && vim_mode == normal",
        "RhoSlackResults > Editor && vim_mode == helix_normal",
    ] {
        cx.bind_keys([
            KeyBinding::new("ctrl-p", SlackQuickSwitch, Some(context)),
            KeyBinding::new("ctrl-n", SlackNewMessage, Some(context)),
        ]);
    }
    for context in ["RhoSlackConversation", "RhoSlackResults", "RhoSlackList"] {
        cx.bind_keys([
            KeyBinding::new("ctrl-w h", SlackSidebarFocus, Some(context)),
            KeyBinding::new("ctrl-w l", SlackConversationFocus, Some(context)),
        ]);
    }
    // Writing is a conversation's, not the list's: both of these take a key
    // vim has a use for, so on a surface where they do nothing they have to
    // give it back rather than swallow it.
    for context in [
        "RhoSlackConversation > Editor && vim_mode == normal && !VimDeal",
        "RhoSlackConversation > Editor && vim_mode == helix_normal && !VimDeal",
    ] {
        cx.bind_keys([
            KeyBinding::new("i", SlackCompose, Some(context)),
            KeyBinding::new("e", SlackEditMessage, Some(context)),
            // `r` on a message opens what to react with. On a line that is
            // not a message it gives the key back, the way `e` does.
            KeyBinding::new("r", SlackReactTo, Some(context)),
            KeyBinding::new("m", SlackMarkUnread, Some(context)),
            KeyBinding::new("b", SlackSaveForLater, Some(context)),
        ]);
    }
    // Marking the old backlog is a list-wide verb, so it lives on the list
    // and not inside a conversation, and so is watching the row under the
    // point.
    for context in [
        "RhoSlackList > Editor && vim_mode == normal && !VimDeal",
        "RhoSlackList > Editor && vim_mode == helix_normal && !VimDeal",
    ] {
        cx.bind_keys([KeyBinding::new("m", SlackMarkReadBefore, Some(context))]);
    }
    cx.bind_keys([
        KeyBinding::new(
            "enter",
            SlackOpenRow,
            Some("RhoSlackConversation > Editor && vim_mode == normal"),
        ),
        KeyBinding::new(
            "enter",
            SlackOpenRow,
            Some("RhoSlackConversation > Editor && vim_mode == helix_normal"),
        ),
    ]);
    cx.bind_keys([
        KeyBinding::new("shift-y", GitApprovalAllow, Some("RhoGitApproval")),
        KeyBinding::new("n", GitApprovalDeny, Some("RhoGitApproval")),
        KeyBinding::new("enter", GitApprovalDeny, Some("RhoGitApproval")),
        KeyBinding::new("escape", GitApprovalDeny, Some("RhoGitApproval")),
    ]);
    for context in [
        "RhoNote > Editor && vim_mode == normal && !VimDeal",
        "RhoNote > Editor && vim_mode == helix_normal && !VimDeal",
    ] {
        cx.bind_keys([KeyBinding::new("enter", NoteOpenRow, Some(context))]);
    }
    // A note body is text, so enter is a newline. Bound explicitly because
    // the transcript prompt's `RhoGui > Editor` SubmitPrompt binding would
    // otherwise win here and swallow the key.
    cx.bind_keys([KeyBinding::new(
        "enter",
        editor::actions::Newline,
        Some("RhoNote > Editor && vim_mode == insert"),
    )]);
    // The top of a transcript is the top of its history, and history is
    // composed as it is asked for, so `gg` here is the transcript's own:
    // it composes everything on the way. Anywhere else the action gives
    // the key back and vim keeps it. Not bound with an operator pending —
    // `y g g` stays vim's.
    for context in [
        "RhoGui > Editor && vim_mode == normal && vim_operator == none",
        "RhoGui > Editor && vim_mode == helix_normal && vim_operator == none",
    ] {
        cx.bind_keys([KeyBinding::new("g g", TranscriptTop, Some(context))]);
    }
    // Vim is vim on every surface: a card is read, searched and yanked like
    // any buffer, and the verdicts live in the transient `tab` opens.
    // `shift-u` stays a key of its own because undoing a verdict is
    // the same verb whether or not the card is still on screen.
    for context in [
        "RhoGui > Editor",
        "RhoSlackConversation > Editor",
        "RhoSlackList > Editor",
    ] {
        let context = format!(
            "{context} && vim_operator == none && (vim_mode == normal || vim_mode == helix_normal)"
        );
        cx.bind_keys([KeyBinding::new("shift-u", UndoVerdict, Some(&context))]);
    }
}

/// Initializes the modal editor engine and the exact keymap stack shared by
/// every Rho GUI frontend. Platform-specific actions remain harmless when
/// their corresponding surface is unavailable; their contexts never match.
pub fn init_vim_mode(cx: &mut App) -> anyhow::Result<()> {
    // Rho is Helix-first: force Helix on so no settings file can silently
    // drop the user back into plain Vim.
    let settings = cx.global_mut::<settings::SettingsStore>();
    settings.override_global(vim_mode_setting::VimModeSetting(false));
    settings.override_global(vim_mode_setting::HelixModeSetting(true));
    vim::init(cx);
    let default_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)?;
    cx.bind_keys(default_key_bindings);
    let vim_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::VIM_KEYMAP_PATH, cx)?;
    anyhow::ensure!(
        cx.build_action("vim::Left", None).is_ok(),
        "Vim actions are missing from the application registry"
    );
    anyhow::ensure!(
        vim_key_bindings
            .iter()
            .any(|binding| binding.action().name() == "vim::Left"),
        "the Vim keymap did not load its core motion bindings"
    );
    cx.bind_keys(vim_key_bindings);
    bind_rho_key_overrides(cx);
    Ok(())
}

#[cfg(test)]
mod slack_tests;
#[cfg(test)]
mod tests;

/// The dealer's policy constants as the journal records them at session start.
pub fn dealer_policy_snapshot() -> rho_journal::DealerPolicySnapshot {
    use rho_dealer::curve::*;
    rho_journal::DealerPolicySnapshot {
        queue_floor: DEAL_QUEUE_FLOOR,
        skip_cooldown_minutes: SKIP_COOLDOWN.num_minutes(),
        blocked_reply_head_start: BLOCKED_REPLY_HEAD_START,
        blocked_reply_slope_per_day: BLOCKED_REPLY_SLOPE_PER_DAY,
        fyi_reply_pace_days: FYI_REPLY_PACE_DAYS,
        thread_reply_head_start: THREAD_REPLY_HEAD_START,
        channel_traffic_head_start: CHANNEL_TRAFFIC_HEAD_START,
        channel_answered_drop: CHANNEL_ANSWERED_DROP,
        lamp_threshold: LAMP_THRESHOLD,
        chime_threshold: CHIME_THRESHOLD,
        agent_recency_bonus: AGENT_RECENCY_BONUS,
        agent_recency_window_ms: AGENT_RECENCY_WINDOW_MS,
    }
}

pub mod wayland_view;
