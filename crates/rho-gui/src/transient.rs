//! Magit-style menus: the keyboard-first command layer.
//!
//! A menu is data — a title and rows of (key, label, action) — drawn as a
//! block under the point while it captures the keyboard. A key either runs a
//! command and closes, opens a nested menu, or drops into the minibuffer for
//! a value. The full menu appears immediately and stays up for toggles.
//! There is no textual command grammar — commands are Rust values, the menus
//! are how fingers reach them.
//!
//! The menus here are values and nothing else: an item is a `MenuAction`,
//! never a closure and never a series of data. What a menu means is decided
//! by `Workspace::run_menu_action`, and what a screen draws is the screen's
//! own business.

use crate::workspace::Subject;

/// What a menu item does, as a value the window hands back rather than a
/// closure over the workspace. `rho_window::transient` never learns what any
/// of these mean; the matches in `Workspace::run_menu_action` are the only
/// places that do.
///
/// Why a value at all, when a closure would be shorter: a closure over the
/// workspace can only be written by something that already has the
/// workspace, which is every screen and no source crate. An item that is
/// data can be put in a menu by the crate that owns the thing it acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MenuAction {
    /// Open this menu in place of the one on screen, over the same row.
    /// Escape goes back to it: a submenu is a step, not a new place.
    Open(MenuId),
    /// A verdict on the card under the point.
    Verdict(VerdictAction),
    /// Everything else: one command, named.
    Command(Command),
}

/// A menu by name, so an item can reach another menu without building it.
/// The building is `Workspace::open_menu_by_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuId {
    Slack,
    Hosts,
    Projects,
    /// The snooze units under the verdicts (`s` then `m`, `h`, `d`, `w`).
    Verdict,
    VerdictSnooze,
    Input,
    Agent,
    New,
    Status,
    /// `space s u`: which usage chart to look at.
    UsageRoot,
    PhoneTools,
    PhoneFiling,
    Editing,
}

/// A command a menu item runs, which is the whole of what the item means.
/// One variant per item and no arguments beyond what the item itself says,
/// so the match that runs them reads as the list of what the menus can do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    // The root menu.
    AllCommands,
    Edit(EditCommand),
    FindText,
    Voice,
    SwitchBuffer,
    MessageLog,
    SurfaceBack,
    SurfaceForward,
    FileSave,
    PullCard,
    CloseAndDeal,
    OpenFile,
    FindNode,
    NotesForThis,
    Shell,
    ShellClose,
    Wayland,
    Terminal,
    NewTerminal,
    UndoVerdict,
    Quit,
    /// Home, from the verdict menu's own `tab` row.
    Home,
    AgentActivity,
    AgentConversation,
    // Slack.
    /// Put this emoji on the message under the point, or take the
    /// reader's own off if it is already there. Slack's shortcode without
    /// the colons, which is what the API takes and what the emoji table
    /// keys on — and what the menu row shows beside the glyph, so a
    /// reader learns the name of the thing they keep pressing.
    SlackReact(gpui::SharedString),
    /// Any emoji at all, typed by name in the minibuffer.
    SlackReactByName,
    SlackConversations,
    SlackAttach,
    SlackSwitch,
    SlackPeople,
    SlackBrowse,
    SlackFind,
    SlackFiles,
    SlackActivity,
    SlackSaved,
    SlackDrafts,
    SlackDetach,
    SlackMessageActions,
    SlackBroadcast,
    SlackFavorite,
    SlackFollow,
    SlackMarkReadBefore,
    SlackMarkUnread,
    SlackSaveForLater,
    SlackRegister,
    SlackMessageEdit(rho_slack::types::Ts),
    SlackMessageDelete(rho_slack::types::Ts),
    SlackMessageReact(rho_slack::types::Ts),
    SlackMessageCopyLink(rho_slack::types::Ts),
    SlackMessageForward(rho_slack::types::Ts),
    // Hosts.
    HostsList,
    HostAttach,
    HostDetach,
    HostAuth,
    // Projects.
    ProjectAdd,
    ProjectRemove,
    // Input.
    EndVoice,
    PastePrompt,
    ClearPromptImages,
    // New: creation, one verb.
    NewAgent,
    NewPage,
    BrowserType,
    NewNote,
    // Status.
    /// One of the usage screen's charts, over that many days.
    Usage(crate::usage::Chart, u64),
    UploadTelemetry,
    Version,
    // The note or label in view.
    DeleteMade,
    MoveLabel,
    SecretPhrase,
    // The agent under the point.
    AgentCancel,
    AgentRole,
    VerdictName,
    AgentCompact,
    AgentRewind,
    AgentRewindMany,
    AgentContinue,
    AgentCacheKey,
    AgentRestartWorkset,
    // The phone.
    /// A distance ahead, the sizes a thumb picks.
    PhoneSnoozeAhead(crate::workspace::SnoozeUnit, usize),
    /// A named hour of the day: `tonight` is this evening while it is still
    /// ahead and the next one after that, `tomorrow` is always the next day.
    PhoneSnoozeAt {
        hour: u32,
        tomorrow: bool,
    },
}

