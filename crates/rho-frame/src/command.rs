//! A command as `Commands` lists it (`window.Commands`): a flat verb-noun
//! name with an explicit object noun, the other words that mean it, what
//! it acts on right now, and what it does, given the argument typed after
//! its name when it takes one.
//!
//! A system says what its commands are called once, as [`Spelling`]
//! constants, so the words that find a command are data a test can rank
//! without the system behind them.

use std::rc::Rc;

use gpui::{Context, Window};

use crate::frame::Frame;

/// What a command does, given the text typed after its name, when it takes
/// one and the user typed it. Without it, a command that needs a value or a
/// target asks with a `Prompt`.
pub type Run = Rc<dyn Fn(&mut Frame, Option<String>, &mut Window, &mut Context<Frame>)>;

/// What a value typed after a command's name comes to, as its row
/// previews it: `→ today 19:56` for `snooze thread 45m`. `None` when the
/// words are no such value.
pub type Preview = Rc<dyn Fn(&str) -> Option<String>>;

/// How strongly the name itself means its command; an alias weighs less.
pub(crate) const NAME_WEIGHT: u8 = 10;

/// What a command is called.
#[derive(Debug)]
pub struct Spelling {
    /// Verb, then the object noun: `set agent runtime`, never
    /// `agent › runtime` or `set runtime`.
    pub name: &'static str,
    /// Other words that mean the command, each with how surely it means
    /// this one, from 1 (a loose association) to 9 (what the user almost
    /// always means by it). `kill` is 9 for `stop agent` and 7 for
    /// `cancel agent request`.
    pub aliases: &'static [(&'static str, u8)],
    /// What may follow the name on the line, run in one shot: `runtime`
    /// for `set agent runtime sol-medium`. `None` when nothing may.
    pub argument: Option<&'static str>,
}

/// One row of `Commands`.
#[derive(Clone)]
pub struct Command {
    pub(crate) spelling: &'static Spelling,
    /// What the command acts on and its current value, from the screen in
    /// view: `tide · Flaky clock test · working (opus-high)`, or
    /// `pick agent…` when the screen does not say.
    pub(crate) target: Option<Target>,
    pub(crate) shortcut: Option<Shortcut>,
    /// Values offered as rows of their own before one is typed, such as
    /// the snooze times, with what each value comes to.
    pub(crate) presets: &'static [&'static str],
    pub(crate) preview: Option<Preview>,
    /// It acts on the screen in view: among commands the words mean as
    /// surely, it comes first (`snooze` on a note is `snooze note`).
    pub(crate) here: bool,
    pub(crate) run: Run,
}

/// What a row says the command acts on.
#[derive(Clone)]
pub(crate) enum Target {
    /// Its current value, or what it acts on, which words typed after its
    /// name never change: `lab · online`.
    Shown(String),
    /// What it acts on from the place in view. Words typed after its name
    /// name another place to act from instead: `stop agent clock` stops
    /// the agent of the thread `clock` finds, wherever the user is.
    Here(String),
}

/// The keys that run a command without `Commands`.
#[derive(Clone)]
pub(crate) enum Shortcut {
    /// A binding, resolved in the key context of the screen in view.
    Action(Rc<dyn gpui::Action>),
    /// A literal sequence: `space s`, `tab`.
    Keys(&'static str),
}

impl Command {
    /// A command spelled `spelling`. `run` gets the argument typed after
    /// the name, which only a spelling with an [`Spelling::argument`] ever
    /// receives.
    pub fn new(
        spelling: &'static Spelling,
        run: impl Fn(&mut Frame, Option<String>, &mut Window, &mut Context<Frame>) + 'static,
    ) -> Self {
        debug_assert!(
            spelling
                .aliases
                .iter()
                .all(|(_, weight)| (1..NAME_WEIGHT).contains(weight)),
            "{} weighs an alias outside 1..=9",
            spelling.name
        );
        Self {
            spelling,
            target: None,
            shortcut: None,
            presets: &[],
            preview: None,
            here: false,
            run: Rc::new(run),
        }
    }

    /// A command that takes nothing after its name.
    pub fn simple(
        spelling: &'static Spelling,
        run: impl Fn(&mut Frame, &mut Window, &mut Context<Frame>) + 'static,
    ) -> Self {
        debug_assert!(
            spelling.argument.is_none(),
            "{} takes an argument",
            spelling.name
        );
        Self::new(spelling, move |frame, _, window, cx| run(frame, window, cx))
    }

    /// What it acts on and its current value, as the row shows it.
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(Target::Shown(target.into()));
        self
    }

    /// What it acts on from the place in view, as the row shows it, for a
    /// command that takes nothing after its name: words typed there name
    /// the place to act from instead (`window.Commands`).
    pub fn acts_on(mut self, target: impl Into<String>) -> Self {
        debug_assert!(
            self.spelling.argument.is_none(),
            "{} takes an argument, so the words after it are not a place",
            self.spelling.name
        );
        self.target = Some(Target::Here(target.into()));
        self
    }

    /// The row shows the keys bound to `action` where the user is.
    pub fn bound_to(mut self, action: impl gpui::Action) -> Self {
        self.shortcut = Some(Shortcut::Action(Rc::new(action)));
        self
    }

    /// The row shows `keys`, a sequence such as `space s`.
    pub fn keys(mut self, keys: &'static str) -> Self {
        self.shortcut = Some(Shortcut::Keys(keys));
        self
    }

    /// Before a value is typed after its name, the line offers `presets`
    /// as rows of their own; each row, and a value typed, shows what
    /// `preview` makes of it.
    pub fn presets(
        mut self,
        presets: &'static [&'static str],
        preview: impl Fn(&str) -> Option<String> + 'static,
    ) -> Self {
        debug_assert!(
            self.spelling.argument.is_some(),
            "{} takes no value to preset",
            self.spelling.name
        );
        self.presets = presets;
        self.preview = Some(Rc::new(preview));
        self
    }

    /// It acts on the screen in view, so it ranks before a command the
    /// words mean as surely.
    pub fn here(mut self) -> Self {
        self.here = true;
        self
    }

    pub fn name(&self) -> &'static str {
        self.spelling.name
    }

    pub fn spelling(&self) -> &'static Spelling {
        self.spelling
    }

    pub fn target_text(&self) -> Option<&str> {
        match self.target.as_ref()? {
            Target::Shown(text) | Target::Here(text) => Some(text),
        }
    }

    /// Whether words typed after its name name a place to act from.
    pub(crate) fn takes_place(&self) -> bool {
        matches!(self.target, Some(Target::Here(_)))
    }

    /// Runs it, as `enter` on its row does.
    pub fn run(
        &self,
        argument: Option<String>,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut Context<Frame>,
    ) {
        (self.run)(frame, argument, window, cx)
    }
}
