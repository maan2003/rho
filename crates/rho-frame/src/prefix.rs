//! `space`: the silent prefix (`window.Screen`).
//!
//! Helix's leader without its popup: `space` arms the prefix, the next key
//! runs what it is bound to, and `escape` or a key bound to nothing
//! disarms it. Nothing is drawn and nothing waits on a timer, so a fast
//! chord never flickers. A key the prefix knows but refuses here, as `d`
//! where nothing waits, says why on the echo line. What each key does is
//! listed with its keys in `Commands` (`:`), the one directory of every
//! action.
//!
//! A system hands the frame rows for the card verdicts only it can give
//! (`approve` and `deny` on a staged push); the frame binds them beside
//! its own.

use std::rc::Rc;

use gpui::{Context, Keystroke, Window};

use crate::frame::Frame;

/// What a row does when its key follows `space`.
pub type Run = Rc<dyn Fn(&mut Frame, &mut Window, &mut Context<Frame>)>;

/// One key after `space`: the key, the command it is, and what it does.
#[derive(Clone)]
pub struct Row {
    /// Binding notation: `d`, `shift-d`.
    pub key: String,
    /// What it is called: `done`, `approve`.
    pub word: String,
    pub(crate) run: Run,
}

impl Row {
    pub fn run(
        key: impl Into<String>,
        word: impl Into<String>,
        run: impl Fn(&mut Frame, &mut Window, &mut Context<Frame>) + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            word: word.into(),
            run: Rc::new(run),
        }
    }
}

/// The keys `space` binds over the screen in view, the first to claim a
/// key keeping it, and the keys it refuses here with the reason.
#[derive(Clone, Default)]
pub(crate) struct Prefix {
    rows: Vec<Row>,
    refused: Vec<(&'static str, String)>,
}

/// What the key after `space` meant.
pub(crate) enum Press {
    /// A modifier alone, as `shift` before `D`: still waiting.
    Pending,
    /// The prefix is spent: run this.
    Run(Run),
    /// The prefix is spent on a key it knows that does nothing here: say
    /// why.
    Refused(String),
    /// The prefix is spent on `escape` or a key bound to nothing.
    Cancelled,
}

impl Prefix {
    pub(crate) fn push(&mut self, row: Row) {
        if let Some(taken) = self.rows.iter().find(|held| held.key == row.key) {
            tracing::warn!(
                key = %row.key,
                kept = %taken.word,
                dropped = %row.word,
                "two rows on one key after space"
            );
            return;
        }
        self.rows.push(row);
    }

    /// `key` does nothing over this screen, for `reason`, unless a row
    /// binds it.
    pub(crate) fn refuse(&mut self, key: &'static str, reason: impl Into<String>) {
        self.refused.push((key, reason.into()));
    }

    /// Each key and what it is called.
    pub(crate) fn keys(&self) -> Vec<(String, String)> {
        self.rows
            .iter()
            .map(|row| (row.key.clone(), row.word.clone()))
            .collect()
    }

    pub(crate) fn press(&self, keystroke: &Keystroke) -> Press {
        if matches!(
            keystroke.key.as_str(),
            "shift" | "control" | "alt" | "platform" | "function"
        ) {
            return Press::Pending;
        }
        if let Some(row) = self.rows.iter().find(|row| matches(&row.key, keystroke)) {
            return Press::Run(row.run.clone());
        }
        match self.refused.iter().find(|(key, _)| matches(key, keystroke)) {
            Some((_, reason)) => Press::Refused(reason.clone()),
            None => Press::Cancelled,
        }
    }
}

/// Binding notation against a real keystroke. The keys after `space` are
/// plain and shifted letters; a chord with control or alt is no such key.
fn matches(spec: &str, keystroke: &Keystroke) -> bool {
    let (shift, key) = match spec.strip_prefix("shift-") {
        Some(rest) => (true, rest),
        None => (false, spec),
    };
    keystroke.key == key
        && keystroke.modifiers.shift == shift
        && !keystroke.modifiers.control
        && !keystroke.modifiers.alt
        && !keystroke.modifiers.platform
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(notation: &'static str) -> Keystroke {
        Keystroke::parse(notation).expect("a keystroke")
    }

    fn prefix() -> Prefix {
        let mut prefix = Prefix::default();
        prefix.push(Row::run("d", "done", |_, _, _| {}));
        prefix.push(Row::run("shift-s", "loud", |_, _, _| {}));
        // A second claim on a key loses to the first.
        prefix.push(Row::run("d", "deny", |_, _, _| {}));
        // A refusal gives way to a row on the same key.
        prefix.refuse("d", "not waiting here");
        prefix.refuse("u", "nothing to undo");
        prefix
    }

    fn pressed(prefix: &Prefix, notation: &'static str) -> String {
        match prefix.press(&key(notation)) {
            Press::Pending => "pending".into(),
            Press::Run(_) => "run".into(),
            Press::Refused(reason) => reason,
            Press::Cancelled => "cancelled".into(),
        }
    }

    #[test]
    fn the_next_key_runs_refuses_or_cancels_and_a_modifier_waits() {
        let prefix = prefix();
        assert_eq!(
            prefix.keys(),
            [
                ("d".into(), "done".into()),
                ("shift-s".into(), "loud".into())
            ],
            "a refused key is not bound"
        );
        assert_eq!(pressed(&prefix, "d"), "run");
        assert_eq!(pressed(&prefix, "shift-s"), "run");
        assert_eq!(pressed(&prefix, "u"), "nothing to undo");
        // `s` is not `S`, and a chord is no prefix key.
        assert_eq!(pressed(&prefix, "s"), "cancelled");
        assert_eq!(pressed(&prefix, "ctrl-d"), "cancelled");
        assert_eq!(pressed(&prefix, "ctrl-u"), "cancelled");
        assert_eq!(pressed(&prefix, "escape"), "cancelled");
        assert_eq!(pressed(&prefix, "shift"), "pending");
    }
}
