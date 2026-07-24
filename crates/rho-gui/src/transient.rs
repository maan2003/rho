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
use gpui::{
    AnyElement, Context, Hsla, Keystroke, PathBuilder, Pixels, Point, Window, canvas, div, point,
    px,
};
use theme::ActiveTheme as _;

use crate::minibuffer::bottom_strip;
use crate::workspace::{Workspace, WorkstreamPrompt};

pub type TransientRun = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

pub struct TransientItem {
    /// Keystroke in binding notation: `"d"`, `"shift-d"`, `"3"`.
    key: &'static str,
    description: String,
    /// Infixes display their current value separately from their description.
    value: Option<String>,
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
    quota_usage: Option<Vec<rho_ui_proto::QuotaSeries>>,
    agent_usage: Option<(
        String,
        Vec<rho_ui_proto::AgentUsageBucket>,
        rho_ui_proto::AgentUsageBucket,
    )>,
}

impl Transient {
    fn new(title: &'static str) -> Self {
        Self {
            title,
            items: Vec::new(),
            quota_usage: None,
            agent_usage: None,
        }
    }

    pub fn title(&self) -> &'static str {
        self.title
    }

    fn push(
        mut self,
        key: &'static str,
        description: impl Into<String>,
        value: Option<String>,
        stay: bool,
        when: Option<fn(&Workspace) -> bool>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.items.push(TransientItem {
            key,
            description: description.into(),
            value,
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
        self.push(key, label, None, false, None, run)
    }

    /// An item that keeps the menu open after running.
    fn toggle(
        self,
        key: &'static str,
        label: impl Into<String>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.push(key, label, None, true, None, run)
    }

    /// A value-setting item. Like upstream Transient infixes, its current
    /// value is rendered separately from the command description.
    fn infix(
        self,
        key: &'static str,
        description: impl Into<String>,
        value: impl Into<String>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.push(key, description, Some(value.into()), false, None, run)
    }

    /// An infix that updates immediately and keeps the transient open.
    fn infix_toggle(
        self,
        key: &'static str,
        description: impl Into<String>,
        value: impl Into<String>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.push(key, description, Some(value.into()), true, None, run)
    }

    /// An item present only while `when` holds at menu open.
    fn item_when(
        self,
        when: fn(&Workspace) -> bool,
        key: &'static str,
        label: impl Into<String>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        self.push(key, label, None, false, Some(when), run)
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
        let value_color = colors.terminal_ansi_green;
        if let Some(series) = &self.quota_usage {
            let series = series.clone();
            let gpt: Hsla = colors.terminal_ansi_cyan.into();
            let fable: Hsla = colors.terminal_ansi_magenta.into();
            let grid: Hsla = colors.text_muted.opacity(0.22).into();
            return bottom_strip(text_style, cx)
                .child(
                    div()
                        .px_2()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("usage · last 7 days"),
                )
                .child(
                    div()
                        .flex()
                        .gap_4()
                        .px_2()
                        .child(div().text_color(gpt).child("gpt"))
                        .child(div().text_color(fable).child("fable")),
                )
                .child(
                    div().px_2().pb_1().child(
                        div()
                            .flex()
                            .items_start()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(
                                div()
                                    .flex()
                                    .h(px(240.))
                                    .w(px(36.))
                                    .flex_col()
                                    .justify_between()
                                    .child("100%")
                                    .child("50%")
                                    .child("0%"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .w(px(832.))
                                            .h(px(240.))
                                            .child(usage_chart(series, gpt, fable, grid)),
                                    )
                                    .child(
                                        div()
                                            .mt_1()
                                            .flex()
                                            .w(px(832.))
                                            .justify_between()
                                            .child("−7d")
                                            .child("now"),
                                    ),
                            ),
                    ),
                )
                .into_any_element();
        }
        if let Some((label, buckets, total)) = &self.agent_usage {
            let buckets = buckets.clone();
            let line: Hsla = colors.terminal_ansi_cyan.into();
            let grid: Hsla = colors.text_muted.opacity(0.22).into();
            let total_tokens = total.input_tokens
                + total.cache_read_tokens
                + total.cache_write_tokens
                + total.output_tokens;
            return bottom_strip(text_style, cx)
                .child(
                    div()
                        .px_2()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(format!("usage · {label}")),
                )
                .child(div().px_2().text_color(muted).child(format!(
                    "{total_tokens} tokens · {} requests · input {} · cache {} · output {}{}",
                    total.requests,
                    total.input_tokens,
                    total.cache_read_tokens + total.cache_write_tokens,
                    total.output_tokens,
                    if total.approximate {
                        " · includes approximate backfill"
                    } else {
                        ""
                    }
                )))
                .child(
                    div().px_2().pb_1().child(
                        div()
                            .w(px(832.))
                            .h(px(240.))
                            .child(agent_usage_chart(buckets, line, grid)),
                    ),
                )
                .into_any_element();
        }
        let columns = self.items.chunks(COLUMN_ROWS).map(|chunk| {
            div().flex().flex_col().children(chunk.iter().map(|item| {
                let mut row = div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .child(
                        div()
                            .w_8()
                            .text_align(gpui::TextAlign::Right)
                            .pr_2()
                            .text_color(accent)
                            .child(display_key(item.key)),
                    )
                    .child(item.description.clone());
                if let Some(value) = &item.value {
                    row = row
                        .child(div().pl_1().text_color(muted).child("("))
                        .child(
                            div()
                                .text_color(value_color)
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(value.clone()),
                        )
                        .child(div().text_color(muted).child(")"));
                }
                row
            }))
        });
        bottom_strip(text_style, cx)
            .child(
                div()
                    .px_2()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(self.title),
            )
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
        .item("c", "start/attach shell", |workspace, _, cx| {
            workspace.cmd_shell(cx);
        })
        .item("shift-c", "close shell", |workspace, _, cx| {
            workspace.cmd_shell_close(cx);
        })
        .item_when(
            Workspace::has_selected_agent,
            "d",
            "changes",
            |workspace, _, cx| workspace.cmd_diff(cx),
        )
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
        .item("u", "usage…", |workspace, window, cx| {
            workspace.open_transient(usage_root_menu(), window, cx);
        })
        .item("q", "quit", |_, _, cx| cx.quit())
}

pub fn usage_root_menu() -> Transient {
    Transient::new("usage")
        .item("q", "provider quota", |workspace, window, cx| {
            workspace.open_usage_transient(window, cx);
        })
        .item_when(
            Workspace::has_selected_agent,
            "a",
            "current agent tokens",
            |workspace, window, cx| workspace.open_agent_usage_transient(window, cx),
        )
        .item("s", "agent by handle…", |workspace, window, cx| {
            workspace.prompt_agent_usage(window, cx);
        })
}

pub fn usage_menu(series: Vec<rho_ui_proto::QuotaSeries>) -> Transient {
    let mut menu = Transient::new("provider quota");
    menu.quota_usage = Some(series);
    menu
}

pub fn agent_usage_menu(
    label: String,
    buckets: Vec<rho_ui_proto::AgentUsageBucket>,
    total: rho_ui_proto::AgentUsageBucket,
) -> Transient {
    let mut menu = Transient::new("agent usage");
    menu.agent_usage = Some((label, buckets, total));
    menu
}

fn usage_chart(
    series: Vec<rho_ui_proto::QuotaSeries>,
    gpt: Hsla,
    fable: Hsla,
    grid: Hsla,
) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds, _, window, _| {
            let pixels_per_percent = bounds.size.height / 100.0;
            for percent in (0..=100).step_by(10) {
                let y = bounds.origin.y + pixels_per_percent * (100.0 - percent as f32);
                let mut builder = PathBuilder::stroke(px(1.));
                builder.move_to(point(bounds.origin.x, y));
                builder.line_to(point(bounds.right(), y));
                if let Ok(path) = builder.build() {
                    window.paint_path(path, grid);
                }
            }

            let now = crate::workspace::now_ms();
            let start = now.saturating_sub(7 * 24 * 60 * 60 * 1_000);
            for model in &series {
                let color = if model.model == "fable" { fable } else { gpt };
                let mut segment = Vec::new();
                let mut previous: Option<&rho_ui_proto::QuotaPoint> = None;
                for sample in &model.points {
                    let reset = previous.is_some_and(|old| {
                        let reset_time_changed = match (old.reset_at_unix, sample.reset_at_unix) {
                            (Some(old), Some(new)) => old.abs_diff(new) > 60,
                            (None, None) => false,
                            _ => true,
                        };
                        reset_time_changed || sample.remaining_percent > old.remaining_percent
                    });
                    if reset {
                        paint_usage_segment(&segment, color, window);
                        segment.clear();
                    }
                    let elapsed = sample.observed_at_ms.saturating_sub(start);
                    let x_ratio = (elapsed as f64 / (now.saturating_sub(start).max(1)) as f64)
                        .clamp(0.0, 1.0) as f32;
                    segment.push(point(
                        bounds.origin.x + bounds.size.width * x_ratio,
                        bounds.origin.y
                            + pixels_per_percent * (100.0 - f32::from(sample.remaining_percent)),
                    ));
                    previous = Some(sample);
                }
                paint_usage_segment(&segment, color, window);
            }
        },
    )
    .size_full()
}