/// Direct editing without modal key sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditCommand {
    Undo,
    Redo,
    Copy,
    Cut,
    Paste,
    SelectAll,
    SelectWord,
    Top,
    Bottom,
    PageUp,
    PageDown,
}

/// What a verdict item does. Kept apart from [`Command`] because a verdict
/// is the one action that lands on the card under the point rather than on
/// the workspace, and because the count belongs to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerdictAction {
    Done,
    Mute,
    RoomSnooze,
    Todo,
    File,
    Undo,
    Pull,
    WrongCard,
    Pile,
    /// A unit from the snooze menu. `None` is the bare `s` that has always
    /// meant a day whatever the count.
    Snooze(Option<crate::workspace::SnoozeUnit>),
}

/// A menu: a block under the point, `rho_window::transient` with this
/// application's own values in it.
pub(crate) type Menu = rho_window::transient::Transient<MenuAction>;

/// `tab` — the verdicts, on the card under the point. Deal mode used to
/// take `d`, `x`, `s`, `t` and `f` from every dealt surface, so a card could
/// not be read, searched or yanked like the buffer it is; the keys live here
/// instead and vim keeps its own everywhere. `tab` again is Home. It was a
/// tap of `shift` until the user found the modifier distracting.
pub(crate) fn verdict_menu() -> Menu {
    Menu::new("verdict")
        .item("d", "done", MenuAction::Verdict(VerdictAction::Done))
        .item("x", "mute", MenuAction::Verdict(VerdictAction::Mute))
        .item("s", "snooze…", MenuAction::Open(MenuId::VerdictSnooze))
        .item(
            "shift-s",
            "snooze the room…",
            MenuAction::Verdict(VerdictAction::RoomSnooze),
        )
        .item("p", "pile…", MenuAction::Verdict(VerdictAction::Pile))
        .item("t", "todo", MenuAction::Verdict(VerdictAction::Todo))
        .item("f", "file…", MenuAction::Verdict(VerdictAction::File))
        .item("n", "name…", MenuAction::Command(Command::VerdictName))
        .item(
            "w",
            "wrong card…",
            MenuAction::Verdict(VerdictAction::WrongCard),
        )
        .item(
            "u",
            "undo the last verdict",
            MenuAction::Verdict(VerdictAction::Undo),
        )
        .item(
            "j",
            "open the top card",
            MenuAction::Verdict(VerdictAction::Pull),
        )
        .item("tab", "home", MenuAction::Command(Command::Home))
        .counted()
}

