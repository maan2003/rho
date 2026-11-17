//! The painting: a summary already the size of the picture, drawn as a
//! block under the surface's header. Nothing here does arithmetic per
//! sample; a frame walks the reduced points the summary holds.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, Hsla, PathBuilder, Pixels, Point, TextStyle, Window, canvas, div,
    point, px, rgb,
};
use rho_agents_client::usage::{
    AgentCostSummary, ChartPoint, CostSummary, Legend, QuotaSummary, SeriesColor, ShareSummary,
};
use theme::ActiveTheme as _;

/// The least a chart is drawn at. Below this the lines are on top of each
/// other and the picture says nothing, so a short window scrolls instead.
const MIN_CHART_HEIGHT: Pixels = px(220.);

/// What the chart is not allowed to use: the frame's chrome, the header's
/// two lines, the legend above the chart and the day labels under it.
const CHART_CHROME: Pixels = px(150.);

/// What the block paints: already the size of the picture.
pub(super) enum Summary {
    Quota(QuotaSummary),
    Cost(CostSummary),
    Share(ShareSummary),
    AgentCost(AgentCostSummary),
}

/// How many points the chart can show: the window's width, since the chart
/// spans it. An overestimate by the gutter's width costs a few points and
/// never a coarser line.
pub(super) fn columns_for(window: &Window) -> usize {
    f32::from(window.viewport_size().width).max(1.0) as usize
}

/// How tall to draw the chart: what the screen has left under the header.
pub(super) fn height_for(window: &Window) -> Pixels {
    (window.viewport_size().height - CHART_CHROME).max(MIN_CHART_HEIGHT)
}

pub(super) fn backfill_note(approximate: bool) -> &'static str {
    if approximate {
        " · includes approximate backfill"
    } else {
        ""
    }
}

/// The chart as a block under the header, sized by what it draws.
pub(super) fn chart_block(
    anchor: multi_buffer::Anchor,
    summary: Arc<Summary>,
    height: Pixels,
) -> editor::display_map::BlockProperties<multi_buffer::Anchor> {
    editor::display_map::BlockProperties {
        placement: editor::display_map::BlockPlacement::Below(anchor),
        // A starting height, not the real one: the editor measures the
        // element and resizes the block to what it drew, the same way the
        // menu block is measured.
        height: Some(1),
        // Flex, not Fixed: a fixed block is measured at min-content, which
        // draws the chart at whatever its labels happen to need instead of
        // across the screen it now owns.
        style: editor::display_map::BlockStyle::Flex,
        render: Arc::new(move |cx| {
            // The editor's own text style, carried in by hand: a block's
            // element is not inside the editor's text, so it inherits
            // gpui's default — a black one, in the buffer's absence of a
            // font. Everything the chart draws in words hangs off this.
            let text_style = cx.editor_style.text.clone();
            render_chart(&summary, height, &text_style, cx.app).into_any_element()
        }),
        priority: 1,
    }
}

/// Every chart, wrapped in the one place its words get a colour and a font.
/// A `div` under a block inherits gpui's defaults, which are black text in a
/// font that is not the buffer's; the axis labels, the end labels and any
/// word added here would draw black on a dark theme and in the wrong face.
/// Setting it at the root rather than per element is what makes that true of
/// the next label as well as these — a legend entry that wants its series'
/// colour still says so and wins, because a child overrides its parent.
fn render_chart(summary: &Summary, height: Pixels, text_style: &TextStyle, cx: &App) -> AnyElement {
    let chart = match summary {
        Summary::Quota(quota) => render_quota(quota, height, cx),
        Summary::Cost(cost) => render_cost(cost, height, cx),
        Summary::Share(share) => render_share(share, height, cx),
        Summary::AgentCost(agent_cost) => render_agent_cost(agent_cost, height, cx),
    };
    div()
        .font_family(text_style.font_family.clone())
        .font_weight(text_style.font_weight)
        .text_size(text_style.font_size)
        .text_color(text_style.color)
        .child(chart)
        .into_any_element()
}

