//! The screen of one request that waits: who asks, where it goes, and
//! exactly what it would change, as a buffer the user reads like any other.

use editor::Editor;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Window, div};
use jiff::Timestamp;
use rho_agent_hosts::git::{display_field, display_lines, ref_name};
use rho_agent_hosts::protocol::{GitPush, GitRefChange, MAX_PUSH_DIFF_BYTES};
use rho_frame::{CardHere, Subject, Surface};
use theme::ActiveTheme as _;

/// What the screen shows, as the system read it when the card opened. A
/// request never changes while it waits; one that ends closes its screen.
pub(crate) struct Shown {
    pub title: String,
    /// The work that asks, `tide · Flaky clock test`, when it has a thread.
    pub work: Option<String>,
    pub asker: String,
    pub destination: String,
    /// What a push carries; `None` for a fetch.
    pub push: Option<GitPush>,
    pub asked: Timestamp,
}

impl Shown {
    /// The screen's text, one fact a line. The verdicts come before the
    /// diff, which can be long.
    pub(crate) fn text(&self) -> String {
        // The screen's font is proportional, so nothing lines up in
        // columns: each item sits indented on its own line.
        let mut lines = Vec::new();
        lines.extend(self.work.clone());
        let action = match self.push {
            Some(_) => "push",
            None => "fetch",
        };
        lines.push(format!(
            "{} asks to {action} with your ssh login",
            self.asker
        ));
        lines.push(String::new());
        lines.push(format!("repository {}", self.destination));
        match &self.push {
            Some(push) => push_lines(push, &mut lines),
            None => lines.push(
                "a fetch changes nothing there · Git names the refs it wants once you approve"
                    .to_owned(),
            ),
        }
        lines.push(format!("asked {}", local(self.asked, "%a %H:%M")));
        lines.push(String::new());
        lines.push("space a approves · space d denies · space s snoozes".to_owned());
        if let Some(push) = &self.push {
            diff_lines(push, &mut lines);
        }
        lines.join("\n")
    }
}

/// Each ref it changes, as the agent's repository last saw the remote, and
/// the commits it carries.
fn push_lines(push: &GitPush, lines: &mut Vec<String>) {
    lines.push("changes, from where the agent's repository last saw them".to_owned());
    lines.extend(push.updates.iter().map(|update| {
        let name = ref_name(&update.reference);
        let change = match &update.change {
            GitRefChange::Create { new } => format!("new at {}", short(new)),
            GitRefChange::FastForward { old, new } => format!("{}..{}", short(old), short(new)),
            GitRefChange::Force { old, new, dropped } => {
                let dropped = match dropped {
                    Some(1) => "drops 1 commit".to_owned(),
                    Some(count) => format!("drops {count} commits"),
                    None => "drops commits the agent never fetched".to_owned(),
                };
                format!("FORCE-PUSH {}..{} · {dropped}", short(old), short(new))
            }
            GitRefChange::Delete { old: Some(old) } => format!("deleted, was {}", short(old)),
            GitRefChange::Delete { old: None } => "deleted".to_owned(),
        };
        format!("  {name}  {change}")
    }));
    lines.push(match push.commit_count {
        1 => "1 commit".to_owned(),
        count => format!("{count} commits"),
    });
    lines.extend(push.commits.iter().map(|commit| {
        let date = Timestamp::from_second(commit.authored_at)
            .map_or_else(|_| "?".to_owned(), |at| local(at, "%Y-%m-%d"));
        format!(
            "  {} {} · {} · {date}",
            display_field(&commit.short_id),
            display_field(&commit.subject),
            display_field(&commit.author),
        )
    }));
    let unlisted = push.commit_count.saturating_sub(push.commits.len() as u64);
    if unlisted > 0 {
        lines.push(format!("  and {unlisted} more"));
    }
}

/// What each ref's update changes, stat first, then the patch.
fn diff_lines(push: &GitPush, lines: &mut Vec<String>) {
    for diff in &push.diffs {
        lines.push(String::new());
        lines.push(format!("diff of {}", ref_name(&diff.reference)));
        lines.extend(display_lines(&diff.text));
    }
    if push.diff_cut {
        lines.push(String::new());
        lines.push(format!(
            "the diff stops here: it is longer than {} KiB",
            MAX_PUSH_DIFF_BYTES / 1024
        ));
    }
}

/// An object id as the screen shows it.
fn short(object: &str) -> &str {
    &object[..object.len().min(8)]
}

fn local(at: Timestamp, format: &str) -> String {
    at.to_zoned(jiff::tz::TimeZone::system())
        .strftime(format)
        .to_string()
}

pub struct GitAskSurface {
    title: String,
    editor: Entity<Editor>,
}

impl GitAskSurface {
    pub(crate) fn new(shown: Shown, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = shown.text();
        let buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local(text, cx);
            buffer.set_capability(language::Capability::Read, cx);
            buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer, None, window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_read_only(true);
            editor
        });
        Self {
            title: shown.title,
            editor,
        }
    }
}

impl Surface for GitAskSurface {
    fn title(&self, _cx: &App) -> String {
        self.title.clone()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    fn subject(&self, _card: Option<rho_frame::surface::Seen>, _cx: &App) -> Subject {
        Subject::Own
    }

    fn card(&self, _cx: &App) -> CardHere {
        CardHere::Itself
    }
}

impl Focusable for GitAskSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for GitAskSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("RhoGitAsk")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}