/// The snooze unit, after a count: `45 m`, `3 h`, `7 d`, `w`, and `s` for
/// the day the bare key used to mean.
pub(crate) fn verdict_snooze_menu() -> Menu {
    use crate::workspace::SnoozeUnit;
    Menu::new("snooze")
        .item(
            "s",
            "a day",
            MenuAction::Verdict(VerdictAction::Snooze(None)),
        )
        .item(
            "m",
            "minutes",
            MenuAction::Verdict(VerdictAction::Snooze(Some(SnoozeUnit::Minutes))),
        )
        .item(
            "h",
            "hours",
            MenuAction::Verdict(VerdictAction::Snooze(Some(SnoozeUnit::Hours))),
        )
        .item(
            "d",
            "days",
            MenuAction::Verdict(VerdictAction::Snooze(Some(SnoozeUnit::Days))),
        )
        .item(
            "w",
            "weeks",
            MenuAction::Verdict(VerdictAction::Snooze(Some(SnoozeUnit::Weeks))),
        )
        .counted()
}

/// `space` — the root menu: every leader chord lives here (or one level
/// down), so the whole vocabulary is discoverable by pausing.
///
/// The subject is read once, here, rather than filtered afterwards: an item
/// with nothing to act on is not in the menu the reader sees.
pub(crate) fn root_menu(subject: &Subject) -> Menu {
    Menu::new("rho")
        .item("i", "input…", MenuAction::Open(MenuId::Input))
        .item(
            "m",
            "voice microphone · mute/unmute",
            MenuAction::Command(Command::Voice),
        )
        .when(
            subject.has_agent(),
            "a",
            "agent…",
            MenuAction::Open(MenuId::Agent),
        )
        .item(
            "b",
            "switch buffer…",
            MenuAction::Command(Command::SwitchBuffer),
        )
        // The echo area keeps two seconds; the log keeps everything it
        // said. Reachable by no key at all until now, which made every
        // notice that scrolled past unrecoverable.
        .item("l", "message log", MenuAction::Command(Command::MessageLog))
        .item(
            "k",
            "surface back",
            MenuAction::Command(Command::SurfaceBack),
        )
        .item(
            "j",
            "open the top card",
            MenuAction::Command(Command::PullCard),
        )
        .item(
            "shift-j",
            "close · deal",
            MenuAction::Command(Command::CloseAndDeal),
        )
        .item("f", "open file…", MenuAction::Command(Command::OpenFile))
        .item(
            "shift-f",
            "find node…",
            MenuAction::Command(Command::FindNode),
        )
        .item("n", "new…", MenuAction::Open(MenuId::New))
        .when(
            subject.has_made(),
            "d",
            "delete this",
            MenuAction::Command(Command::DeleteMade),
        )
        .when(
            subject.has_label(),
            "r",
            "rename/move label…",
            MenuAction::Command(Command::MoveLabel),
        )
        .item(
            "shift-n",
            "notes for this",
            MenuAction::Command(Command::NotesForThis),
        )
        .item(
            "c",
            "start/attach shell",
            MenuAction::Command(Command::Shell),
        )
        .item(
            "shift-c",
            "close shell",
            MenuAction::Command(Command::ShellClose),
        )
        .item("w", "agent desktop", MenuAction::Command(Command::Wayland))
        .item("t", "terminal", MenuAction::Command(Command::Terminal))
        .item(
            "shift-t",
            "new terminal",
            MenuAction::Command(Command::NewTerminal),
        )
        .item("p", "projects…", MenuAction::Open(MenuId::Projects))
        .item("h", "hosts…", MenuAction::Open(MenuId::Hosts))
        .item("s", "status…", MenuAction::Open(MenuId::Status))
        .item(
            "shift-u",
            "undo verdict",
            MenuAction::Command(Command::UndoVerdict),
        )
        .item("shift-s", "slack…", MenuAction::Open(MenuId::Slack))
        .item("q", "quit", MenuAction::Command(Command::Quit))
}

/// The keys a reaction menu hands out, in the order the rows are read.
/// The home row first because that is where the fingers are; nothing
/// mnemonic is possible when the rows are emoji.
const REACTION_KEYS: [&str; 12] = ["a", "s", "d", "f", "g", "h", "j", "k", "l", "q", "w", "e"];