fn render_quota(summary: &QuotaSummary, height: Pixels, cx: &App) -> AnyElement {
    let lines = summary
        .lines
        .iter()
        .map(|line| (color_of(line.color, cx), line.segments.clone()))
        .collect::<Vec<_>>();
    let midnights = summary.midnights.clone();
    let grid = grid_color(cx);
    div()
        .flex()
        .flex_col()
        .child(legend_row(&summary.legend, cx))
        .child(
            axis_row(
                height,
                ["100%", "50%", "0%"].into_iter().map(str::to_owned),
                summary.days,
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        for x in &midnights {
                            paint_column(*x, bounds, grid, window);
                        }
                        for percent in (0..=100).step_by(10) {
                            paint_row(1.0 - percent as f32 / 100.0, bounds, grid, window);
                        }
                        for (color, segments) in &lines {
                            for segment in segments {
                                paint_curve(segment, bounds, *color, window);
                            }
                        }
                    },
                )
                .size_full()
                .into_any_element(),
            )
            .into_any_element(),
        )
        .into_any_element()
}

fn render_cost(summary: &CostSummary, height: Pixels, cx: &App) -> AnyElement {
    let bands = [
        color_of(SeriesColor::Fable, cx),
        color_of(SeriesColor::Gpt, cx),
        color_of(SeriesColor::Astra, cx),
        color_of(SeriesColor::Opus, cx),
        color_of(SeriesColor::Terra, cx),
    ];
    let columns = summary.columns.clone();
    let max = summary.max;
    let midnights = summary.midnights.clone();
    let grid = grid_color(cx);
    div()
        .flex()
        .flex_col()
        .child(legend_row(&summary.legend, cx))
        .child(
            axis_row(
                height,
                [
                    format!("${:.2}", summary.total),
                    format!("${:.2}", summary.total / 2.0),
                    "$0".to_owned(),
                ]
                .into_iter(),
                summary.days,
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        let stacked = columns
                            .iter()
                            .map(|(x, dollars)| {
                                (*x, dollars.map(|value| 1.0 - (value / max) as f32))
                            })
                            .collect::<Vec<_>>();
                        paint_bands(&stacked, bands, bounds, window);
                        paint_usage_grid(&midnights, bounds, grid, window);
                    },
                )
                .size_full()
                .into_any_element(),
            )
            .into_any_element(),
        )
        .into_any_element()
}

fn render_share(summary: &ShareSummary, height: Pixels, cx: &App) -> AnyElement {
    let bands = [
        color_of(SeriesColor::Fable, cx),
        color_of(SeriesColor::Gpt, cx),
        color_of(SeriesColor::Astra, cx),
        color_of(SeriesColor::Opus, cx),
        color_of(SeriesColor::Terra, cx),
        color_of(SeriesColor::Luna, cx),
    ];
    let legend = [
        (SeriesColor::Fable, "fable", summary.latest[0]),
        (SeriesColor::Opus, "opus", summary.latest[3]),
        (SeriesColor::Gpt, "gpt", summary.latest[1]),
        (SeriesColor::Astra, "astra", summary.latest[2]),
        (SeriesColor::Luna, "luna", summary.latest[5]),
        (SeriesColor::Terra, "terra", summary.latest[4]),
    ]
    .into_iter()
    .map(|(color, model, share)| Legend {
        color,
        label: format!("{model} {:.0}%", share * 100.0),
    })
    .collect::<Vec<_>>();
    let columns = summary
        .columns
        .iter()
        .map(|(x, heights)| (*x, heights.map(|height| 1.0 - height)))
        .collect::<Vec<_>>();
    let midnights = summary.midnights.clone();
    let grid = grid_color(cx);
    div()
        .flex()
        .flex_col()
        .child(legend_row(&legend, cx))
        .child(
            axis_row(
                height,
                ["full", "½", "0%"].into_iter().map(str::to_owned),
                summary.days,
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        paint_bands(&columns, bands, bounds, window);
                        paint_usage_grid(&midnights, bounds, grid, window);
                    },
                )
                .size_full()
                .into_any_element(),
            )
            .into_any_element(),
        )
        .into_any_element()
}

