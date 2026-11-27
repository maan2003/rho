//! The window's own commands (`window.Commands`): those about the window,
//! the deal and this device rather than about any system's things.

use gpui::{Context, Window};
use rho_dealer::Address;
use rho_window::style::StyleClass;

use crate::attention::Verdict;
use crate::command::{Command, Spelling};
use crate::frame::Frame;
use crate::keys;
use crate::prefix::{Prefix, Row};
use crate::when::{CLEAR_SNOOZE, SNOOZE_PRESETS, SNOOZED_PRESETS, read_snooze, snooze_preview};

pub static WRITE_IDEA: Spelling = Spelling {
    name: "write idea",
    aliases: &[
        ("idea", 9),
        ("new idea", 9),
        ("new thread", 7),
        ("start agent", 6),
        ("compose", 5),
        ("ask", 4),
    ],
    argument: None,
};

pub static DEAL_NEXT: Spelling = Spelling {
    name: "deal next",
    aliases: &[("deal", 9), ("next card", 8), ("next", 6), ("skip", 5)],
    argument: None,
};

pub static CLOSE_AND_DEAL: Spelling = Spelling {
    name: "close and deal",
    aliases: &[("close deal", 8), ("close next", 6)],
    argument: None,
};

pub static DONE: Spelling = Spelling {
    name: "done",
    aliases: &[
        ("dismiss", 7),
        ("archive", 6),
        ("read", 5),
        ("clear card", 5),
    ],
    argument: None,
};

pub static MUTE: Spelling = Spelling {
    name: "mute",
    aliases: &[("silence", 8), ("ignore", 6), ("hush", 5)],
    argument: None,
};

/// What `snooze thread` and `snooze note` take after their names.
const HOW_LONG: &str = "how long: 45m, 3h, evening, tomorrow, next week or mon 9am";

pub static SNOOZE_THREAD: Spelling = Spelling {
    name: "snooze thread",
    aliases: &[
        ("snooze", 9),
        ("later", 7),
        ("defer", 7),
        ("postpone", 7),
        ("remind me", 6),
        ("sleep", 5),
    ],
    argument: Some(HOW_LONG),
};

pub static SNOOZE_NOTE: Spelling = Spelling {
    name: "snooze note",
    aliases: &[
        ("snooze", 8),
        ("reminder", 8),
        ("remind me", 5),
        ("later", 6),
        ("defer", 6),
    ],
    argument: Some(HOW_LONG),
};

pub static APPROVE_PUSH: Spelling = Spelling {
    name: "approve push",
    aliases: &[("approve", 9), ("allow push", 7), ("approve fetch", 6)],
    argument: None,
};

/// What may follow `deny push`: the thread's draft, which goes to its
/// agents with the denial.
const DENY_WORDS: &str = "the draft's words, which go with the denial";

pub static DENY_PUSH: Spelling = Spelling {
    name: "deny push",
    aliases: &[
        ("deny", 9),
        ("refuse push", 7),
        ("reject push", 7),
        ("deny fetch", 6),
    ],
    argument: Some(DENY_WORDS),
};

pub static UNDO: Spelling = Spelling {
    name: "undo",
    aliases: &[
        ("undo verdict", 9),
        ("take back", 7),
        ("undone", 5),
        ("oops", 4),
    ],
    argument: None,
};

pub static OPEN_PLACES: Spelling = Spelling {
    name: "open places",
    aliases: &[
        ("places", 9),
        ("go to", 7),
        ("find place", 6),
        ("switch to", 5),
    ],
    argument: None,
};

pub static OPEN_DESK: Spelling = Spelling {
    name: "open desk",
    aliases: &[("desk", 9), ("home", 6), ("what waits", 5)],
    argument: None,
};

pub static CLOSE_SCREEN: Spelling = Spelling {
    name: "close screen",
    aliases: &[("close", 9), ("close page", 6)],
    argument: None,
};

pub static UNSNOOZE_THREAD: Spelling = Spelling {
    name: "unsnooze thread",
    aliases: &[
        ("unsnooze", 9),
        ("wake", 8),
        ("wake up", 6),
        ("back now", 5),
    ],
    argument: None,
};