/// `r` on a message: what to react with.
///
/// Two groups, in the order a reader wants them. What is already on the
/// message comes first, because joining a reaction is the commonest thing
/// anyone does with one, and a row for one the reader already has says
/// "remove" — the key is one state, not two. Then what this reader
/// reaches for, most recent first. Anything else is `/`, by name.
///
/// Cost: one row per choice, and the choices are what the menu draws.
pub(crate) fn slack_react_menu(choices: &rho_slack::ui::ReactionChoices) -> Menu {
    let mut menu = Menu::new("react");
    let rows = choices
        .on_message
        .iter()
        .chain(choices.recent.iter())
        .take(REACTION_KEYS.len());
    for (key, choice) in REACTION_KEYS.iter().zip(rows) {
        let description = match choice.mine {
            true => format!("{} {} — remove", choice.glyph, choice.name),
            false => format!("{} {}", choice.glyph, choice.name),
        };
        menu = menu.item(
            *key,
            description,
            MenuAction::Command(Command::SlackReact(choice.name.clone().into())),
        );
    }
    menu.item(
        "/",
        "by name…",
        MenuAction::Command(Command::SlackReactByName),
    )
}

pub(crate) fn slack_message_menu(actions: &rho_slack::ui::conversation::MessageActions) -> Menu {
    let ts = actions.ts.clone();
    let mut menu = Menu::new("message");
    if actions.can_edit {
        menu = menu.item(
            "e",
            "edit",
            MenuAction::Command(Command::SlackMessageEdit(ts.clone())),
        );
    }
    menu = menu.item(
        "r",
        "react…",
        MenuAction::Command(Command::SlackMessageReact(ts.clone())),
    );
    if actions.can_copy_link {
        menu = menu.item(
            "c",
            "copy link",
            MenuAction::Command(Command::SlackMessageCopyLink(ts.clone())),
        );
    }
    if actions.can_forward {
        menu = menu.item(
            "f",
            "forward…",
            MenuAction::Command(Command::SlackMessageForward(ts.clone())),
        );
    }
    if actions.can_delete {
        menu = menu.item(
            "d",
            "delete…",
            MenuAction::Command(Command::SlackMessageDelete(ts)),
        );
    }
    menu.item(
        "u",
        "mark unread",
        MenuAction::Command(Command::SlackMarkUnread),
    )
    .item(
        "s",
        "save / unsave for later (local)",
        MenuAction::Command(Command::SlackSaveForLater),
    )
}

pub(crate) fn slack_menu() -> Menu {
    Menu::new("slack")
        .item(
            "e",
            "message actions…",
            MenuAction::Command(Command::SlackMessageActions),
        )
        .item(
            "o",
            "conversations",
            MenuAction::Command(Command::SlackConversations),
        )
        .item("j", "jump…", MenuAction::Command(Command::SlackSwitch))
        .item(
            "n",
            "new message…",
            MenuAction::Command(Command::SlackPeople),
        )
        .item(
            "c",
            "browse channels…",
            MenuAction::Command(Command::SlackBrowse),
        )
        .item(
            "s",
            "search messages…",
            MenuAction::Command(Command::SlackFind),
        )
        .item(
            "f",
            "search files…",
            MenuAction::Command(Command::SlackFiles),
        )
        .item("v", "activity", MenuAction::Command(Command::SlackActivity))
        .item(
            "l",
            "saved (local)",
            MenuAction::Command(Command::SlackSaved),
        )
        .item("d", "drafts", MenuAction::Command(Command::SlackDrafts))
        .item(
            "x",
            "remove attachment…",
            MenuAction::Command(Command::SlackDetach),
        )
        .item(
            "b",
            "broadcast reply",
            MenuAction::Command(Command::SlackBroadcast),
        )
        .item(
            "z",
            "toggle favorite",
            MenuAction::Command(Command::SlackFavorite),
        )
        .item(
            "t",
            "follow / unfollow",
            MenuAction::Command(Command::SlackFollow),
        )
        .item(
            "a",
            "attach file…",
            MenuAction::Command(Command::SlackAttach),
        )
        .item(
            "m",
            "mark read before…",
            MenuAction::Command(Command::SlackMarkReadBefore),
        )
        .item(
            "r",
            "register workspace…",
            MenuAction::Command(Command::SlackRegister),
        )
}