fn render_agent_cost(summary: &AgentCostSummary, height: Pixels, cx: &App) -> AnyElement {
    let curves = [SeriesColor::P50, SeriesColor::P90, SeriesColor::P99]
        .map(|color| color_of(color, cx))
        .into_iter()
        .zip(summary.curves.clone())
        .collect::<Vec<_>>();
    let midnights = summary.midnights.clone();
    let rows = summary.grid.clone();
    let grid = grid_color(cx);
    let mut labels = div().relative().h(height).w(px(44.));
    if let Some(latest) = summary.latest_y {
        for (index, (name, color)) in [
            ("p50", SeriesColor::P50),
            ("p90", SeriesColor::P90),
            ("p99", SeriesColor::P99),
        ]
        .into_iter()
        .enumerate()
        {
            // Against the chart's own height, less the line's own, so a
            // label at the bottom is still beside its curve.
            let top = (latest[index] * (f32::from(height) - 12.0)).max(0.0);
            labels = labels.child(
                div()
                    .absolute()
                    .top(px(top))
                    .text_color(color_of(color, cx))
                    .child(name),
            );
        }
    }
    div()
        .flex()
        .flex_row()
        .items_start()
        .child(
            axis_row(
                height,
                summary.ticks.clone().into_iter(),
                summary.days,
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        for y in &rows {
                            paint_row(*y, bounds, grid, window);
                        }
                        for x in &midnights {
                            paint_column(*x, bounds, grid, window);
                        }
                        for (color, curve) in &curves {
                            paint_curve(curve, bounds, *color, window);
                        }
                    },
                )
                .size_full()
                .into_any_element(),
            )
            .flex_1(),
        )
        .child(labels)
        .into_any_element()
}

/// The legend: one coloured label per series, in the order the summary put
/// them.
fn legend_row(legend: &[Legend], cx: &App) -> gpui::Div {
    div()
        .flex()
        .gap_4()
        .px_2()
        .children(legend.iter().map(|entry| {
            div()
                .text_color(color_of(entry.color, cx))
                .child(entry.label.clone())
        }))
}

/// A chart with its axes: the value labels down the left, the chart itself,
/// and how far back the left edge is under it.
fn axis_row(
    height: Pixels,
    labels: impl Iterator<Item = String>,
    days: u64,
    chart: AnyElement,
) -> gpui::Div {
    div()
        .px_2()
        .pb_1()
        .flex()
        .items_start()
        // Smaller than the buffer's text on purpose: an axis is read past,
        // not read. The face and the colour are the buffer's, inherited.
        .text_size(px(11.))
        .child(
            // No width given: the column is as wide as its widest label,
            // which is the only width that is right for every chart. A
            // number picked for one of them wrapped `100%` onto two lines
            // while `50%` fit.
            div()
                .h(height)
                .flex_none()
                .whitespace_nowrap()
                .pr_2()
                .flex()
                .flex_col()
                .items_end()
                .justify_between()
                .children(labels),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(div().w_full().h(height).child(chart))
                .child(
                    div()
                        .mt_1()
                        .flex()
                        .w_full()
                        .justify_between()
                        .child(format!("−{days}d"))
                        .child("now"),
                ),
        )
}

fn color_of(color: SeriesColor, cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    match color {
        // Anthropic's own orange: the one colour here that is not the
        // terminal palette's, because the model has a colour of its own.
        SeriesColor::Fable => rgb(0xd97757).into(),
        SeriesColor::Gpt => colors.terminal_ansi_cyan.into(),
        SeriesColor::Astra => colors.terminal_ansi_green.into(),
        SeriesColor::Opus => colors.terminal_ansi_magenta.into(),
        SeriesColor::Terra => colors.terminal_ansi_yellow.into(),
        SeriesColor::Luna => colors.terminal_ansi_blue.into(),
        SeriesColor::Auth(index) => quota_auth_color(index),
        SeriesColor::P50 => colors.terminal_ansi_cyan.into(),
        SeriesColor::P90 => colors.terminal_ansi_yellow.into(),
        SeriesColor::P99 => colors.terminal_ansi_red.into(),
    }
}

