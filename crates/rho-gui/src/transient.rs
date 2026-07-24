//! Magit-style transient menus: the keyboard-first command layer.
//!
//! A transient is data — a title and rows of (key, label, action) — shown
//! in the bottom strip while it captures the keyboard. A key either runs a
//! command and closes, opens a nested transient, or drops into the
//! minibuffer for a value. The full menu appears immediately and stays up
//! for toggles. There is no textual command grammar — commands are Rust
//! values, the menus are how fingers reach them.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Hsla, Keystroke, PathBuilder, Pixels, Point, Window, canvas, div, point,
    px, rgb,
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
    global_usage: Option<Vec<rho_ui_proto::AgentUsageSeries>>,
}

impl Transient {
    fn new(title: &'static str) -> Self {
        Self {
            title,
            items: Vec::new(),
            quota_usage: None,
            global_usage: None,
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
            let fable: Hsla = rgb(0xd97757).into();
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
                        .child(div().text_color(fable).child("claude")),
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
        if let Some(series) = &self.global_usage {
            let series = series.clone();
            let gpt: Hsla = colors.terminal_ansi_cyan.into();
            let claude: Hsla = rgb(0xd97757).into();
            let grid: Hsla = colors.text_muted.opacity(0.22).into();
            let now = crate::workspace::now_ms();
            let since = now.saturating_sub(7 * 24 * 60 * 60 * 1_000);
            let gpt_cost = provider_cost(&series, "gpt", since);
            let claude_cost = provider_cost(&series, "claude", since);
            let total_cost = gpt_cost + claude_cost;
            let requests = series
                .iter()
                .flat_map(|series| &series.buckets)
                .filter(|bucket| bucket.bucket_start_ms >= since)
                .map(|bucket| bucket.requests)
                .sum::<u64>();
            let approximate = series
                .iter()
                .flat_map(|series| &series.buckets)
                .filter(|bucket| bucket.bucket_start_ms >= since)
                .any(|bucket| bucket.approximate);
            return bottom_strip(text_style, cx)
                .child(
                    div()
                        .px_2()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("model cost · last 7 days"),
                )
                .child(div().px_2().text_color(muted).child(format!(
                    "${total_cost:.2} estimated API cost · {requests} requests{}",
                    if approximate {
                        " · includes approximate backfill"
                    } else {
                        ""
                    }
                )))
                .child(
                    div()
                        .flex()
                        .gap_4()
                        .px_2()
                        .child(
                            div()
                                .text_color(claude)
                                .child(format!("claude ${claude_cost:.2}")),
                        )
                        .child(div().text_color(gpt).child(format!("gpt ${gpt_cost:.2}"))),
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
                                    .h(px(220.))
                                    .w(px(64.))
                                    .pr_2()
                                    .flex()
                                    .flex_col()
                                    .items_end()
                                    .justify_between()
                                    .child(format!("${total_cost:.2}"))
                                    .child(format!("${:.2}", total_cost / 2.0))
                                    .child("$0"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div().w(px(832.)).h(px(220.)).child(global_usage_chart(
                                            series, now, gpt, claude, grid,
                                        )),
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
        .item("c", "model cost", |workspace, window, cx| {
            workspace.open_global_usage_transient(window, cx);
        })
}

pub fn usage_menu(series: Vec<rho_ui_proto::QuotaSeries>) -> Transient {
    let mut menu = Transient::new("provider quota");
    menu.quota_usage = Some(series);
    menu
}

pub fn global_usage_menu(series: Vec<rho_ui_proto::AgentUsageSeries>) -> Transient {
    let mut menu = Transient::new("model cost");
    menu.global_usage = Some(series);
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
    let points = points
        .iter()
        .copied()
        .fold(Vec::<Point<Pixels>>::new(), |mut points, point| {
            if let Some(previous) = points.last_mut()
                && point.x <= previous.x
            {
                *previous = point;
            } else {
                points.push(point);
            }
            points
        });
    let Some(first) = points.first().copied() else {
        return;
    };
    let mut builder = PathBuilder::stroke(px(2.));
    builder.move_to(first);
    if points.len() == 2 {
        builder.line_to(points[1]);
    } else if points.len() > 2 {
        let xs = points
            .iter()
            .map(|point| f64::from(point.x))
            .collect::<Vec<_>>();
        let ys = points
            .iter()
            .map(|point| f64::from(point.y))
            .collect::<Vec<_>>();
        let slopes = pchip_slopes(&xs, &ys);
        for (index, pair) in points.windows(2).enumerate() {
            let to = pair[1];
            let width = xs[index + 1] - xs[index];
            builder.cubic_bezier_to(
                to,
                point(
                    px((xs[index] + width / 3.0) as f32),
                    px((ys[index] + slopes[index] * width / 3.0) as f32),
                ),
                point(
                    px((xs[index + 1] - width / 3.0) as f32),
                    px((ys[index + 1] - slopes[index + 1] * width / 3.0) as f32),
                ),
            );
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn pchip_slopes(xs: &[f64], ys: &[f64]) -> Vec<f64> {
    debug_assert_eq!(xs.len(), ys.len());
    debug_assert!(xs.len() >= 3);
    let widths = xs
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect::<Vec<_>>();
    let secants = ys
        .windows(2)
        .zip(&widths)
        .map(|(pair, width)| (pair[1] - pair[0]) / width)
        .collect::<Vec<_>>();
    let mut slopes = vec![0.0; xs.len()];
    slopes[0] = pchip_endpoint(widths[0], widths[1], secants[0], secants[1]);
    for index in 1..xs.len() - 1 {
        let before = secants[index - 1];
        let after = secants[index];
        if before == 0.0 || after == 0.0 || before.signum() != after.signum() {
            slopes[index] = 0.0;
        } else {
            let before_weight = 2.0 * widths[index] + widths[index - 1];
            let after_weight = widths[index] + 2.0 * widths[index - 1];
            slopes[index] =
                (before_weight + after_weight) / (before_weight / before + after_weight / after);
        }
    }
    let last = widths.len() - 1;
    slopes[xs.len() - 1] = pchip_endpoint(
        widths[last],
        widths[last - 1],
        secants[last],
        secants[last - 1],
    );
    slopes
}

fn pchip_endpoint(width: f64, adjacent_width: f64, secant: f64, adjacent: f64) -> f64 {
    let mut slope =
        ((2.0 * width + adjacent_width) * secant - width * adjacent) / (width + adjacent_width);
    if slope.signum() != secant.signum() {
        slope = 0.0;
    } else if secant.signum() != adjacent.signum() && slope.abs() > 3.0 * secant.abs() {
        slope = 3.0 * secant;
    }
    slope
}

fn global_usage_chart(
    series: Vec<rho_ui_proto::AgentUsageSeries>,
    now: u64,
    gpt: Hsla,
    claude: Hsla,
    grid: Hsla,
) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds, _, window, _| {
            const BUCKET_MS: u64 = 5 * 60 * 1_000;
            const WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
            let start = now.saturating_sub(WINDOW_MS);
            let mut costs = HashMap::<u64, (f64, f64)>::new();
            for provider in &series {
                for bucket in &provider.buckets {
                    if bucket.bucket_start_ms < start {
                        continue;
                    }
                    let entry = costs.entry(bucket.bucket_start_ms).or_default();
                    let cost = bucket_cost_usd(bucket, &provider.provider);
                    if provider.provider == "claude" {
                        entry.1 += cost;
                    } else {
                        entry.0 += cost;
                    }
                }
            }

            let max = costs
                .values()
                .map(|(gpt, claude)| gpt + claude)
                .sum::<f64>()
                .max(f64::EPSILON);
            let to_point = |at: u64, value: f64| {
                let x = at.saturating_sub(start) as f64 / WINDOW_MS as f64;
                let y = 1.0 - value / max;
                point(
                    bounds.origin.x + bounds.size.width * x.clamp(0.0, 1.0) as f32,
                    bounds.origin.y + bounds.size.height * y.clamp(0.0, 1.0) as f32,
                )
            };
            let mut claude_total = 0.0;
            let mut gpt_total = 0.0;
            let mut points = vec![(
                to_point(start, claude_total),
                to_point(start, claude_total + gpt_total),
            )];
            let mut bucket_start = start.div_ceil(BUCKET_MS) * BUCKET_MS;
            while bucket_start <= now {
                points.push((
                    to_point(bucket_start, claude_total),
                    to_point(bucket_start, claude_total + gpt_total),
                ));
                if let Some((gpt_cost, claude_cost)) = costs.get(&bucket_start) {
                    gpt_total += gpt_cost;
                    claude_total += claude_cost;
                }
                let end = bucket_start.saturating_add(BUCKET_MS).min(now);
                points.push((
                    to_point(end, claude_total),
                    to_point(end, claude_total + gpt_total),
                ));
                bucket_start = bucket_start.saturating_add(BUCKET_MS);
            }

            let mut claude_area = PathBuilder::fill();
            claude_area.move_to(point(bounds.origin.x, bounds.bottom()));
            for (point, _) in &points {
                claude_area.line_to(*point);
            }
            claude_area.line_to(point(bounds.right(), bounds.bottom()));
            claude_area.close();
            if let Ok(path) = claude_area.build() {
                window.paint_path(path, claude.opacity(0.72));
            }

            let mut gpt_area = PathBuilder::fill();
            if let Some((_, first)) = points.first() {
                gpt_area.move_to(*first);
            }
            for (_, point) in &points[1..] {
                gpt_area.line_to(*point);
            }
            for (point, _) in points.iter().rev() {
                gpt_area.line_to(*point);
            }
            gpt_area.close();
            if let Ok(path) = gpt_area.build() {
                window.paint_path(path, gpt.opacity(0.72));
            }

            for step in 0..=4 {
                let y = bounds.origin.y + bounds.size.height * (step as f32 / 4.0);
                paint_grid_line(
                    point(bounds.origin.x, y),
                    point(bounds.right(), y),
                    grid,
                    window,
                );
            }
            for day in 1..7 {
                let x = bounds.origin.x + bounds.size.width * (day as f32 / 7.0);
                paint_grid_line(
                    point(x, bounds.origin.y),
                    point(x, bounds.bottom()),
                    grid,
                    window,
                );
            }
        },
    )
    .size_full()
}

fn paint_grid_line(from: Point<Pixels>, to: Point<Pixels>, color: Hsla, window: &mut Window) {
    let mut builder = PathBuilder::stroke(px(1.));
    builder.move_to(from);
    builder.line_to(to);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn provider_cost(series: &[rho_ui_proto::AgentUsageSeries], provider: &str, since: u64) -> f64 {
    series
        .iter()
        .filter(|series| series.provider == provider)
        .flat_map(|series| &series.buckets)
        .filter(|bucket| bucket.bucket_start_ms >= since)
        .map(|bucket| bucket_cost_usd(bucket, provider))
        .sum()
}

fn bucket_cost_usd(bucket: &rho_ui_proto::AgentUsageBucket, provider: &str) -> f64 {
    let (input, cache_read, cache_write, output) = match provider {
        "claude" => (10.0, 1.0, 12.5, 50.0),
        _ => (5.0, 0.5, 6.25, 30.0),
    };
    (bucket.input_tokens as f64 * input
        + bucket.cache_read_tokens as f64 * cache_read
        + bucket.cache_write_tokens as f64 * cache_write
        + bucket.output_tokens as f64 * output)
        / 1_000_000.0
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_cost_uses_provider_cache_rates() {
        let usage = rho_ui_proto::AgentUsageBucket {
            input_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            cache_write_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        };
        assert_eq!(bucket_cost_usd(&usage, "claude"), 73.5);
        assert_eq!(bucket_cost_usd(&usage, "gpt"), 41.75);
    }

    #[test]
    fn pchip_preserves_linear_slope() {
        assert_eq!(
            pchip_slopes(&[0.0, 2.0, 5.0], &[1.0, 3.0, 6.0]),
            vec![1.0, 1.0, 1.0]
        );
    }

    #[test]
    fn pchip_monotone_samples_do_not_overshoot() {
        let xs = [0.0, 1.0, 4.0, 10.0];
        let ys = [0.0, 2.0, 3.0, 8.0];
        let slopes = pchip_slopes(&xs, &ys);
        for index in 0..xs.len() - 1 {
            let width = xs[index + 1] - xs[index];
            for step in 0..=100 {
                let t = f64::from(step) / 100.0;
                let value = (2.0 * t.powi(3) - 3.0 * t.powi(2) + 1.0) * ys[index]
                    + (t.powi(3) - 2.0 * t.powi(2) + t) * width * slopes[index]
                    + (-2.0 * t.powi(3) + 3.0 * t.powi(2)) * ys[index + 1]
                    + (t.powi(3) - t.powi(2)) * width * slopes[index + 1];
                assert!(value >= ys[index] && value <= ys[index + 1]);
            }
        }
    }
}