/// `space h`: the attached agent hosts. Attaching and detaching are rare, so
/// they live one level down rather than on the root's crowded first row.
pub(crate) fn hosts_menu() -> Menu {
    Menu::new("hosts")
        .item("l", "list", MenuAction::Command(Command::HostsList))
        .item("a", "attach…", MenuAction::Command(Command::HostAttach))
        .item("d", "detach…", MenuAction::Command(Command::HostDetach))
        .item("u", "auth…", MenuAction::Command(Command::HostAuth))
}

/// Creation, the one verb: everything new starts here and is filed where
/// the area picker's first row already points.
pub(crate) fn new_menu() -> Menu {
    Menu::new("new")
        .item("a", "agent…", MenuAction::Command(Command::NewAgent))
        .item("p", "page…", MenuAction::Command(Command::NewPage))
        .item("n", "note…", MenuAction::Command(Command::NewNote))
}

pub(crate) fn input_menu() -> Menu {
    Menu::new("input")
        .item(
            "m",
            "voice microphone · mute/unmute",
            MenuAction::Command(Command::Voice),
        )
        .item(
            "e",
            "voice session · end",
            MenuAction::Command(Command::EndVoice),
        )
        .item(
            "p",
            "paste clipboard",
            MenuAction::Command(Command::PastePrompt),
        )
        .item(
            "c",
            "clear images",
            MenuAction::Command(Command::ClearPromptImages),
        )
}

pub(crate) fn status_menu() -> Menu {
    Menu::new("status")
        .item(
            "p",
            "upload GUI performance snapshot",
            MenuAction::Command(Command::UploadTelemetry),
        )
        .item("u", "usage…", MenuAction::Open(MenuId::UsageRoot))
        .item("v", "version", MenuAction::Command(Command::Version))
        .item(
            "s",
            "secret phrase…",
            MenuAction::Command(Command::SecretPhrase),
        )
}

/// `space a`: driving the current conversation.
pub(crate) fn agent_menu() -> Menu {
    Menu::new("agent")
        .item("a", "activity", MenuAction::Command(Command::AgentActivity))
        .item(
            "m",
            "conversation",
            MenuAction::Command(Command::AgentConversation),
        )
        .item(
            "c",
            "cancel turn",
            MenuAction::Command(Command::AgentCancel),
        )
        .item("r", "role…", MenuAction::Command(Command::AgentRole))
        .item("k", "compact", MenuAction::Command(Command::AgentCompact))
        .item(
            "w",
            "rewind turn",
            MenuAction::Command(Command::AgentRewind),
        )
        .item(
            "shift-w",
            "rewind turns…",
            MenuAction::Command(Command::AgentRewindMany),
        )
        .item(
            "shift-c",
            "continue turn",
            MenuAction::Command(Command::AgentContinue),
        )
        .item(
            "shift-k",
            "new prompt cache key",
            MenuAction::Command(Command::AgentCacheKey),
        )
        .item(
            "shift-r",
            "restart workset",
            MenuAction::Command(Command::AgentRestartWorkset),
        )
}