fn grid_color(cx: &App) -> Hsla {
    cx.theme().colors().text_muted.opacity(0.22).into()
}

/// Stable visual order for the alphabetically sorted auth namespaces shown
/// in the rate-limit graph.
fn quota_auth_color(index: usize) -> Hsla {
    const COLORS: [u32; 6] = [
        0x22d3ee, // cyan
        0x60a5fa, // blue
        0xa78bfa, // violet
        0x34d399, // green
        0xfbbf24, // amber
        0xfb7185, // rose
    ];
    rgb(COLORS[index % COLORS.len()]).into()
}

fn at(bounds: Bounds<Pixels>, (x, y): ChartPoint) -> Point<Pixels> {
    point(
        bounds.origin.x + bounds.size.width * x,
        bounds.origin.y + bounds.size.height * y,
    )
}

/// The stacked bands of a cumulative chart, back to front. Each band is
/// filled between its own line and the one under it.
fn paint_bands<const N: usize>(
    columns: &[(f32, [f32; N])],
    colors: [Hsla; N],
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    for (index, color) in colors.into_iter().enumerate() {
        let mut area = PathBuilder::fill();
        if index == 0 {
            let (Some(first), Some(last)) = (columns.first(), columns.last()) else {
                continue;
            };
            area.move_to(point(at(bounds, (first.0, 0.0)).x, bounds.bottom()));
            for (x, heights) in columns {
                area.line_to(at(bounds, (*x, heights[0])));
            }
            area.line_to(point(at(bounds, (last.0, 0.0)).x, bounds.bottom()));
        } else {
            let Some(first) = columns.first() else {
                continue;
            };
            area.move_to(at(bounds, (first.0, first.1[index - 1])));
            for (x, heights) in columns {
                area.line_to(at(bounds, (*x, heights[index])));
            }
            for (x, heights) in columns.iter().rev() {
                area.line_to(at(bounds, (*x, heights[index - 1])));
            }
        }
        area.close();
        if let Ok(path) = area.build() {
            window.paint_path(path, color.opacity(0.72));
        }
    }
}

fn paint_usage_grid(midnights: &[f32], bounds: Bounds<Pixels>, grid: Hsla, window: &mut Window) {
    for step in 0..=4 {
        paint_row(step as f32 / 4.0, bounds, grid, window);
    }
    for x in midnights {
        paint_column(*x, bounds, grid, window);
    }
}

fn paint_row(y: f32, bounds: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let y = bounds.origin.y + bounds.size.height * y;
    paint_line(
        point(bounds.origin.x, y),
        point(bounds.right(), y),
        color,
        window,
    );
}

fn paint_column(x: f32, bounds: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let x = bounds.origin.x + bounds.size.width * x;
    paint_line(
        point(x, bounds.origin.y),
        point(x, bounds.bottom()),
        color,
        window,
    );
}

fn paint_line(from: Point<Pixels>, to: Point<Pixels>, color: Hsla, window: &mut Window) {
    let mut builder = PathBuilder::stroke(px(1.));
    builder.move_to(from);
    builder.line_to(to);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// One reduced line, smoothed through its own points. The smoothing is
/// shape-preserving (pchip), so a flat stretch stays flat and a fall never
/// overshoots into a rise that did not happen.
fn paint_curve(points: &[ChartPoint], bounds: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let points = points.iter().map(|point| at(bounds, *point)).fold(
        Vec::<Point<Pixels>>::new(),
        |mut points, point| {
            if let Some(previous) = points.last_mut()
                && point.x <= previous.x
            {
                *previous = point;
            } else {
                points.push(point);
            }
            points
        },
    );
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

#[cfg(test)]
mod tests {
    use super::*;

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
