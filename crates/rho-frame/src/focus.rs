//! Dealer focus (`deal.Card.set dealer focus`): what the deal, the lamp
//! and the chime answer to for a while. Desk is one global glance and
//! never narrows; only what deals and what signals does.
//!
//! The focus is this device's, kept with where the user was
//! (`place`), since it says how the user is working here and now and not
//! anything about a card.

use std::rc::Rc;

use jiff::{SignedDuration, Timestamp};
use rho_dealer::Address;
use rho_window::style::StyleClass;
use senax_encoder::{Decode, Encode};

use crate::command::{Command, Spelling};
use crate::frame::Frame;
use crate::minibuffer::Candidate;
use crate::system::Project;

pub static SET_DEALER_FOCUS: Spelling = Spelling {
    name: "set dealer focus",
    aliases: &[
        ("focus", 9),
        ("quiet", 9),
        ("dnd", 9),
        ("do not disturb", 8),
        ("focus on", 8),
        ("mute everything", 4),
    ],
    argument: Some("a project, quiet 45m, or off"),
};

pub static CLEAR_DEALER_FOCUS: Spelling = Spelling {
    name: "clear dealer focus",
    aliases: &[
        ("focus off", 9),
        ("unfocus", 9),
        ("focus all", 7),
        ("unquiet", 7),
    ],
    argument: None,
};

/// What the deal, the lamp and the chime answer to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum DealerFocus {
    /// Every card deals and signals.
    #[default]
    All,
    /// Only the cards of the project at this address deal and signal.
    Project(Address),
    /// Every card deals when the user asks, and none signals, until then.
    QuietUntil(Timestamp),
}

impl DealerFocus {
    /// The focus that holds at `now`: a quiet that has run out is all.
    pub(crate) fn at(&self, now: Timestamp) -> &Self {
        match self {
            Self::QuietUntil(until) if *until <= now => &Self::All,
            focus => focus,
        }
    }

    /// Whether the card at `card`, in `project`, deals on `space-j` and
    /// may light the lamp or ring the chime. A project focus lets the
    /// user's own due notes through, which belong to no project; a quiet
    /// holds everything until it ends.
    pub(crate) fn admits(&self, card: &Address, project: Option<&Address>, now: Timestamp) -> bool {
        match self.at(now) {
            Self::All => true,
            Self::QuietUntil(_) => false,
            Self::Project(focus) => card.as_note().is_some() || project == Some(focus),
        }
    }

    /// What `space-j` says in a quiet in place of a deal: `quiet 32m`.
    pub(crate) fn quiet_left(&self, now: Timestamp) -> Option<String> {
        match self.at(now) {
            Self::QuietUntil(until) => Some(format!("quiet {}", left(now, *until))),
            Self::All | Self::Project(_) => None,
        }
    }

    /// The quiet note beside the Desk count and on the status line:
    /// `focus tide` or `quiet 45m`; nothing while every card counts.
    /// `name` is what the user calls the project at an address now.
    pub(crate) fn note(&self, name: impl Fn(&Address) -> String, now: Timestamp) -> Option<String> {
        match self.at(now) {
            Self::All => None,
            Self::Project(project) => Some(format!("focus {}", name(project))),
            Self::QuietUntil(_) => self.quiet_left(now),
        }
    }

    /// When the focus next changes by itself, or its note says another
    /// time: a quiet's minutes count down, and it ends.
    pub(crate) fn next_change(&self, now: Timestamp) -> Option<Timestamp> {
        match self.at(now) {
            Self::QuietUntil(until) => Some((*until).min(now + SignedDuration::from_mins(1))),
            Self::All | Self::Project(_) => None,
        }
    }
}

/// How long is left until `until`, in the words every age is said in,
/// rounded up so the last minute still says `1m`.
fn left(now: Timestamp, until: Timestamp) -> String {
    let remaining = now.duration_until(until).max(SignedDuration::ZERO);
    let whole_minutes = (remaining.as_secs() + 59) / 60;
    let shown = SignedDuration::from_mins(whole_minutes.max(1));
    rho_dealer::curve::age(now, now + shown)
}

/// A project the user can focus on: its address, and every name that
/// finds it (`tide`, `tide@lab`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Focusable {
    pub project: Project,
    pub also: Vec<String>,
}

const HOW_LONG: &str = "say how long, such as 45m, 2h or 1d";

