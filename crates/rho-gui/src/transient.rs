//! Magit-style transient menus: the keyboard-first command layer.
//!
//! A transient is data — a title and rows of (key, label, action) — shown
//! in the bottom strip while it captures the keyboard. A key either runs a
//! command and closes, opens a nested transient, or drops into the
//! minibuffer for a value. Typed fast, `space a d` acts before the menu
//! registers visually; pausing shows every option. There is no textual
//! command grammar — commands are Rust values, the menus are how fingers
//! reach them.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, Context, Keystroke, Window, div};
use theme::ActiveTheme as _;

use crate::minibuffer::bottom_strip;
use crate::workspace::{WorkstreamPrompt, Workspace};

pub type TransientRun = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

pub struct TransientItem {
    /// Keystroke in binding notation: `"d"`, `"shift-d"`, `"3"`.
    key: &'static str,
    label: &'static str,
    run: TransientRun,
}

pub struct Transient {
    title: &'static str,
    items: Vec<TransientItem>,
}

impl Transient {
    fn new(title: &'static str) -> Self {
        Self {
            title,
            items: Vec::new(),
        }
    }

    fn item(
        mut self,
        key: &'static str,
        label: &'static str,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.items.push(TransientItem {
            key,
            label,
            run: Rc::new(run),
        });
        self
    }

    /// The action bound to `keystroke`, if any.
    pub fn action_for(&self, keystroke: &Keystroke) -> Option<TransientRun> {
        self.items
            .iter()
            .find(|item| matches_key(item.key, keystroke))
            .map(|item| item.run.clone())
    }

    /// Magit's layout: a title line, then items flowing down short columns
    /// so the eye scans vertically. Keys align in their own sub-column.
    pub fn render(&self, text_style: &gpui::TextStyle, cx: &Context<Workspace>) -> AnyElement {
        const COLUMN_ROWS: usize = 4;
        let colors = cx.theme().colors();
        let accent = colors.text_accent;
        let muted = colors.text_muted;
        let columns = self.items.chunks(COLUMN_ROWS).map(|chunk| {
            div().flex().flex_col().children(chunk.iter().map(|item| {
                div()
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .w_8()
                            .text_align(gpui::TextAlign::Right)
                            .pr_2()
                            .text_color(accent)
                            .child(display_key(item.key)),
                    )
                    .child(item.label)
            }))
        });
        bottom_strip(text_style, cx)
            .child(div().px_2().text_color(muted).child(self.title))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_x_6()
                    .px_2()
                    .children(columns),
            )
            .into_any_element()
    }
}

fn matches_key(spec: &str, keystroke: &Keystroke) -> bool {
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

fn display_key(spec: &str) -> String {
    match spec.strip_prefix("shift-") {
        Some(rest) => rest.to_uppercase(),
        None => spec.to_owned(),
    }
}

/// `space :` — the root menu: everything that isn't about the current
/// agent, plus a door into the agent menu.
pub fn root_menu() -> Transient {
    Transient::new("rho")
        .item("a", "agent…", |workspace, window, cx| {
            workspace.open_transient(agent_menu(), window, cx);
        })
        .item("b", "switch buffer…", |workspace, window, cx| {
            workspace.open_buffer_picker(window, cx);
        })
        .item("k", "close buffer", |workspace, window, cx| {
            workspace.close_surface(None, window, cx);
        })
        .item("f", "open file…", |workspace, window, cx| {
            workspace.prompt_open_file(window, cx);
        })
        .item("t", "terminal", |workspace, _, cx| {
            workspace.cmd_term(false, cx);
        })
        .item("shift-t", "new terminal", |workspace, _, cx| {
            workspace.cmd_term(true, cx);
        })
        .item("p", "projects…", |workspace, window, cx| {
            workspace.open_transient(projects_menu(), window, cx);
        })
        .item("v", "version", |workspace, _, cx| {
            workspace.cmd_version(cx);
        })
        .item("q", "quit", |_, _, cx| cx.quit())
}

fn projects_menu() -> Transient {
    Transient::new("projects")
        .item("a", "add…", |workspace, window, cx| {
            workspace.prompt_project_add(window, cx);
        })
        .item("r", "remove…", |workspace, window, cx| {
            workspace.prompt_project_remove(window, cx);
        })
}

/// `space a`: everything about the current agent.
pub fn agent_menu() -> Transient {
    Transient::new("agent")
        .item("d", "done", |workspace, window, cx| {
            workspace.cmd_agent_done(false, window, cx);
        })
        .item("shift-d", "done+hide", |workspace, window, cx| {
            workspace.cmd_agent_done(true, window, cx);
        })
        .item("r", "rename…", |workspace, window, cx| {
            workspace.prompt_rename(window, cx);
        })
        .item("s", "snooze…", |workspace, window, cx| {
            workspace.open_transient(snooze_menu(), window, cx);
        })
        .item("p", "pin", |workspace, _, cx| {
            workspace.cmd_agent_pin(cx);
        })
        .item("c", "cancel turn", |workspace, _, cx| {
            workspace.cmd_agent_cancel(cx);
        })
        .item("k", "compact", |workspace, _, cx| {
            workspace.cmd_compact(cx);
        })
        .item("w", "rewind turn", |workspace, _, cx| {
            workspace.cmd_rewind(1, cx);
        })
        .item("shift-w", "rewind turns…", |workspace, window, cx| {
            workspace.prompt_rewind(window, cx);
        })
        .item("shift-c", "continue turn", |workspace, _, cx| {
            workspace.cmd_continue_turn(cx);
        })
        .item("shift-k", "new prompt cache key", |workspace, _, cx| {
            workspace.cmd_change_prompt_cache_key(cx);
        })
        .item("n", "new agent", |workspace, window, cx| {
            workspace.cmd_agent_new(window, cx);
        })
        .item("shift-t", "workstream…", |workspace, window, cx| {
            workspace.open_transient(workstream_menu(), window, cx);
        })
}

fn snooze_menu() -> Transient {
    const MINUTE_MS: u64 = 60 * 1000;
    Transient::new("snooze")
        .item("3", "30 minutes", |workspace, _, cx| {
            workspace.cmd_agent_snooze(30 * MINUTE_MS, cx);
        })
        .item("h", "2 hours", |workspace, _, cx| {
            workspace.cmd_agent_snooze(2 * 60 * MINUTE_MS, cx);
        })
        .item("d", "1 day", |workspace, _, cx| {
            workspace.cmd_agent_snooze(24 * 60 * MINUTE_MS, cx);
        })
        .item("c", "custom…", |workspace, window, cx| {
            workspace.prompt_snooze(window, cx);
        })
}

/// `space a T`: workstream operations, rare enough to live one level down.
fn workstream_menu() -> Transient {
    Transient::new("workstream")
        .item("m", "move agent here…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Move, window, cx);
        })
        .item("g", "group workstream…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Group, window, cx);
        })
        .item("l", "add label…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Label, window, cx);
        })
        .item("u", "remove label…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Unlabel, window, cx);
        })
        .item("r", "rename workstream…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Rename, window, cx);
        })
        .item("p", "pin workstream", |workspace, _, cx| {
            workspace.cmd_workstream_pin(cx);
        })
}