fn paint_usage_segment(points: &[Point<Pixels>], color: Hsla, window: &mut Window) {
    let Some(first) = points.first().copied() else {
        return;
    };
    let mut builder = PathBuilder::stroke(px(2.));
    builder.move_to(first);
    for pair in points.windows(2) {
        let from = pair[0];
        let to = pair[1];
        let mid_x = from.x + (to.x - from.x) / 2.0;
        builder.cubic_bezier_to(to, point(mid_x, from.y), point(mid_x, to.y));
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn agent_usage_chart(
    buckets: Vec<rho_ui_proto::AgentUsageBucket>,
    line: Hsla,
    grid: Hsla,
) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds, _, window, _| {
            for step in 0..=4 {
                let y = bounds.origin.y + bounds.size.height * (step as f32 / 4.0);
                let mut builder = PathBuilder::stroke(px(1.));
                builder.move_to(point(bounds.origin.x, y));
                builder.line_to(point(bounds.right(), y));
                if let Ok(path) = builder.build() {
                    window.paint_path(path, grid);
                }
            }
            let max = buckets
                .iter()
                .map(agent_bucket_tokens)
                .max()
                .unwrap_or(1)
                .max(1);
            let now = crate::workspace::now_ms();
            let start = now.saturating_sub(7 * 24 * 60 * 60 * 1_000);
            let points = buckets
                .iter()
                .map(|bucket| {
                    let x = bucket.bucket_start_ms.saturating_sub(start) as f64
                        / now.saturating_sub(start).max(1) as f64;
                    let y = 1.0 - agent_bucket_tokens(bucket) as f64 / max as f64;
                    point(
                        bounds.origin.x + bounds.size.width * x.clamp(0.0, 1.0) as f32,
                        bounds.origin.y + bounds.size.height * y as f32,
                    )
                })
                .collect::<Vec<_>>();
            paint_usage_segment(&points, line, window);
        },
    )
    .size_full()
}