/// What the words typed after `set dealer focus` ask for, at `now`:
/// `off` or `all`; `quiet 45m`, or the time alone; a project by name,
/// with `project` before it or not. `projects` are the projects there are.
pub(crate) fn parse(
    text: &str,
    projects: &[Focusable],
    now: Timestamp,
) -> Result<DealerFocus, String> {
    let text = text.trim().to_lowercase();
    if matches!(text.as_str(), "off" | "all" | "none" | "everything") {
        return Ok(DealerFocus::All);
    }
    let (quiet, rest) = match text.strip_prefix("quiet") {
        Some(rest) if rest.is_empty() || rest.starts_with(' ') => (true, rest.trim()),
        _ => (false, text.as_str()),
    };
    if quiet || rest.starts_with(|c: char| c.is_ascii_digit()) {
        let quiet_for = duration(rest).ok_or(HOW_LONG)?;
        return Ok(DealerFocus::QuietUntil(now + quiet_for));
    }
    let name = match rest.strip_prefix("project") {
        Some(name) if name.is_empty() || name.starts_with(' ') => name.trim(),
        _ => rest,
    };
    project_named(name, projects).map(|project| DealerFocus::Project(project.address.clone()))
}

/// `45m`, `2h`, `1 day`: a count and a unit of minutes, hours or days.
fn duration(text: &str) -> Option<SignedDuration> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let count: i64 = text[..digits].parse().ok().filter(|count| *count > 0)?;
    let unit = match text[digits..].trim() {
        "m" | "min" | "mins" | "minute" | "minutes" => SignedDuration::from_mins(1),
        "h" | "hr" | "hrs" | "hour" | "hours" => SignedDuration::from_hours(1),
        "d" | "day" | "days" => SignedDuration::from_hours(24),
        _ => return None,
    };
    unit.checked_mul(i32::try_from(count).ok()?)
}