pub static UNSNOOZE_NOTE: Spelling = Spelling {
    name: "unsnooze note",
    aliases: &[
        ("unsnooze", 8),
        ("clear reminder", 9),
        ("remove reminder", 7),
        ("wake", 7),
    ],
    argument: None,
};

pub static TOGGLE_NOTCH_MASK: Spelling = Spelling {
    name: "toggle notch mask",
    aliases: &[
        ("notch mask", 9),
        ("notch", 8),
        ("hide notch", 6),
        ("show notch", 6),
    ],
    argument: None,
};

pub static SHOW_MESSAGES: Spelling = Spelling {
    name: "show messages",
    aliases: &[
        ("messages", 9),
        ("echo history", 7),
        ("recent messages", 7),
        ("what did it say", 4),
    ],
    argument: None,
};

pub static UNMUTE: Spelling = Spelling {
    name: "unmute",
    aliases: &[("unsilence", 7), ("hear again", 5), ("listen", 4)],
    argument: None,
};

pub static PAIR_DEVICE: Spelling = Spelling {
    name: "pair device",
    aliases: &[("pair", 9), ("sync", 6), ("link device", 6), ("phone", 4)],
    argument: None,
};

pub static SHOW_SECRET_PHRASE: Spelling = Spelling {
    name: "show secret phrase",
    aliases: &[
        ("secret", 8),
        ("phrase", 8),
        ("recovery words", 5),
        ("pairing words", 5),
    ],
    argument: None,
};

pub static QUIT: Spelling = Spelling {
    name: "quit",
    aliases: &[("exit", 9), ("close rho", 7), ("close window", 5)],
    argument: None,
};

/// The keys `space` binds over the screen in view (`window.Screen`):
/// everywhere `f`; `d` on a waiting card, `m` on an open thread, `s` on a
/// thread that waits or sleeps and on a note, `a` and `x` on a push in
/// view, and `u` while there is a verdict to take back. Each of these keys
/// that does nothing here says why instead.
pub(crate) fn prefix(frame: &Frame, cx: &mut gpui::App) -> Prefix {
    let mut prefix = Prefix::default();
    prefix.push(Row::run("f", "places", |frame, window, cx| {
        frame.places(window, cx)
    }));
    let push = frame.push_here(cx);
    let refused = push
        .as_ref()
        .and_then(|_| frame.own_card(cx))
        .and_then(|card| frame.push_refused(&card, cx));
    match (&push, refused) {
        (Some(_), Some(reason)) => {
            prefix.refuse("a", reason.clone());
            prefix.refuse("x", reason);
        }
        (Some(rows), None) => {
            if let Some(approve) = rows.iter().find(|row| row.key == "a") {
                prefix.push(Row {
                    word: "approve".to_owned(),
                    ..approve.clone()
                });
            }
            prefix.push(Row::run("x", "deny", |frame, window, cx| {
                frame.deny_prompt(window, cx)
            }));
        }
        (None, _) => {
            prefix.refuse("a", "no push here");
            prefix.refuse("x", "no push here");
        }
    }
    let note = frame.reminder_in_view(cx).is_some();
    match frame.waiting_card(cx) {
        Some(_) if push.is_some() => prefix.refuse("d", PUSH_WAITS),
        Some(_) => prefix.push(Row::run("d", "done", |frame, window, cx| {
            frame.done_here(window, cx)
        })),
        None => prefix.refuse("d", "not waiting here"),
    }
    match frame.thread_here(cx) {
        _ if note => prefix.refuse("m", "a note is never muted"),
        Some(_) if push.is_some() => prefix.refuse("m", PUSH_WAITS),
        Some(_) => prefix.push(Row::run("m", "mute", |frame, window, cx| {
            frame.mute_here(window, cx)
        })),
        None => prefix.refuse("m", "no thread here"),
    }
    let snooze = |spelling: &'static Spelling| {
        Row::run("s", "snooze", move |frame, window, cx| {
            frame.commands_prompt_at(&format!("{} ", spelling.name), window, cx)
        })
    };
    if note {
        prefix.push(snooze(&SNOOZE_NOTE));
    } else if frame.snoozable_thread(cx).is_some() {
        prefix.push(snooze(&SNOOZE_THREAD));
    } else {
        prefix.refuse("s", "nothing to snooze here");
    }
    match frame.attention.can_undo() {
        true => prefix.push(Row::run("u", "undo", |frame, window, cx| {
            frame.undo_verdict(window, cx)
        })),
        false => prefix.refuse("u", "nothing to undo"),
    }
    prefix
}