fn agent_bucket_tokens(bucket: &rho_ui_proto::AgentUsageBucket) -> u64 {
    bucket.input_tokens
        + bucket.cache_read_tokens
        + bucket.cache_write_tokens
        + bucket.output_tokens
}

pub fn new_agent_menu(project: String, workspace: String, role: String) -> Transient {
    Transient::new("new agent")
        .infix("p", "project", project, |workspace, window, cx| {
            workspace.prompt_new_agent_project(window, cx);
        })
        .infix("w", "workspace", workspace, |workspace, window, cx| {
            workspace.open_new_agent_workspace_transient(window, cx);
        })
        .infix_toggle("r", "role", role, |workspace, window, cx| {
            workspace.cycle_new_agent_role(window, cx);
        })
        .item("c", "compose", |workspace, window, cx| {
            workspace.compose_new_agent(window, cx);
        })
}

pub fn new_agent_workspace_menu() -> Transient {
    Transient::new("workspace")
        .item("n", "new on…", |workspace, window, cx| {
            workspace.prompt_new_agent_workspace(
                crate::draft_view::StartFieldMode::NewOn,
                window,
                cx,
            );
        })
        .item("j", "join…", |workspace, window, cx| {
            workspace.prompt_new_agent_workspace(
                crate::draft_view::StartFieldMode::Join,
                window,
                cx,
            );
        })
        .item("s", "sandbox on…", |workspace, window, cx| {
            workspace.prompt_new_agent_workspace(
                crate::draft_view::StartFieldMode::Sandbox,
                window,
                cx,
            );
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
        .item("shift-d", "hide", |workspace, window, cx| {
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