pub(crate) fn phone_root_menu(subject: &Subject) -> Menu {
    Menu::new("Rho")
        .item(
            "/",
            "Find a command…",
            MenuAction::Command(Command::AllCommands),
        )
        .item("n", "New…", MenuAction::Open(MenuId::New))
        .item(
            "b",
            "Buffers & agents…",
            MenuAction::Command(Command::SwitchBuffer),
        )
        .item(
            "f",
            "Find notes, agents & conversations…",
            MenuAction::Command(Command::FindNode),
        )
        .item("h", "Home", MenuAction::Command(Command::Home))
        .item("s", "Slack…", MenuAction::Open(MenuId::Slack))
        .when(
            subject.has_agent(),
            "a",
            "Agent actions…",
            MenuAction::Open(MenuId::Agent),
        )
        .item(
            "e",
            "Edit & search this buffer…",
            MenuAction::Open(MenuId::Editing),
        )
        .item(
            "w",
            "Files, browser, shell & desktop…",
            MenuAction::Open(MenuId::PhoneTools),
        )
        .item(
            "l",
            "Notes & filing…",
            MenuAction::Open(MenuId::PhoneFiling),
        )
        .item("p", "Projects…", MenuAction::Open(MenuId::Projects))
        .item("o", "Hosts…", MenuAction::Open(MenuId::Hosts))
        .item("i", "Voice & input…", MenuAction::Open(MenuId::Input))
        .item(
            "m",
            "Message history",
            MenuAction::Command(Command::MessageLog),
        )
        .item(
            "u",
            "Undo verdict",
            MenuAction::Command(Command::UndoVerdict),
        )
        .item("j", "Next card", MenuAction::Command(Command::PullCard))
        .item("v", "Status & usage…", MenuAction::Open(MenuId::Status))
        .item(
            "d",
            "Card actions & counts…",
            MenuAction::Open(MenuId::Verdict),
        )
}

pub(crate) fn phone_tools_menu() -> Menu {
    Menu::new("Work")
        .item("f", "Open file…", MenuAction::Command(Command::OpenFile))
        .item("s", "Save file", MenuAction::Command(Command::FileSave))
        .item(
            "p",
            "New browser page…",
            MenuAction::Command(Command::NewPage),
        )
        .item(
            "i",
            "Type in browser field…",
            MenuAction::Command(Command::BrowserType),
        )
        .item(
            "c",
            "Start / attach shell",
            MenuAction::Command(Command::Shell),
        )
        .item(
            "shift-c",
            "Close shell",
            MenuAction::Command(Command::ShellClose),
        )
        .item("t", "Terminal", MenuAction::Command(Command::Terminal))
        .item(
            "shift-t",
            "New terminal",
            MenuAction::Command(Command::NewTerminal),
        )
        .item("w", "Agent desktop…", MenuAction::Command(Command::Wayland))
        .item(
            "k",
            "Previous surface",
            MenuAction::Command(Command::SurfaceBack),
        )
        .item(
            "shift-k",
            "Forward surface",
            MenuAction::Command(Command::SurfaceForward),
        )
        .item(
            "j",
            "Close buffer & deal",
            MenuAction::Command(Command::CloseAndDeal),
        )
        .item("q", "Quit Rho", MenuAction::Command(Command::Quit))
}

pub(crate) fn phone_filing_menu() -> Menu {
    Menu::new("Notes & filing")
        .item("n", "New note…", MenuAction::Command(Command::NewNote))
        .item(
            "f",
            "Find note or label…",
            MenuAction::Command(Command::FindNode),
        )
        .item(
            "shift-n",
            "Notes for this",
            MenuAction::Command(Command::NotesForThis),
        )
        .item(
            "r",
            "Rename / move label…",
            MenuAction::Command(Command::MoveLabel),
        )
        .item(
            "d",
            "Delete this…",
            MenuAction::Command(Command::DeleteMade),
        )
}

pub(crate) fn editing_menu() -> Menu {
    use EditCommand::*;
    let mut menu = Menu::new("Edit");
    for (key, label, action) in [
        ("u", "Undo edit", Undo),
        ("r", "Redo edit", Redo),
        ("c", "Copy", Copy),
        ("x", "Cut", Cut),
        ("p", "Paste", Paste),
        ("a", "Select all", SelectAll),
        ("w", "Extend selection by word", SelectWord),
        ("g", "Beginning", Top),
        ("shift-g", "End", Bottom),
        ("k", "Page up", PageUp),
        ("j", "Page down", PageDown),
    ] {
        menu = menu.item(key, label, MenuAction::Command(Command::Edit(action)));
    }
    menu.item("/", "Find text…", MenuAction::Command(Command::FindText))
}