/// The one project `name` means: the one called that, or else the one
/// whose name starts with it.
fn project_named<'a>(name: &str, projects: &'a [Focusable]) -> Result<&'a Project, String> {
    if name.is_empty() {
        return Err("name a project, quiet 45m, or off".to_owned());
    }
    let names = |focusable: &'a Focusable| {
        std::iter::once(focusable.project.name.to_lowercase())
            .chain(focusable.also.iter().map(|also| also.to_lowercase()))
    };
    let pick = |fits: &dyn Fn(&str) -> bool| -> Vec<&'a Project> {
        projects
            .iter()
            .filter(|focusable| names(focusable).any(|known| fits(&known)))
            .map(|focusable| &focusable.project)
            .collect()
    };
    let exact = pick(&|known| known == name);
    let fits = match exact.as_slice() {
        [] => pick(&|known| known.starts_with(name)),
        _ => exact,
    };
    match fits.as_slice() {
        [project] => Ok(project),
        [] => Err(format!("no project called {name}")),
        many => Err(format!(
            "{name} is more than one project: {}",
            many.iter()
                .map(|project| project.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// `set dealer focus` and `clear dealer focus`, with what each acts on
/// from where the user is.
pub(crate) fn commands(frame: &Frame, cx: &gpui::App) -> Vec<Command> {
    let now = Timestamp::now();
    let current = frame
        .focus_note(now, cx)
        .unwrap_or_else(|| "all".to_owned());
    // What it changes, as it stands: the words typed, or the project in
    // view when none are, say what it becomes.
    let set_target = format!("now {current}");
    vec![
        Command::new(
            &SET_DEALER_FOCUS,
            move |frame, argument, window, cx| match argument {
                Some(text) => {
                    let parsed = parse(&text, &frame.focusable(cx), Timestamp::now());
                    match parsed {
                        Ok(focus) => frame.set_dealer_focus(focus, cx),
                        Err(reason) => frame.echo(
                            &format!("{}: {reason}", SET_DEALER_FOCUS.name),
                            StyleClass::SystemInfo,
                            cx,
                        ),
                    }
                }
                None => match frame.context_project(cx) {
                    Some(project) => {
                        frame.set_dealer_focus(DealerFocus::Project(project.address), cx)
                    }
                    None => frame.prompt_dealer_focus(window, cx),
                },
            },
        )
        .target(set_target),
        Command::simple(&CLEAR_DEALER_FOCUS, |frame, _, cx| {
            frame.set_dealer_focus(DealerFocus::All, cx)
        })
        .target(match current.as_str() {
            "all" => "already all".to_owned(),
            _ => current,
        }),
    ]
}

impl Frame {
    /// The focus the deal, the lamp and the chime answer to now.
    pub fn dealer_focus(&self) -> &DealerFocus {
        self.dealer_focus.at(Timestamp::now())
    }

    /// Sets the dealer focus, keeps it on this device, and says so.
    pub(crate) fn set_dealer_focus(&mut self, focus: DealerFocus, cx: &mut gpui::Context<Self>) {
        let now = Timestamp::now();
        let said = match &focus {
            DealerFocus::All => "dealer focus: all".to_owned(),
            DealerFocus::Project(project) => {
                format!("dealer focus: {}", self.project_name(project, cx))
            }
            DealerFocus::QuietUntil(until) => {
                let zone = jiff::tz::TimeZone::system();
                let at = until.to_zoned(zone.clone());
                format!(
                    "quiet for {} · {}",
                    left(now, *until),
                    crate::when::until_said(&at, &now.to_zoned(zone))
                )
            }
        };
        self.place.write(crate::place::Write::Focus(focus.clone()));
        self.dealer_focus = focus;
        self.echo(&said, StyleClass::SystemInfo, cx);
        self.systems_changed(cx);
    }

    /// `focus tide` or `quiet 45m`, while a focus holds.
    pub(crate) fn focus_note(&self, now: Timestamp, cx: &gpui::App) -> Option<String> {
        self.dealer_focus
            .note(|project| self.project_name(project, cx), now)
    }

    /// What the user calls the project at `address` now, or its address
    /// when no system knows it any more.
    pub(crate) fn project_name(&self, address: &Address, cx: &gpui::App) -> String {
        self.project_of(address, cx)
            .map_or_else(|| address.key().to_owned(), |project| project.name)
    }

    /// The project the point is in, or the screen in view is.
    pub(crate) fn context_project(&self, cx: &gpui::App) -> Option<Project> {
        let point = self.point(cx);
        point
            .subject()
            .and_then(|subject| self.project_of(subject, cx))
            .or_else(|| self.project_of(&point.surface, cx))
    }

    /// Every project there is, by every name that finds it.
    pub(crate) fn focusable(&self, cx: &gpui::App) -> Vec<Focusable> {
        let mut projects: Vec<Focusable> = Vec::new();
        for system in &self.systems {
            for found in system.find(&self.attention.marks, cx) {
                let Some(project) = self
                    .project_of(&found.address, cx)
                    .filter(|project| project.address == found.address)
                else {
                    continue;
                };
                if projects.iter().all(|known| known.project != project) {
                    projects.push(Focusable {
                        project,
                        also: found.also,
                    });
                }
            }
        }
        projects
    }

    /// `set dealer focus:` with nothing typed and no project in view: every
    /// project, then quiet and all.
    fn prompt_dealer_focus(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        self.open_prompt(
            "set dealer focus (a project, quiet 45m, or off):",
            Rc::new(|frame, needle, cx| {
                let needle = needle.trim().to_lowercase();
                frame
                    .focusable(cx)
                    .into_iter()
                    .map(|focusable| Candidate {
                        value: focusable.project.name,
                        description: "project".to_owned(),
                    })
                    .chain([
                        Candidate {
                            value: "quiet 1h".to_owned(),
                            description: "no lamp or chime".to_owned(),
                        },
                        Candidate {
                            value: "off".to_owned(),
                            description: "every card".to_owned(),
                        },
                    ])
                    .filter(|candidate| candidate.value.to_lowercase().contains(&needle))
                    .collect()
            }),
            Rc::new(|frame, input, _, cx| {
                match parse(&input, &frame.focusable(cx), Timestamp::now()) {
                    Ok(focus) => frame.set_dealer_focus(focus, cx),
                    Err(reason) => frame.echo(
                        &format!("{}: {reason}", SET_DEALER_FOCUS.name),
                        StyleClass::SystemInfo,
                        cx,
                    ),
                }
            }),
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noon() -> Timestamp {
        "2026-11-26T12:00:00Z".parse().unwrap()
    }

    fn project(key: &str, name: &str, also: &[&str]) -> Focusable {
        Focusable {
            project: Project {
                address: Address::new("thread", key),
                name: name.to_owned(),
            },
            also: also.iter().map(|word| (*word).to_owned()).collect(),
        }
    }

    fn projects() -> Vec<Focusable> {
        vec![
            project("lab/1", "tide", &["tide@lab"]),
            project("box/1", "tidepool", &[]),
            project("lab/9", "reef", &[]),
        ]
    }

    #[test]
    fn typed_words_name_a_focus() {
        let at = |key: &str| Ok(DealerFocus::Project(Address::new("thread", key)));
        let quiet = |mins| {
            Ok(DealerFocus::QuietUntil(
                noon() + SignedDuration::from_mins(mins),
            ))
        };
        for (typed, focus) in [
            ("tide", at("lab/1")),
            ("Tide", at("lab/1")),
            ("project tide", at("lab/1")),
            ("tide@lab", at("lab/1")),
            ("tidep", at("box/1")),
            ("re", at("lab/9")),
            ("45m", quiet(45)),
            ("quiet 45m", quiet(45)),
            ("quiet 2h", quiet(120)),
            ("1 day", quiet(24 * 60)),
            ("off", Ok(DealerFocus::All)),
            (" ALL ", Ok(DealerFocus::All)),
        ] {
            assert_eq!(parse(typed, &projects(), noon()), focus, "{typed:?}");
        }
    }

    #[test]
    fn words_that_name_no_one_focus_say_why() {
        for (typed, why) in [
            ("quiet", HOW_LONG.to_owned()),
            ("quiet soon", HOW_LONG.to_owned()),
            ("0m", HOW_LONG.to_owned()),
            ("45x", HOW_LONG.to_owned()),
            ("kelp", "no project called kelp".to_owned()),
            (
                "ti",
                "ti is more than one project: tide, tidepool".to_owned(),
            ),
            ("project ", "name a project, quiet 45m, or off".to_owned()),
            ("", "name a project, quiet 45m, or off".to_owned()),
        ] {
            assert_eq!(parse(typed, &projects(), noon()), Err(why), "{typed:?}");
        }
        // `quieter` is a project's name, not a quiet.
        let quieter = vec![project("lab/3", "quieter", &[])];
        assert_eq!(
            parse("quieter", &quieter, noon()),
            Ok(DealerFocus::Project(Address::new("thread", "lab/3")))
        );
    }

    /// A project focus admits only its own cards and the user's due
    /// notes, which are in no project; a quiet admits nothing, notes
    /// included, until it ends, then is all again.
    #[test]
    fn focus_decides_what_deals_and_signals() {
        let tide = Address::new("thread", "lab/1");
        let reef = Address::new("thread", "lab/9");
        let clock = Address::new("thread", "lab/2");
        let retry = Address::new("thread", "lab/10");
        let note = Address::note(uuid::Uuid::new_v4());
        let focus = DealerFocus::Project(tide.clone());
        for (card, project, admits) in [
            (&clock, Some(&tide), true),
            (&retry, Some(&reef), false),
            (&retry, None, false),
            (&note, None, true),
        ] {
            assert_eq!(focus.admits(card, project, noon()), admits, "{card:?}");
        }
        let until = noon() + SignedDuration::from_mins(45);
        let quiet = DealerFocus::QuietUntil(until);
        for (card, project) in [(&clock, Some(&tide)), (&note, None)] {
            assert!(!quiet.admits(card, project, noon()), "{card:?}");
            assert!(
                quiet.admits(card, project, until),
                "a quiet that ended is all"
            );
            assert!(DealerFocus::All.admits(card, project, noon()));
        }
        assert_eq!(quiet.at(until), &DealerFocus::All);
        assert_eq!(quiet.quiet_left(noon()).as_deref(), Some("quiet 45m"));
        assert_eq!(quiet.quiet_left(until), None);
        assert_eq!(focus.quiet_left(noon()), None);
    }

    #[test]
    fn the_note_says_the_focus_calmly() {
        let name = |_: &Address| "tide".to_owned();
        assert_eq!(DealerFocus::All.note(name, noon()), None);
        assert_eq!(
            DealerFocus::Project(Address::new("thread", "lab/1")).note(name, noon()),
            Some("focus tide".to_owned())
        );
        let quiet = |secs| DealerFocus::QuietUntil(noon() + SignedDuration::from_secs(secs));
        assert_eq!(
            quiet(45 * 60).note(name, noon()),
            Some("quiet 45m".to_owned())
        );
        assert_eq!(
            quiet(44 * 60 + 1).note(name, noon()),
            Some("quiet 45m".to_owned())
        );
        assert_eq!(quiet(20).note(name, noon()), Some("quiet 1m".to_owned()));
        assert_eq!(
            quiet(90 * 60).note(name, noon()),
            Some("quiet 1.5h".to_owned())
        );
        assert_eq!(quiet(0).note(name, noon()), None);
        assert_eq!(
            quiet(45 * 60).next_change(noon()),
            Some(noon() + SignedDuration::from_mins(1))
        );
        assert_eq!(
            quiet(20).next_change(noon()),
            Some(noon() + SignedDuration::from_secs(20))
        );
    }
}
