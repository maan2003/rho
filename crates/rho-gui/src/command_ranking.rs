//! Every system's command spellings, ranked together as `Commands` ranks
//! them (`window.Commands`): an alias means what the product says it means
//! even with every other system's words beside it.

use rho_frame::{Spelling, rank_spellings};

fn every_spelling() -> Vec<&'static Spelling> {
    [
        rho_frame::SPELLINGS,
        rho_chat::ui::SPELLINGS,
        rho_browser::ui::SPELLINGS,
        rho_notes::SPELLINGS,
        rho_desktop_client::ui::SPELLINGS,
        rho_agents_view::SPELLINGS,
        &[&crate::telemetry::UPLOAD_PERFORMANCE_REPORT],
    ]
    .concat()
}

fn names(input: &str) -> Vec<&'static str> {
    rank_spellings(&every_spelling(), input)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

fn first(input: &str) -> (&'static str, Option<String>) {
    rank_spellings(&every_spelling(), input)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("nothing ranks for {input:?}"))
}

/// One name, one command: a second command of the same name would hide
/// behind the first. Names are verb and noun, never a breadcrumb or an
/// ellipsis that hides a prompt.
#[test]
fn every_name_is_its_own_and_plain() {
    let spellings = every_spelling();
    let mut seen = std::collections::HashSet::new();
    for spelling in &spellings {
        assert!(
            seen.insert(spelling.name),
            "{} is spelled twice",
            spelling.name
        );
        assert!(
            !spelling.name.contains('›') && !spelling.name.contains('…'),
            "{} is not a plain name",
            spelling.name
        );
        assert_eq!(
            spelling.name,
            spelling.name.to_lowercase(),
            "{}",
            spelling.name
        );
    }
    for named in [
        "compact agent",
        "reset agent cache",
        "rewind agent",
        "set agent runtime",
        "cancel agent request",
        "stop agent",
        "rewind agent to here",
        "finish work",
        "rename thread",
        "fork thread",
        "rename project",
        "retire project",
        "set project place",
        "set project mode",
        "set project runtime",
        "new project",
        "attach host",
        "detach host",
        "list hosts",
        "open terminal",
        "new terminal",
        "open shell",
        "close shell",
        "open desktop",
        "draw on desktop",
        "back to live",
        "open transcript",
        "filter transcript",
        "open thread",
        "open page",
        "bookmark page",
        "remove bookmark",
        "switch claude account",
        "open usage",
        "new note",
        "list notes",
        "reply privately",
        "write idea",
        "write reply again",
        "pair device",
        "show secret phrase",
        "list devices",
        "revoke device",
        "upload performance report",
        "quit",
        "snooze",
        "unmute",
        "set dealer focus",
        "clear dealer focus",
    ] {
        assert!(seen.contains(named), "no command is called {named}");
    }
}

/// `kill` stops the agent first and cancels its request second, and
/// nothing else is killed.
#[test]
fn kill_means_stop_agent_then_cancel_agent_request() {
    assert_eq!(names("kill"), ["stop agent", "cancel agent request"]);
    for word in ["abort", "interrupt"] {
        assert_eq!(first(word).0, "cancel agent request", "{word}");
    }
}

/// `model` is the agent's runtime before the project's.
#[test]
fn model_means_the_agents_runtime_then_the_projects() {
    assert_eq!(
        names("model")[..2],
        ["set agent runtime", "set project runtime"]
    );
    assert_eq!(
        names("runtime")[..2],
        ["set agent runtime", "set project runtime"]
    );
}

/// The argument is what follows the words that name the command.
#[test]
fn inline_arguments_reach_their_command() {
    for (line, command, argument) in [
        ("model sol-medium", "set agent runtime", "sol-medium"),
        (
            "set agent runtime opus-high",
            "set agent runtime",
            "opus-high",
        ),
        (
            "set project runtime sol-high",
            "set project runtime",
            "sol-high",
        ),
        (
            "rename Fix the flaky clock",
            "rename thread",
            "Fix the flaky clock",
        ),
        ("snooze 3h", "snooze", "3h"),
        ("later tomorrow", "snooze", "tomorrow"),
        (
            "open https://example.com/a b",
            "open page",
            "https://example.com/a b",
        ),
        ("bookmark Slack", "bookmark page", "Slack"),
        ("rewind 2", "rewind agent", "2"),
        ("rewind agent 3", "rewind agent", "3"),
        ("attach lab=local", "attach host", "lab=local"),
        ("detach lab", "detach host", "lab"),
        ("filter whole workset", "filter transcript", "whole workset"),
        ("finish work clock", "finish work", "clock"),
        ("rename project apart", "rename project", "apart"),
        ("retire project tide", "retire project", "tide"),
        (
            "switch claude account work",
            "switch claude account",
            "work",
        ),
        ("focus tide", "set dealer focus", "tide"),
        ("quiet 45m", "set dealer focus", "45m"),
        ("dnd 2h", "set dealer focus", "2h"),
        (
            "set dealer focus project reef",
            "set dealer focus",
            "project reef",
        ),
    ] {
        assert_eq!(first(line), (command, Some(argument.to_owned())), "{line}");
    }
    // A command that takes nothing is never handed the rest of the line.
    assert!(
        rank_spellings(&every_spelling(), "quit now")
            .iter()
            .all(|(name, argument)| *name != "quit" || argument.is_none())
    );
}

/// Short and everyday words land on the command people mean by them, even
/// where another system's words hold the same word.
#[test]
fn everyday_words_find_what_they_mean() {
    for (word, command) in [
        ("term", "open terminal"),
        ("terminal", "open terminal"),
        ("shell", "open shell"),
        ("usage", "open usage"),
        ("quota", "open usage"),
        ("undo", "undo verdict"),
        ("exit", "quit"),
        ("hosts", "list hosts"),
        ("devices", "list devices"),
        ("lost laptop", "revoke device"),
        ("notes", "list notes"),
        ("url", "open page"),
        ("go", "open page"),
        ("clear cache", "reset agent cache"),
        ("compact", "compact agent"),
        ("transcript", "open transcript"),
        ("desktop", "open desktop"),
        ("annotate", "draw on desktop"),
        ("title", "rename thread"),
        ("idea", "write idea"),
        ("slow", "upload performance report"),
        ("account", "switch claude account"),
        ("tangent", "fork thread"),
        ("finish", "finish work"),
        ("rewind agent to here", "rewind agent to here"),
        ("rewind to here", "rewind agent to here"),
        ("rewind here", "rewind agent to here"),
    ] {
        assert_eq!(first(word).0, command, "{word}");
    }
    assert_eq!(
        names("rewind agent to here")[..2],
        ["rewind agent to here", "rewind agent"]
    );
    // `undo turn` is the agent's rewind only when said outright.
    assert_eq!(first("undo turn").0, "rewind agent");
    // `rewind` alone takes back the last turn, not to a step.
    assert_eq!(first("rewind").0, "rewind agent");
}

/// `focus` sets the dealer's focus and `focus off` clears it, though one
/// begins the other.
#[test]
fn focus_sets_the_dealer_focus_and_focus_off_clears_it() {
    for word in ["focus", "quiet", "dnd", "do not disturb"] {
        assert_eq!(first(word), ("set dealer focus", None), "{word}");
    }
    for words in ["focus off", "unfocus", "clear dealer focus"] {
        assert_eq!(first(words), ("clear dealer focus", None), "{words}");
    }
}