/// Why done and mute do nothing on a push: it ends when it is answered.
const PUSH_WAITS: &str = "a push waits · space a approves, space x denies";

impl Frame {
    /// The thread in view, waiting or not; no note.
    pub(crate) fn thread_here(&self, cx: &gpui::App) -> Option<Address> {
        self.card_in_view(cx)
            .filter(|_| self.reminder_in_view(cx).is_none())
    }

    /// The answers a push on screen takes, from the system that staged it:
    /// only on the push's own thread, never from a list or another screen.
    /// A push the user can read is theirs to answer, even on a thread they
    /// muted or snoozed, which deals it to nobody.
    pub(crate) fn push_here(&self, cx: &mut gpui::App) -> Option<Vec<Row>> {
        let target = self.own_card(cx)?;
        self.own_verdicts(&target, cx)
    }

    /// Why the push at `address` takes no answer now: its host is away,
    /// and an answer never waits offline for it (`deal.Verdict`).
    pub(crate) fn push_refused(&self, address: &Address, cx: &gpui::App) -> Option<String> {
        self.away_host(address, cx)
            .map(|host| format!("{} away · approve when it is back", host.name))
    }

    /// The first push that waits in the deal, which `approve push` and
    /// `deny push` open from anywhere else.
    fn first_push(&self, cx: &mut gpui::App) -> Option<rho_dealer::Card> {
        self.hand()
            .cards
            .clone()
            .into_iter()
            .find(|card| self.own_verdicts(&card.address, cx).is_some())
    }

    /// Whether `address` sleeps under a snooze that has not ended; a
    /// note's reminder, due or not.
    fn snoozed(&self, address: &Address, note: bool) -> bool {
        self.marks()
            .get(address)
            .facts()
            .snooze()
            .is_some_and(|snooze| note || snooze.until > jiff::Timestamp::now())
    }

