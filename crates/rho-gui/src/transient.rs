//! Magit-style transient menus: the keyboard-first command layer.
//!
//! A transient is data — a title and rows of (key, label, action) — shown
//! in the bottom strip while it captures the keyboard. A key either runs a
//! command and closes, opens a nested transient, or drops into the
//! minibuffer for a value. The full menu appears immediately and stays up
//! for toggles. There is no textual command grammar — commands are Rust
//! values, the menus are how fingers reach them.

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
    label: String,
    run: TransientRun,
    /// A toggle: running it keeps the menu open (magit's do-stay), so
    /// several toggles chain without reopening.
    stay: bool,
    /// Menu-time applicability: items whose context is missing (no agent
    /// selected, say) drop out at open instead of failing when pressed.
    when: Option<fn(&Workspace) -> bool>,
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

    fn push(
        mut self,
        key: &'static str,
        label: impl Into<String>,
        stay: bool,
        when: Option<fn(&Workspace) -> bool>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.items.push(TransientItem {
            key,
            label: label.into(),
            run: Rc::new(run),
            stay,
            when,
        });
        self
    }

    fn item(
        self,
        key: &'static str,
        label: impl Into<String>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.push(key, label, false, None, run)
    }

    /// An item that keeps the menu open after running.
    fn toggle(
        self,
        key: &'static str,
        label: impl Into<String>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.push(key, label, true, None, run)
    }

    /// An item present only while `when` holds at menu open.
    fn item_when(
        self,
        when: fn(&Workspace) -> bool,
        key: &'static str,
        label: impl Into<String>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.push(key, label, false, Some(when), run)
    }

    /// Drops items whose context predicate fails right now.
    pub fn retain_applicable(&mut self, workspace: &Workspace) {
        self.items
            .retain(|item| item.when.is_none_or(|when| when(workspace)));
    }

    /// The action bound to `keystroke` and whether the menu stays open.
    pub fn action_for(&self, keystroke: &Keystroke) -> Option<(TransientRun, bool)> {
        self.items
            .iter()
            .find(|item| matches_key(item.key, keystroke))
            .map(|item| (item.run.clone(), item.stay))
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
                    .child(item.label.clone())
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

/// `space` — the root menu: every leader chord lives here (or one level
/// down), so the whole vocabulary is discoverable by pausing.
pub fn root_menu() -> Transient {
    Transient::new("rho")
        .item("n", "new agent", |workspace, window, cx| {
            workspace.open_new_agent_transient(window, cx);
        })
        .item_when(
            Workspace::has_selected_agent,
            "a",
            "agent…",
            |workspace, window, cx| {
                workspace.open_transient(agent_menu(), window, cx);
            },
        )
        .item_when(
            Workspace::has_focused_workstream,
            "s",
            "workstream…",
            |workspace, window, cx| {
                workspace.open_transient(workstream_menu(), window, cx);
            },
        )
        .item("w", "window…", |workspace, window, cx| {
            workspace.open_transient(window_menu(), window, cx);
        })
        .item("r", "rail", |workspace, window, cx| {
            workspace.focus_rail(window, cx);
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

pub fn new_agent_menu(project: String, mode: String, target: String, role: String) -> Transient {
    Transient::new("new agent")
        .item("p", format!("project  {project}"), |workspace, window, cx| {
            workspace.prompt_new_agent_project(window, cx);
        })
        .toggle("m", format!("workspace  {mode}"), |workspace, window, cx| {
            workspace.cycle_new_agent_mode(window, cx);
        })
        .item("b", format!("base  {target}"), |workspace, window, cx| {
            workspace.prompt_new_agent_base(window, cx);
        })
        .toggle("r", format!("role  {role}"), |workspace, window, cx| {
            workspace.cycle_new_agent_role(window, cx);
        })
        .item("c", "compose", |workspace, window, cx| {
            workspace.compose_new_agent(window, cx);
        })
}

/// `space w`: pane arrangement, on vim's window letters — practiced
/// `space w v` fingers land exactly where they always did.
fn window_menu() -> Transient {
    Transient::new("window")
        .item("v", "split right", |workspace, window, cx| {
            workspace.split_pane(crate::pane::SplitAxis::Row, window, cx);
        })
        .item("s", "split down", |workspace, window, cx| {
            workspace.split_pane(crate::pane::SplitAxis::Column, window, cx);
        })
        .item("q", "close pane", |workspace, window, cx| {
            workspace.close_pane(window, cx);
        })
        .item("w", "focus next", |workspace, window, cx| {
            workspace.focus_pane_by_delta(1, window, cx);
        })
        .item("b", "back", |workspace, window, cx| {
            workspace.pane_back(window, cx);
        })
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

/// `space a`: driving the current conversation.
fn agent_menu() -> Transient {
    Transient::new("agent")
        .item("d", "done", |workspace, window, cx| {
            workspace.cmd_agent_done(false, window, cx);
        })
        .item("shift-d", "done+hide", |workspace, window, cx| {
            workspace.cmd_agent_done(true, window, cx);
        })
        .item("s", "snooze…", |workspace, window, cx| {
            workspace.open_transient(snooze_menu(), window, cx);
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

/// `space s`: the focused workstream as the rail row the user is
/// triaging — name it, keep it up, put it away, file it with its kin.
fn workstream_menu() -> Transient {
    Transient::new("workstream")
        .item("r", "rename…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Rename, window, cx);
        })
        .toggle("p", "pin", |workspace, _, cx| {
            workspace.cmd_workstream_pin(cx);
        })
        .toggle("h", "hide", |workspace, _, cx| {
            workspace.cmd_workstream_hide(cx);
        })
        .item("g", "group…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Group, window, cx);
        })
        .item("l", "add label…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Label, window, cx);
        })
        .item("u", "remove label…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Unlabel, window, cx);
        })
        .item("m", "merge into…", |workspace, window, cx| {
            workspace.prompt_workstream(WorkstreamPrompt::Merge, window, cx);
        })
}
