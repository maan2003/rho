//! The window's own commands (`window.Commands`): those about the window,
//! the deal and this device rather than about any system's things.

use rho_window::style::StyleClass;

use crate::attention::Verdict;
use crate::command::{Command, Spelling};
use crate::frame::{Frame, snooze_rows};
use crate::keys;
use crate::when::parse_snooze;

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

pub static UNDO_VERDICT: Spelling = Spelling {
    name: "undo verdict",
    aliases: &[("undo", 9), ("take back", 7), ("undone", 5), ("oops", 4)],
    argument: None,
};

pub static SNOOZE: Spelling = Spelling {
    name: "snooze",
    aliases: &[
        ("later", 7),
        ("defer", 7),
        ("postpone", 7),
        ("remind me", 6),
        ("sleep", 5),
    ],
    argument: Some("how long: 3h, 2d, 1w, tomorrow or next week"),
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

/// The window's commands, with what each acts on from where the user is.
pub(crate) fn commands(frame: &Frame, cx: &gpui::App) -> Vec<Command> {
    let waiting = frame.waiting_card(cx);
    let card_words = |address: &rho_dealer::Address| frame.card_for(address, cx).heading();
    let muted = frame
        .card_in_view(cx)
        .filter(|address| frame.marks().get(address).facts().muted());
    let paired = frame.attention.secret().is_some();
    vec![
        Command::simple(&WRITE_IDEA, |_, window, cx| {
            window.dispatch_action(Box::new(keys::PromptIdea), cx)
        })
        .bound_to(keys::PromptIdea),
        Command::simple(&UNDO_VERDICT, |frame, window, cx| {
            frame.undo_verdict(window, cx)
        })
        .keys("space u"),
        Command::new(&SNOOZE, |frame, argument, window, cx| {
            let Some(target) = frame.waiting_card(cx) else {
                return frame.echo("snooze: no waiting card here", StyleClass::SystemInfo, cx);
            };
            match argument.as_deref().map(parse_snooze) {
                None => frame.open_menu(snooze_rows(&target), window, cx),
                Some(Ok((unit, count))) => frame.snooze(&target, unit, Some(count), window, cx),
                Some(Err(reason)) => {
                    frame.echo(&format!("snooze: {reason}"), StyleClass::SystemInfo, cx)
                }
            }
        })
        .keys("space s")
        .target(
            waiting
                .as_ref()
                .map_or_else(|| "no waiting card".to_owned(), card_words),
        ),
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
        .target(
            muted
                .as_ref()
                .map_or_else(|| "nothing muted here".to_owned(), card_words),
        ),
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