    /// `done` on the card in view: read through, nothing more to say.
    pub(crate) fn done_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.waiting_card(cx) else {
            return self.echo("done: not waiting here", StyleClass::SystemInfo, cx);
        };
        if self.push_here(cx).is_some() {
            return self.echo(&format!("done: {PUSH_WAITS}"), StyleClass::SystemInfo, cx);
        }
        self.verdict(&target, Verdict::Done, "done".to_owned(), window, cx);
    }

    /// `mute` on the thread in view, waiting or not: nothing from it until
    /// it is unmuted.
    pub(crate) fn mute_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.thread_here(cx) else {
            return self.echo("mute: no thread here", StyleClass::SystemInfo, cx);
        };
        if self.push_here(cx).is_some() {
            return self.echo(&format!("mute: {PUSH_WAITS}"), StyleClass::SystemInfo, cx);
        }
        if self.marks().get(&target).facts().muted() {
            return self.echo("mute: muted already", StyleClass::SystemInfo, cx);
        }
        self.verdict(&target, Verdict::Mute, "mute".to_owned(), window, cx);
    }

    /// `approve push` and `deny push` answer only the push on screen. From
    /// anywhere else they open the thread of the first push that waits,
    /// for the user to read before they answer.
    fn answer_push(
        &mut self,
        spelling: &'static Spelling,
        answer: impl FnOnce(&mut Self, Vec<Row>, &mut Window, &mut Context<Self>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(rows) = self.push_here(cx) {
            let refused = self
                .own_card(cx)
                .and_then(|card| self.push_refused(&card, cx));
            return match refused {
                Some(reason) => self.echo(&reason, StyleClass::SystemInfo, cx),
                None => answer(self, rows, window, cx),
            };
        }
        // A push is a card its system answers in its own words.
        match self.first_push(cx) {
            Some(card) => {
                self.open_card(card, window, cx);
                self.echo(
                    &format!(
                        "{}: the push is on screen · read it, then answer",
                        spelling.name
                    ),
                    StyleClass::SystemInfo,
                    cx,
                );
            }
            None => self.echo(
                &format!("{}: no push waits", spelling.name),
                StyleClass::SystemInfo,
                cx,
            ),
        }
    }

    /// `space x`: `deny push` on the line with the draft's words after it,
    /// for `enter` to confirm.
    pub(crate) fn deny_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = self
            .own_card(cx)
            .and_then(|card| self.push_refused(&card, cx))
        {
            return self.echo(&reason, StyleClass::SystemInfo, cx);
        }
        let draft = self.current_surface().draft(cx).unwrap_or_default();
        let draft = draft.split_whitespace().collect::<Vec<_>>().join(" ");
        self.commands_prompt_at(&format!("{} {draft}", DENY_PUSH.name), window, cx);
    }

    /// `deny push`: the system's own denial, which sends the draft. Words
    /// on the line that are not the draft's would be lost, so they are
    /// refused rather than dropped.
    fn deny_push(&mut self, typed: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.answer_push(
            &DENY_PUSH,
            |frame, rows, window, cx| {
                let typed = typed.unwrap_or_default();
                let typed = typed.split_whitespace().collect::<Vec<_>>();
                let draft = frame.current_surface().draft(cx).unwrap_or_default();
                if !typed.is_empty() && typed != draft.split_whitespace().collect::<Vec<_>>() {
                    return frame.echo(
                        "deny push: only the draft's words go with it · write them in the draft",
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
                if let Some(row) = rows.into_iter().find(|row| row.key == "x") {
                    (row.run)(frame, window, cx);
                }
            },
            window,
            cx,
        );
    }

    /// The thread a `snooze thread` lands on: the thread in view, while it
    /// waits or sleeps.
    fn snoozable_thread(&self, cx: &gpui::App) -> Option<Address> {
        self.thread_here(cx)
            .filter(|thread| self.waits(thread) || self.snoozed(thread, false))
    }

    /// `snooze thread` or `snooze note` on `target`, for as long as
    /// `typed` says, or `clear`; with nothing typed, `Commands` asks how
    /// long.
    fn snooze_typed(
        &mut self,
        spelling: &'static Spelling,
        target: Option<Address>,
        typed: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let note = spelling.name == SNOOZE_NOTE.name;
        let Some(target) = target else {
            let what = match note {
                true => "no note in view",
                false => "nothing to snooze here",
            };
            return self.echo(
                &format!("{}: {what}", spelling.name),
                StyleClass::SystemInfo,
                cx,
            );
        };
        let Some(typed) = typed.filter(|typed| !typed.trim().is_empty()) else {
            return self.commands_prompt_at(&format!("{} ", spelling.name), window, cx);
        };
        if typed.trim().eq_ignore_ascii_case(CLEAR_SNOOZE) {
            return self.unsnooze(&target, note, spelling, window, cx);
        }
        match read_snooze(&typed, &jiff::Zoned::now()) {
            Ok(until) => self.snooze_until(&target, until, window, cx),
            Err(reason) => {
                // The line comes back as it was, for the user to fix.
                self.commands_prompt_at(&format!("{} {typed}", spelling.name), window, cx);
                self.echo(
                    &format!("{}: {reason}", spelling.name),
                    StyleClass::SystemInfo,
                    cx,
                );
            }
        }
    }

    /// Takes a snooze back: a thread asks again from now, and a note has
    /// no reminder any more.
    fn unsnooze(
        &mut self,
        target: &Address,
        note: bool,
        spelling: &'static Spelling,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.snoozed(target, note) {
            return self.echo(
                &format!("{}: nothing snoozed here", spelling.name),
                StyleClass::SystemInfo,
                cx,
            );
        }
        match note {
            // Done takes a note's reminder back.
            true => self.verdict(
                target,
                Verdict::Done,
                "reminder cleared".to_owned(),
                window,
                cx,
            ),
            false => self.verdict(
                target,
                Verdict::Snooze(rho_dealer::Until::In(jiff::SignedDuration::ZERO)),
                "unsnoozed".to_owned(),
                window,
                cx,
            ),
        }
    }
}

/// Where a typed snooze lands, for its row in `Commands`.
fn snooze_lands(typed: &str) -> Option<String> {
    let now = jiff::Zoned::now();
    read_snooze(typed, &now)
        .ok()
        .map(|until| snooze_preview(&until, &now))
}

