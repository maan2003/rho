//! The painting: a summary already the size of the picture, drawn as a
//! block under the surface's header. Nothing here does arithmetic per
//! sample; a frame walks the reduced points the summary holds.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, Hsla, PathBuilder, Pixels, Point, TextAlign, TextStyle, Window,
    canvas, div, fill, point, px, rgb, size,
};
use rho_agents_client::usage::{
    AgentCostSummary, ChartPoint, CostSummary, DayMark, Legend, MODEL_BANDS, QuotaSummary,
    SeriesColor, ShareSummary, dollars,
};
use theme::ActiveTheme as _;

/// The least a chart is drawn at. Below this the lines are on top of each
/// other and the picture says nothing, so a short window scrolls instead.
const MIN_CHART_HEIGHT: Pixels = px(220.);

/// The band reserved under the picture for day labels.
const FOOTER_HEIGHT: Pixels = px(22.);

/// What the block paints: already the size of the picture.
pub(super) enum Summary {
    Quota(QuotaSummary),
    Cost(CostSummary),
    Share(ShareSummary),
    AgentCost(AgentCostSummary),
}

impl Summary {
    /// What each colour in the picture stands for. It is the buffer's
    /// text, not the picture's, so the point can rest on it and a search
    /// finds an account by name.
    pub(super) fn legend(&self) -> &[Legend] {
        match self {
            Summary::Quota(quota) => &quota.legend,
            Summary::Cost(cost) => &cost.legend,
            Summary::Share(share) => &share.legend,
            Summary::AgentCost(cost) => &cost.legend,
        }
    }

    pub(super) fn has_picture(&self) -> bool {
        match self {
            Self::Quota(quota) => !quota.lines.is_empty(),
            Self::Cost(cost) => cost.total > 0.0,
            Self::Share(share) => !share.legend.is_empty(),
            Self::AgentCost(cost) => !cost.curves[0].is_empty(),
        }
    }
}

/// Height including the footer, rounded down to whole editor lines: the
/// editor gives a block an integral number of lines of space.
pub(super) fn height_for(available: Pixels, line_height: Pixels, header_rows: u32) -> Pixels {
    (line_height * ((available / line_height).floor() - header_rows as f32)).max(MIN_CHART_HEIGHT)
}

pub(super) fn backfill_note(approximate: bool) -> &'static str {
    if approximate {
        " · includes estimates for older requests"
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
/// the next label as well as these — a label that wants its series' colour
/// still says so and wins, because a child overrides its parent.
fn render_chart(summary: &Summary, height: Pixels, text_style: &TextStyle, cx: &App) -> AnyElement {
    let height = height - FOOTER_HEIGHT;
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
        .child(
            axis_row(
                height,
                ["100%", "50%", "0%"].into_iter().map(str::to_owned),
                &summary.midnights,
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        for mark in &midnights {
                            paint_column(mark.x, bounds, grid, window);
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
    let bands = MODEL_BANDS.map(|(_, color)| color_of(color, cx));
    let columns = summary
        .columns
        .iter()
        .map(|(x, dollars)| {
            (
                *x,
                dollars.map(|value| 1.0 - (value / summary.total) as f32),
            )
        })
        .collect::<Vec<_>>();
    let midnights = summary.midnights.clone();
    let grid = grid_color(cx);
    div()
        .flex()
        .flex_col()
        .child(
            axis_row(
                height,
                [
                    dollars(summary.total),
                    dollars(summary.total / 2.0),
                    "$0".to_owned(),
                ]
                .into_iter(),
                &summary.midnights,
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

fn render_share(summary: &ShareSummary, height: Pixels, cx: &App) -> AnyElement {
    let bands = MODEL_BANDS.map(|(_, color)| color_of(color, cx));
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
        .child(
            axis_row(
                height,
                ["more", "", "less"].into_iter().map(str::to_owned),
                &summary.midnights,
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
    let curves = summary
        .legend
        .iter()
        .map(|entry| color_of(entry.color, cx))
        .zip(summary.curves.clone())
        .collect::<Vec<_>>();
    let midnights = summary.midnights.clone();
    let rows = summary.grid.clone();
    let grid = grid_color(cx);
    axis_row(
        height,
        summary.ticks.clone().into_iter(),
        &summary.midnights,
        canvas(
            move |_, _, _| {},
            move |bounds, _, window, _| {
                for y in &rows {
                    paint_row(*y, bounds, grid, window);
                }
                for mark in &midnights {
                    paint_column(mark.x, bounds, grid, window);
                }
                // The three costs can coincide (e.g. one agent). Give
                // typical cost the foreground in that case.
                for (color, curve) in curves.iter().rev() {
                    paint_curve(curve, bounds, *color, window);
                }
            },
        )
        .size_full()
        .into_any_element(),
    )
    .into_any_element()
}

/// A chart with its axes: the value labels down the left, the chart itself,
/// and the named days under it.
fn axis_row(
    height: Pixels,
    labels: impl Iterator<Item = String>,
    midnights: &[DayMark],
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
                .child(day_axis(midnights)),
        )
}

fn day_axis(midnights: &[DayMark]) -> impl IntoElement {
    // Seven weekday names fit across the phone. Over a month, label
    // every fifth midnight and leave the intervening grid lines intact.
    let month = midnights.len() > 7;
    let stride = if month { 5 } else { 1 };
    let labels = midnights
        .iter()
        .step_by(stride)
        .filter_map(|mark| {
            let date = DateTime::<Utc>::from_timestamp_millis(mark.at_ms as i64)?;
            Some((
                mark.x,
                date.format(if month { "%b %-d" } else { "%a" }).to_string(),
            ))
        })
        .collect::<Vec<_>>();
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            let style = window.text_style();
            let font_size = style.font_size.to_pixels(window.rem_size());
            let mut after_previous = bounds.left();
            for (x, label) in labels {
                let run = style.to_run(label.len());
                let line = window
                    .text_system()
                    .shape_line(label.into(), font_size, &[run], None);
                if line.width() > bounds.size.width {
                    continue;
                }
                let x = (bounds.left() + bounds.size.width * x - line.width() / 2.)
                    .clamp(bounds.left(), bounds.right() - line.width());
                if x >= after_previous {
                    let _ = line.paint(
                        point(x, bounds.top()),
                        bounds.size.height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    after_previous = x + line.width() + px(4.);
                }
            }
        },
    )
    .mt_1()
    .w_full()
    .h(FOOTER_HEIGHT - px(8.))
}

pub(super) fn color_of(color: SeriesColor, cx: &App) -> Hsla {
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

fn paint_usage_grid(
    midnights: &[DayMark],
    bounds: Bounds<Pixels>,
    grid: Hsla,
    window: &mut Window,
) {
    for step in 0..=4 {
        paint_row(step as f32 / 4.0, bounds, grid, window);
    }
    for mark in midnights {
        paint_column(mark.x, bounds, grid, window);
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
                && point.x.floor() <= previous.x.floor()
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
    if points.len() == 1 {
        let r = px(2.)
            .min(bounds.size.width / 2.)
            .min(bounds.size.height / 2.);
        let origin = point(
            (first.x - r).clamp(bounds.left(), bounds.right() - r * 2.),
            (first.y - r).clamp(bounds.top(), bounds.bottom() - r * 2.),
        );
        window.paint_quad(fill(
            Bounds {
                origin,
                size: size(r * 2., r * 2.),
            },
            color,
        ));
        return;
    }
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