/// The phone's answer to "how long": the times a thumb picks, where a
/// keyboard types a count and a unit. `tonight` and `tomorrow` name an hour
/// of the day, so they land on the clock and not on a distance from now.
pub(crate) fn snooze_sheet() -> Menu {
    use crate::workspace::SnoozeUnit;
    Menu::new("snooze")
        .item(
            "m",
            "30m",
            MenuAction::Command(Command::PhoneSnoozeAhead(SnoozeUnit::Minutes, 30)),
        )
        .item(
            "h",
            "2h",
            MenuAction::Command(Command::PhoneSnoozeAhead(SnoozeUnit::Hours, 2)),
        )
        .item(
            "t",
            "tonight (18:00)",
            MenuAction::Command(Command::PhoneSnoozeAt {
                hour: 18,
                tomorrow: false,
            }),
        )
        .item(
            "shift-t",
            "tomorrow (09:00)",
            MenuAction::Command(Command::PhoneSnoozeAt {
                hour: 9,
                tomorrow: true,
            }),
        )
        .item(
            "d",
            "3d",
            MenuAction::Command(Command::PhoneSnoozeAhead(SnoozeUnit::Days, 3)),
        )
        .item(
            "w",
            "next week",
            MenuAction::Command(Command::PhoneSnoozeAhead(SnoozeUnit::Weeks, 1)),
        )
        .item(
            "c",
            "Custom duration…",
            MenuAction::Open(MenuId::VerdictSnooze),
        )
}

pub(crate) fn projects_menu() -> Menu {
    Menu::new("projects")
        .item("a", "add…", MenuAction::Command(Command::ProjectAdd))
        .item("r", "remove…", MenuAction::Command(Command::ProjectRemove))
}

/// `space s u`: the usage screen, in the two windows worth looking at.
/// Every item is the same command with a different picture and a different
/// number of days — the screen is one surface, and picking again redraws it.
pub(crate) fn usage_root_menu() -> Menu {
    use crate::usage::Chart;
    Menu::new("usage")
        .item(
            "r",
            "rate limit · 7d",
            MenuAction::Command(Command::Usage(Chart::RateLimit, 7)),
        )
        .item(
            "shift-r",
            "rate limit · 30d",
            MenuAction::Command(Command::Usage(Chart::RateLimit, 30)),
        )
        .item(
            "c",
            "model cost · 7d",
            MenuAction::Command(Command::Usage(Chart::ModelCost, 7)),
        )
        .item(
            "shift-c",
            "model cost · 30d",
            MenuAction::Command(Command::Usage(Chart::ModelCost, 30)),
        )
        .item(
            "s",
            "model usage share · 7d",
            MenuAction::Command(Command::Usage(Chart::UsageShare, 7)),
        )
        .item(
            "shift-s",
            "model usage share · 30d",
            MenuAction::Command(Command::Usage(Chart::UsageShare, 30)),
        )
        .item(
            "a",
            "GPT agent cost · 7d",
            MenuAction::Command(Command::Usage(Chart::AgentCost, 7)),
        )
        .item(
            "shift-a",
            "GPT agent cost · 30d",
            MenuAction::Command(Command::Usage(Chart::AgentCost, 30)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phone_snooze_sheet_offers_the_decided_chips() {
        assert_eq!(
            snooze_sheet()
                .items()
                .iter()
                .map(rho_window::transient::Item::description)
                .collect::<Vec<_>>(),
            [
                "30m",
                "2h",
                "tonight (18:00)",
                "tomorrow (09:00)",
                "3d",
                "next week",
                "Custom duration…"
            ]
        );
    }

    #[test]
    fn leader_keeps_usage_under_status() {
        let root = root_menu(&Subject::default());
        assert!(
            root.items()
                .iter()
                .any(|item| { item.key() == "s" && item.description() == "status…" })
        );
        assert!(
            !root
                .items()
                .iter()
                .any(|item| item.description() == "usage…")
        );

        let status = status_menu();
        assert!(
            status
                .items()
                .iter()
                .any(|item| { item.key() == "u" && item.description() == "usage…" })
        );
    }
}