/// The same on a thread already snoozed, where `clear` takes the snooze
/// back.
fn snoozed_thread_lands(typed: &str) -> Option<String> {
    match typed.trim().eq_ignore_ascii_case(CLEAR_SNOOZE) {
        true => Some("→ no snooze".to_owned()),
        false => snooze_lands(typed),
    }
}

/// The same on a note with a reminder, where `clear` takes it back.
fn reminded_note_lands(typed: &str) -> Option<String> {
    match typed.trim().eq_ignore_ascii_case(CLEAR_SNOOZE) {
        true => Some("→ no reminder".to_owned()),
        false => snooze_lands(typed),
    }
}

/// The window's commands, with what each acts on from where the user is.
pub(crate) fn commands(frame: &Frame, cx: &mut gpui::App) -> Vec<Command> {
    let waiting = frame.waiting_card(cx);
    let push = frame.push_here(cx).and_then(|_| frame.own_card(cx));
    // Off the push's thread, a row says it opens the thread rather than
    // answering a push the user has not read.
    let elsewhere = push.is_none() && frame.first_push(cx).is_some();
    let card_words = |address: &Address| frame.card_for(address, cx).heading();
    let note = frame.reminder_in_view(cx);
    let thread = frame.thread_here(cx);
    let snoozable = frame.snoozable_thread(cx);
    let thread_snoozed = thread
        .as_ref()
        .filter(|thread| frame.snoozed(thread, false));
    let note_reminded = note.as_ref().filter(|note| frame.snoozed(note, true));
    let muted = frame
        .card_in_view(cx)
        .filter(|address| frame.marks().get(address).facts().muted());
    let paired = frame.attention.secret().is_some();
    let on_note = note.is_some();
    let note_title = note.as_ref().map(|_| frame.current_surface().title(cx));
    let or = |shown: Option<String>, none: &str| shown.unwrap_or_else(|| none.to_owned());
    let mut snooze_thread = Command::new(&SNOOZE_THREAD, |frame, typed, window, cx| {
        let target = frame.snoozable_thread(cx);
        frame.snooze_typed(&SNOOZE_THREAD, target, typed, window, cx)
    })
    .keys("space s")
    .target(or(snoozable.as_ref().map(card_words), "nothing to snooze"));
    snooze_thread = match thread_snoozed {
        Some(_) => snooze_thread.presets(&SNOOZED_PRESETS, snoozed_thread_lands),
        None => snooze_thread.presets(&SNOOZE_PRESETS, snooze_lands),
    };
    let mut snooze_note = Command::new(&SNOOZE_NOTE, |frame, typed, window, cx| {
        let target = frame.reminder_in_view(cx);
        frame.snooze_typed(&SNOOZE_NOTE, target, typed, window, cx)
    })
    .keys("space s")
    .target(or(note_title.clone(), "no note in view"));
    snooze_note = match note_reminded {
        Some(_) => snooze_note.presets(&SNOOZED_PRESETS, reminded_note_lands),
        None => snooze_note.presets(&SNOOZE_PRESETS, snooze_lands),
    };
    let mut unsnooze_thread = Command::simple(&UNSNOOZE_THREAD, |frame, window, cx| {
        match frame.thread_here(cx) {
            Some(target) => frame.unsnooze(&target, false, &UNSNOOZE_THREAD, window, cx),
            None => frame.echo(
                "unsnooze thread: no thread here",
                StyleClass::SystemInfo,
                cx,
            ),
        }
    })
    .target(or(thread_snoozed.map(card_words), "nothing snoozed here"));
    let mut unsnooze_note = Command::simple(&UNSNOOZE_NOTE, |frame, window, cx| {
        match frame.reminder_in_view(cx) {
            Some(target) => frame.unsnooze(&target, true, &UNSNOOZE_NOTE, window, cx),
            None => frame.echo("unsnooze note: no note in view", StyleClass::SystemInfo, cx),
        }
    })
    .target(or(note_reminded.and(note_title), "no reminder here"));
    match on_note {
        true => {
            snooze_note = snooze_note.here();
            unsnooze_note = unsnooze_note.here();
        }
        false => {
            snooze_thread = snooze_thread.here();
            unsnooze_thread = unsnooze_thread.here();
        }
    }
    let push_target = |verb: &str| match (&push, elsewhere) {
        (Some(push), _) => card_words(push),
        (None, true) => format!("open thread to {verb}"),
        (None, false) => "no push waits".to_owned(),
    };
    // On the push's own thread, `approve` is its approval before any
    // other system's.
    let push_here = |command: Command| match push {
        Some(_) => command.here(),
        None => command,
    };
    vec![
        Command::simple(&DEAL_NEXT, |frame, window, cx| frame.pull_card(window, cx))
            .keys("space-j"),
        Command::simple(&CLOSE_AND_DEAL, |frame, window, cx| {
            frame.close_current(window, cx);
            frame.pull_card(window, cx);
        })
        .keys("space-shift-j"),
        Command::simple(&DONE, |frame, window, cx| frame.done_here(window, cx))
            .keys("space d")
            .target(or(waiting.as_ref().map(card_words), "not waiting here")),
        Command::simple(&MUTE, |frame, window, cx| frame.mute_here(window, cx))
            .keys("space m")
            .target(or(thread.as_ref().map(card_words), "no thread here")),
        snooze_thread,
        snooze_note,
        unsnooze_thread,
        unsnooze_note,
        push_here(
            Command::simple(&APPROVE_PUSH, |frame, window, cx| {
                frame.answer_push(
                    &APPROVE_PUSH,
                    |frame, rows, window, cx| {
                        if let Some(row) = rows.into_iter().find(|row| row.key == "a") {
                            (row.run)(frame, window, cx);
                        }
                    },
                    window,
                    cx,
                )
            })
            .keys("space a")
            .target(push_target("approve")),
        ),
        push_here(
            Command::new(&DENY_PUSH, |frame, typed, window, cx| {
                frame.deny_push(typed, window, cx)
            })
            .keys("space x")
            .target(push_target("deny")),
        ),
        Command::simple(&UNDO, |frame, window, cx| frame.undo_verdict(window, cx)).keys("space u"),
        Command::simple(&OPEN_PLACES, |frame, window, cx| frame.places(window, cx)).keys("space f"),
        Command::simple(&OPEN_DESK, |frame, window, cx| frame.open_desk(window, cx)).keys("tab"),
        Command::simple(&CLOSE_SCREEN, |frame, window, cx| {
            frame.close_current(window, cx)
        })
        .keys("q"),
        Command::simple(&WRITE_IDEA, |_, window, cx| {
            window.dispatch_action(Box::new(keys::PromptIdea), cx)
        })
        .bound_to(keys::PromptIdea),
        Command::simple(&UNMUTE, |frame, window, cx| {
            match frame
                .card_in_view(cx)
                .filter(|address| frame.marks().get(address).facts().muted())
            {
                Some(target) => {
                    frame.verdict(&target, Verdict::Unmute, "unmuted".to_owned(), window, cx)
                }
                None => frame.echo(
                    "unmute: nothing muted here · open the muted thread first",
                    StyleClass::SystemInfo,
                    cx,
                ),
            }
        })
        .target(or(muted.as_ref().map(card_words), "nothing muted here")),
        Command::simple(&TOGGLE_NOTCH_MASK, |frame, _, cx| {
            frame.toggle_notch_mask(cx)
        })
        .target(if frame.notch_mask { "on" } else { "off" }),
        Command::simple(&SHOW_MESSAGES, |frame, window, cx| {
            frame.show_messages(window, cx)
        })
        .target(match frame.echoes().len() {
            1 => "1 message".to_owned(),
            count => format!("{count} messages"),
        }),
        Command::simple(&PAIR_DEVICE, |frame, window, cx| {
            match frame.attention.secret() {
                Some(_) => frame.echo(
                    "pair device: this device is paired · show secret phrase shows its words",
                    StyleClass::SystemInfo,
                    cx,
                ),
                None => frame.prompt_pair(String::new(), None, window, cx),
            }
        })
        .target(if paired { "paired" } else { "not paired" }),
        Command::simple(&SHOW_SECRET_PHRASE, |frame, window, cx| {
            match frame.attention.secret() {
                Some(secret) => {
                    frame.show_secret(secret, "your secret phrase · enter:", window, cx)
                }
                None => frame.echo(
                    "show secret phrase: this device is not paired",
                    StyleClass::SystemInfo,
                    cx,
                ),
            }
        }),
        Command::simple(&QUIT, |_, _, cx| cx.quit()),
    ]
}
