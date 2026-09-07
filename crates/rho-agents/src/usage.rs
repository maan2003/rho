//! What the usage screen draws, reduced once when the series arrives.
//!
//! The daemon sends the raw thing: every quota sample it observed, every
//! hourly cost bucket, every agent's rolling spend. A chart is a few hundred
//! pixels wide, so drawing from that directly means a frame walks ten
//! thousand samples to paint eight hundred columns, every frame, for as long
//! as the screen is open. Everything here runs once per arrival instead, and
//! what comes out is already the size of the picture.
//!
//! Nothing here knows about gpui. A summary is arithmetic on quota and cost —
//! agents' own facts — so it lives beside the rest of what this crate knows
//! about them, and the screen above only paints. The one thing the caller
//! must tell it is `columns`: how many points the chart can show. That is a
//! count, not a pixel, and a chart that grows wider than the count it was
//! reduced to is rebuilt, which is a resize and not a frame.

use std::collections::HashMap;

const HOUR_MS: u64 = 60 * 60 * 1_000;
const DAY_MS: u64 = 24 * HOUR_MS;

/// A point in chart space: `x` runs 0..=1 left to right across the window
/// the chart covers, `y` runs 0..=1 down from its top. The painter turns
/// those into pixels; nothing here knows how wide the screen is.
pub type ChartPoint = (f32, f32);

/// Which colour a line or a band is drawn in, named by what it means. The
/// theme is the screen's business — this says `Opus`, not a hue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeriesColor {
    Fable,
    Gpt,
    Astra,
    Opus,
    Terra,
    Luna,
    /// One ChatGPT auth namespace, by its index in the sorted list of them.
    Auth(usize),
    P50,
    P90,
    P99,
}

/// One entry of a chart's legend: the words and the colour they are in.
#[derive(Clone, Debug)]
pub struct Legend {
    pub color: SeriesColor,
    pub label: String,
}

/// The rate-limit chart: headroom per account over the window.
#[derive(Clone, Debug, Default)]
pub struct QuotaSummary {
    pub days: u64,
    pub legend: Vec<Legend>,
    pub lines: Vec<QuotaLine>,
    /// Midnights in the window, as x.
    pub midnights: Vec<f32>,
}

/// One account's headroom, in the segments a reset cuts it into.
#[derive(Clone, Debug)]
pub struct QuotaLine {
    pub color: SeriesColor,
    pub segments: Vec<Vec<ChartPoint>>,
}

/// The model-cost chart: dollars spent, stacked by model.
#[derive(Clone, Debug, Default)]
pub struct CostSummary {
    pub days: u64,
    pub total: f64,
    pub legend: Vec<Legend>,
    pub requests: u64,
    pub approximate: bool,
    /// Cumulative stacked spend at each column, in dollars: fable, gpt with
    /// luna, astra, opus, then terra. Dollars rather than a ratio
    /// because the axis labels are dollars too.
    pub columns: Vec<(f32, [f64; 5])>,
    /// What the top of the chart is worth.
    pub max: f64,
    pub midnights: Vec<f32>,
}

/// The usage-share chart: who did the work, with the height saying how much
/// work there was.
#[derive(Clone, Debug, Default)]
pub struct ShareSummary {
    pub days: u64,
    pub requests: u64,
    pub approximate: bool,
    /// The latest share of each model, in the order fable, gpt, astra,
    /// opus, terra, luna — which is the order the bands use.
    pub latest: [f64; 6],
    /// Cumulative stacked band heights at each column, 0..=1 of the chart.
    pub columns: Vec<(f32, [f32; 6])>,
    pub midnights: Vec<f32>,
}

/// The agent-cost chart: what a GPT-family agent costs, as three quantiles
/// of the rolling per-agent spend, on a log axis.
#[derive(Clone, Debug, Default)]
pub struct AgentCostSummary {
    pub days: u64,
    /// Axis labels, top to bottom.
    pub ticks: Vec<String>,
    /// p50, p90 and p99 over the window.
    pub curves: [Vec<ChartPoint>; 3],
    /// Where each decade line sits, as y.
    pub grid: Vec<f32>,
    /// Where to write "p50", "p90" and "p99" beside the curves' right end.
    pub latest_y: Option<[f32; 3]>,
    pub midnights: Vec<f32>,
}

/// Headroom per account over the last `days`, as lines a painter can draw
/// straight through.
///
/// A quota series is one sample per poll — thousands over a week — and the
/// reduction to `columns` is what keeps that off the frame.
pub fn quota_summary(
    series: &[rho_ui_proto::QuotaSeries],
    active_auth_namespaces: &[String],
    days: u64,
    now: u64,
    columns: usize,
) -> QuotaSummary {
    let start = now.saturating_sub(days * DAY_MS);
    let span = now.saturating_sub(start).max(1) as f64;
    let x_of = |at: u64| (at.saturating_sub(start) as f64 / span).clamp(0.0, 1.0) as f32;

    let mut auth_names = series
        .iter()
        .filter(|series| series.model == "gpt")
        .filter_map(|series| series.auth_namespace.clone())
        .collect::<Vec<_>>();
    auth_names.sort();
    auth_names.dedup();

    let mut legend = Vec::new();
    for (index, name) in auth_names.iter().enumerate() {
        let latest = series
            .iter()
            .find(|series| {
                series.model == "gpt" && series.auth_namespace.as_deref() == Some(name.as_str())
            })
            .and_then(|series| series.points.last());
        let mut label = if active_auth_namespaces.iter().any(|active| active == name) {
            format!("★ gpt/{name}")
        } else {
            format!("gpt/{name}")
        };
        if let Some(latest) = latest {
            label.push_str(&quota_latest_suffix(latest, now));
        }
        legend.push(Legend {
            color: SeriesColor::Auth(index),
            label,
        });
    }
    for (model, color) in [("opus", SeriesColor::Opus), ("fable", SeriesColor::Fable)] {
        let latest = series
            .iter()
            .filter(|series| series.model == model)
            .filter_map(|series| series.points.last())
            .max_by_key(|point| point.observed_at_ms);
        if let Some(latest) = latest {
            legend.push(Legend {
                color,
                label: format!("{model}{}", quota_latest_suffix(latest, now)),
            });
        } else if series.iter().any(|series| series.model == model) {
            legend.push(Legend {
                color,
                label: model.to_owned(),
            });
        }
    }

    let mut lines = Vec::new();
    for model in series {
        let color = match model.model.as_str() {
            "opus" => SeriesColor::Opus,
            "fable" => SeriesColor::Fable,
            _ => model
                .auth_namespace
                .as_ref()
                .and_then(|name| auth_names.binary_search(name).ok())
                .map_or(SeriesColor::Auth(0), SeriesColor::Auth),
        };
        // A reset is a discontinuity, not a climb: the line stops and a new
        // one starts, which is why a series is segments rather than points.
        let mut segments = Vec::new();
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
            if reset && !segment.is_empty() {
                segments.push(std::mem::take(&mut segment));
            }
            segment.push((
                x_of(sample.observed_at_ms),
                1.0 - f32::from(sample.remaining_percent) / 100.0,
            ));
            previous = Some(sample);
        }
        if !segment.is_empty() {
            segments.push(segment);
        }
        let segments = segments
            .into_iter()
            .map(|segment| reduce(segment, columns))
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if !segments.is_empty() {
            lines.push(QuotaLine { color, segments });
        }
    }

    QuotaSummary {
        days,
        legend,
        lines,
        midnights: midnights(start, now),
    }
}

/// The legend is the one place the reset time lives: the status line shows
/// the percent alone.
fn quota_latest_suffix(latest: &rho_ui_proto::QuotaPoint, now: u64) -> String {
    let mut suffix = format!(" {}%", latest.remaining_percent);
    if let Some(seconds) = latest
        .reset_at_unix
        .map(|reset| reset - now as i64 / 1_000)
        .filter(|seconds| *seconds > 0)
    {
        suffix.push_str(&format!(" · {:.1}d", seconds as f64 / 86_400.0));
    }
    suffix
}

/// Cumulative spend by model over the last `days`.
pub fn cost_summary(
    series: &[rho_ui_proto::AgentUsageSeries],
    days: u64,
    now: u64,
    columns: usize,
) -> CostSummary {
    let window_ms = days * DAY_MS;
    let start = now.saturating_sub(window_ms);
    let x_of = |at: u64| {
        (at.saturating_sub(start) as f64 / window_ms.max(1) as f64).clamp(0.0, 1.0) as f32
    };

    let mut costs = HashMap::<u64, [f64; 5]>::new();
    for model in series {
        let Some(index) = cost_band_index(&model.model) else {
            continue;
        };
        for bucket in &model.buckets {
            if bucket.bucket_start_ms < start {
                continue;
            }
            costs.entry(bucket.bucket_start_ms).or_default()[index] +=
                bucket_cost_usd(bucket, &model.model);
        }
    }
    let max = costs
        .values()
        .map(|costs| costs.iter().sum::<f64>())
        .sum::<f64>()
        .max(f64::EPSILON);

    let mut totals = [0.0; 5];
    let mut columns_out = vec![(x_of(start), [0.0; 5])];
    let mut bucket_start = start.div_ceil(HOUR_MS) * HOUR_MS;
    while bucket_start <= now {
        if let Some(cost) = costs.get(&bucket_start) {
            for (total, cost) in totals.iter_mut().zip(cost) {
                *total += cost;
            }
        }
        let mut cumulative = 0.0;
        let stacked = std::array::from_fn(|index| {
            cumulative += totals[index];
            cumulative
        });
        columns_out.push((x_of(bucket_start.saturating_add(HOUR_MS).min(now)), stacked));
        bucket_start = bucket_start.saturating_add(HOUR_MS);
    }
    // A stacked area is cumulative, so its last column is the total and
    // dropping it would shorten the picture: keep the ends.
    let columns_out = reduce(columns_out, columns);

    let legend = [
        (SeriesColor::Fable, "fable"),
        (SeriesColor::Opus, "opus"),
        (SeriesColor::Gpt, "gpt"),
        (SeriesColor::Astra, "astra"),
        // Luna uses gpt's colour because it is in gpt's band: one provider,
        // one bill.
        (SeriesColor::Gpt, "luna"),
        (SeriesColor::Terra, "terra"),
    ]
    .into_iter()
    .map(|(color, model)| Legend {
        color,
        label: format!("{model} ${:.2}", model_cost(series, model, start)),
    })
    .collect();
    let total = ["fable", "opus", "gpt", "astra", "luna", "terra"]
        .into_iter()
        .map(|model| model_cost(series, model, start))
        .sum();

    CostSummary {
        days,
        total,
        legend,
        requests: requests_since(series, start),
        approximate: approximate_since(series, start),
        columns: columns_out,
        max,
        midnights: midnights(start, now),
    }
}

/// Which band of the cost chart a model is drawn in. Luna shares gpt's: they
/// are one provider's bill. Astra remains separately visible.
fn cost_band_index(model: &str) -> Option<usize> {
    match model {
        "fable" => Some(0),
        "gpt" | "luna" => Some(1),
        "astra" => Some(2),
        "opus" => Some(3),
        "terra" => Some(4),
        _ => None,
    }
}

/// Smoothed model shares over the last `days`, with the band height saying
/// how much work there was.
pub fn share_summary(
    series: &[rho_ui_proto::AgentUsageSeries],
    days: u64,
    now: u64,
    columns: usize,
) -> ShareSummary {
    let window_ms = days * DAY_MS;
    let start = now.saturating_sub(window_ms);
    let shares = usage_share_points(series, now, days);
    let scale = usage_share_scale(&shares);
    let latest = shares
        .last()
        .map(|(_, share, _)| *share)
        .unwrap_or_default();
    let stacked = shares
        .iter()
        .map(|(at, shares, total_usage)| {
            let mut height = 0.0;
            let bands = std::array::from_fn(|index| {
                height += shares[index] * (total_usage / scale).min(1.0);
                height.clamp(0.0, 1.0) as f32
            });
            (
                (at.saturating_sub(start) as f64 / window_ms.max(1) as f64).clamp(0.0, 1.0) as f32,
                bands,
            )
        })
        .collect::<Vec<_>>();

    ShareSummary {
        days,
        requests: requests_since(series, start),
        approximate: approximate_since(series, start),
        latest,
        columns: reduce(stacked, columns),
        midnights: midnights(start, now),
    }
}

/// The three quantiles of per-agent GPT-family spend over the last `days`.
pub fn agent_cost_summary(
    hosts: &[Vec<rho_ui_proto::AgentCostSeries>],
    days: u64,
    now: u64,
    columns: usize,
) -> AgentCostSummary {
    let window_ms = days * DAY_MS;
    let start = now.saturating_sub(window_ms);
    let points = agent_cost_percentile_points(hosts, now, days);
    let scale = agent_cost_scale(&points);
    let x_of = |at: u64| {
        (at.saturating_sub(start) as f64 / window_ms.max(1) as f64).clamp(0.0, 1.0) as f32
    };

    let curves = std::array::from_fn(|index| {
        reduce(
            points
                .iter()
                .map(|(at, values)| (x_of(*at), agent_cost_y_ratio(values[index], scale)))
                .collect(),
            columns,
        )
    });
    let latest_y = points
        .last()
        .map(|(_, latest)| std::array::from_fn(|index| agent_cost_y_ratio(latest[index], scale)));

    AgentCostSummary {
        days,
        ticks: (scale.min_power..=scale.max_power)
            .rev()
            .map(format_agent_cost_tick)
            .collect(),
        curves,
        grid: (scale.min_power..=scale.max_power)
            .map(|power| agent_cost_y_ratio(10.0_f64.powi(power), scale))
            .collect(),
        latest_y,
        midnights: midnights(start, now),
    }
}

/// Midnights inside the window, as x. The chart's only date marks, and
/// there are at most `days` of them.
fn midnights(start: u64, now: u64) -> Vec<f32> {
    let span = now.saturating_sub(start).max(1) as f64;
    let mut midnight = start.div_ceil(DAY_MS) * DAY_MS;
    if midnight == start {
        midnight = midnight.saturating_add(DAY_MS);
    }
    let mut marks = Vec::new();
    while midnight < now {
        marks.push((midnight.saturating_sub(start) as f64 / span) as f32);
        midnight = midnight.saturating_add(DAY_MS);
    }
    marks
}

/// Keeps at most one point per column: the last sample falling in each. A
/// line at one point per column is the same line, and what it drops is the
/// thousands of samples that would have shared a pixel.
///
/// The first sample is kept whatever column it lands in. Taking only the
/// last of each column would move the left end of the line to wherever the
/// first column happened to end, which is a picture of a different week.
fn reduce<T: Copy>(points: Vec<(f32, T)>, columns: usize) -> Vec<(f32, T)> {
    let columns = columns.max(1);
    if points.len() <= columns {
        return points;
    }
    let mut points = points.into_iter();
    let Some(first) = points.next() else {
        return Vec::new();
    };
    let mut reduced: Vec<(f32, T)> = Vec::with_capacity(columns + 1);
    reduced.push(first);
    // No column matches, so the next point is pushed rather than replacing
    // the first one.
    let mut current = usize::MAX;
    for point in points {
        let column = ((point.0.clamp(0.0, 1.0) * columns as f32) as usize).min(columns - 1);
        if column == current {
            *reduced.last_mut().expect("a column was already written") = point;
        } else {
            reduced.push(point);
            current = column;
        }
    }
    reduced
}

fn requests_since(series: &[rho_ui_proto::AgentUsageSeries], since: u64) -> u64 {
    series
        .iter()
        .flat_map(|series| &series.buckets)
        .filter(|bucket| bucket.bucket_start_ms >= since)
        .map(|bucket| bucket.requests)
        .sum()
}

fn approximate_since(series: &[rho_ui_proto::AgentUsageSeries], since: u64) -> bool {
    series
        .iter()
        .flat_map(|series| &series.buckets)
        .filter(|bucket| bucket.bucket_start_ms >= since)
        .any(|bucket| bucket.approximate)
}

#[derive(Clone, Copy)]
struct AgentCostScale {
    min_power: i32,
    max_power: i32,
}

/// Hourly P50/P90/P99 of trailing-seven-day GPT-family cost per agent. Each
/// hourly cross-section becomes a log-cost histogram; EMA is applied to its
/// mass before extracting quantiles so busier hours carry proportionally more
/// evidence without averaging per-host percentiles.
fn agent_cost_percentile_points(
    hosts: &[Vec<rho_ui_proto::AgentCostSeries>],
    now: u64,
    days: u64,
) -> Vec<(u64, [f64; 3])> {
    const COST_WINDOW_HOURS: u64 = rho_ui_proto::AGENT_COST_WINDOW_DAYS * 24;
    const HISTOGRAM_BINS: usize = 256;
    const MIN_LOG_COST: f64 = -3.0;
    const MAX_LOG_COST: f64 = 5.0;

    let visible_start = now.saturating_sub(days * DAY_MS);
    let end_bucket = (now / HOUR_MS * HOUR_MS).saturating_sub(HOUR_MS);
    let half_life_hours = if days <= 7 { 12.0 } else { 48.0 };
    let decay = 0.5_f64.powf(1.0 / half_life_hours);
    let mut hourly = HashMap::<u64, Vec<((usize, rho_ui_proto::AgentId), f64)>>::new();
    let mut first_bucket = end_bucket;

    for (host_index, series_set) in hosts.iter().enumerate() {
        for series in series_set {
            if !matches!(
                series.model.as_str(),
                "gpt" | "astra" | "terra" | "luna" | "unknown"
            ) {
                continue;
            }
            for bucket in &series.buckets {
                let bucket_start = bucket.bucket_start_ms / HOUR_MS * HOUR_MS;
                first_bucket = first_bucket.min(bucket_start);
                hourly.entry(bucket_start).or_default().push((
                    (host_index, series.agent_id),
                    bucket_cost_usd(bucket, &series.model),
                ));
            }
        }
    }

    let mut rolling = HashMap::<(usize, rho_ui_proto::AgentId), f64>::new();
    let mut smoothed_histogram = [0.0; HISTOGRAM_BINS];
    let mut points = Vec::new();
    let mut bucket_start = first_bucket;
    while bucket_start <= end_bucket {
        let expired_at = bucket_start.saturating_sub(COST_WINDOW_HOURS * HOUR_MS);
        if let Some(expired) = hourly.get(&expired_at) {
            for (agent, cost) in expired {
                if let Some(total) = rolling.get_mut(agent) {
                    *total = (*total - cost).max(0.0);
                }
            }
            rolling.retain(|_, total| *total > f64::EPSILON);
        }
        if let Some(current) = hourly.get(&bucket_start) {
            for (agent, cost) in current {
                *rolling.entry(*agent).or_default() += cost;
            }
        }

        let mut histogram = [0.0; HISTOGRAM_BINS];
        for cost in rolling.values().copied().filter(|cost| *cost > 0.0) {
            let ratio = ((cost.log10() - MIN_LOG_COST) / (MAX_LOG_COST - MIN_LOG_COST))
                .clamp(0.0, 1.0 - f64::EPSILON);
            histogram[(ratio * HISTOGRAM_BINS as f64) as usize] += 1.0;
        }
        for (smoothed, current) in smoothed_histogram.iter_mut().zip(histogram) {
            *smoothed = *smoothed * decay + current * (1.0 - decay);
        }
        let total = smoothed_histogram.iter().sum::<f64>();
        if total >= 0.01 && bucket_start >= visible_start {
            let percentiles = [0.5, 0.9, 0.99].map(|percentile| {
                histogram_percentile(&smoothed_histogram, percentile, MIN_LOG_COST, MAX_LOG_COST)
            });
            points.push((bucket_start.saturating_add(HOUR_MS).min(now), percentiles));
        }
        bucket_start = bucket_start.saturating_add(HOUR_MS);
    }
    points
}

fn histogram_percentile(histogram: &[f64], percentile: f64, min_log: f64, max_log: f64) -> f64 {
    let target = histogram.iter().sum::<f64>() * percentile;
    let mut cumulative = 0.0;
    for (index, weight) in histogram.iter().copied().enumerate() {
        let previous = cumulative;
        cumulative += weight;
        if cumulative >= target && weight > 0.0 {
            let within = ((target - previous) / weight).clamp(0.0, 1.0);
            let width = (max_log - min_log) / histogram.len() as f64;
            return 10.0_f64.powf(min_log + (index as f64 + within) * width);
        }
    }
    10.0_f64.powf(max_log)
}

fn agent_cost_scale(points: &[(u64, [f64; 3])]) -> AgentCostScale {
    let min = points
        .iter()
        .map(|(_, values)| values[0])
        .filter(|value| *value > 0.0)
        .min_by(f64::total_cmp)
        .unwrap_or(0.1);
    let max = points
        .iter()
        .map(|(_, values)| values[2])
        .max_by(f64::total_cmp)
        .unwrap_or(10.0);
    let min_power = min.log10().floor() as i32;
    let mut max_power = max.log10().ceil() as i32;
    if max_power <= min_power {
        max_power = min_power + 1;
    }
    AgentCostScale {
        min_power,
        max_power,
    }
}

fn agent_cost_y_ratio(value: f64, scale: AgentCostScale) -> f32 {
    let span = f64::from(scale.max_power - scale.min_power).max(1.0);
    (1.0 - (value.max(10.0_f64.powi(scale.min_power)).log10() - f64::from(scale.min_power)) / span)
        .clamp(0.0, 1.0) as f32
}

fn format_agent_cost_tick(power: i32) -> String {
    match power {
        3 => "$1k".to_owned(),
        4 => "$10k".to_owned(),
        5 => "$100k".to_owned(),
        power if power >= 0 => format!("${}", 10_u64.pow(power as u32)),
        -1 => "$0.10".to_owned(),
        -2 => "$0.01".to_owned(),
        _ => "$0.001".to_owned(),
    }
}

/// Returns hourly exponentially-smoothed model shares. Usage is smoothed
/// before division, so a low-volume hour has proportionally little influence.
fn usage_share_points(
    series: &[rho_ui_proto::AgentUsageSeries],
    now: u64,
    days: u64,
) -> Vec<(u64, [f64; 6], f64)> {
    let start = now.saturating_sub(days * DAY_MS);
    let start_bucket = start / HOUR_MS * HOUR_MS;
    let end_bucket = now / HOUR_MS * HOUR_MS;
    let half_life_hours = if days <= 7 { 12.0 } else { 48.0 };
    let decay = 0.5_f64.powf(1.0 / half_life_hours);
    let mut usage = HashMap::<u64, [f64; 6]>::new();
    let mut first_bucket = start_bucket;

    for model in series {
        let Some(index) = usage_model_index(&model.model) else {
            continue;
        };
        for bucket in &model.buckets {
            let bucket_start = bucket.bucket_start_ms / HOUR_MS * HOUR_MS;
            first_bucket = first_bucket.min(bucket_start);
            usage.entry(bucket_start).or_default()[index] += bucket_usage_units(bucket);
        }
    }

    let mut smoothed = [0.0; 6];
    let mut bucket_start = first_bucket;
    while bucket_start < start_bucket {
        for value in &mut smoothed {
            *value *= decay;
        }
        if let Some(values) = usage.get(&bucket_start) {
            for (smoothed, value) in smoothed.iter_mut().zip(values) {
                *smoothed += value;
            }
        }
        bucket_start = bucket_start.saturating_add(HOUR_MS);
    }

    let mut points = Vec::new();
    while bucket_start <= end_bucket {
        for value in &mut smoothed {
            *value *= decay;
        }
        if let Some(values) = usage.get(&bucket_start) {
            for (smoothed, value) in smoothed.iter_mut().zip(values) {
                *smoothed += value;
            }
        }
        let total = smoothed.iter().sum::<f64>();
        let shares = if total > 0.0 {
            smoothed.map(|value| value / total)
        } else {
            [0.0; 6]
        };
        let at = bucket_start.saturating_add(HOUR_MS).min(now);
        if let Some((last_at, last_share, last_total)) = points.last_mut()
            && *last_at == at
        {
            *last_share = shares;
            *last_total = total;
        } else {
            points.push((at, shares, total));
        }
        bucket_start = bucket_start.saturating_add(HOUR_MS);
    }
    points
}

fn usage_share_scale(points: &[(u64, [f64; 6], f64)]) -> f64 {
    let mut totals = points
        .iter()
        .map(|(_, _, total)| *total)
        .filter(|total| *total > 0.0)
        .collect::<Vec<_>>();
    if totals.is_empty() {
        return 1.0;
    }
    totals.sort_by(f64::total_cmp);
    totals[((totals.len() - 1) * 95) / 100]
}

fn usage_model_index(model: &str) -> Option<usize> {
    match model {
        "fable" => Some(0),
        "gpt" => Some(1),
        "astra" => Some(2),
        "opus" => Some(3),
        "terra" => Some(4),
        "luna" => Some(5),
        _ => None,
    }
}

fn bucket_usage_units(bucket: &rho_ui_proto::AgentUsageBucket) -> f64 {
    10.0 * bucket.input_tokens as f64
        + bucket.cache_read_tokens as f64
        + 30.0 * bucket.output_tokens as f64
}

fn model_cost(series: &[rho_ui_proto::AgentUsageSeries], model: &str, since: u64) -> f64 {
    series
        .iter()
        .filter(|series| series.model == model)
        .flat_map(|series| &series.buckets)
        .filter(|bucket| bucket.bucket_start_ms >= since)
        .map(|bucket| bucket_cost_usd(bucket, model))
        .sum()
}

/// What a bucket cost, at the provider's posted rates per million tokens.
pub fn bucket_cost_usd(bucket: &rho_ui_proto::AgentUsageBucket, model: &str) -> f64 {
    let (input, cache_read, cache_write_5m, cache_write_1h, output) = match model {
        "fable" => (10.0, 1.0, 12.5, 20.0, 50.0),
        "opus" => (5.0, 0.5, 6.25, 10.0, 25.0),
        "astra" => (10.0, 1.0, 12.5, 12.5, 50.0),
        "terra" => (2.5, 0.25, 3.125, 3.125, 15.0),
        "luna" => (1.0, 0.1, 1.25, 1.25, 6.0),
        _ => (5.0, 0.5, 6.25, 6.25, 30.0),
    };
    let cache_write_5m_tokens = bucket
        .cache_write_tokens
        .saturating_sub(bucket.cache_write_1h_tokens);
    (bucket.input_tokens as f64 * input
        + bucket.cache_read_tokens as f64 * cache_read
        + cache_write_5m_tokens as f64 * cache_write_5m
        + bucket.cache_write_1h_tokens as f64 * cache_write_1h
        + bucket.output_tokens as f64 * output)
        / 1_000_000.0
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
            cache_write_1h_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        };
        assert_eq!(bucket_cost_usd(&usage, "fable"), 81.0);
        assert_eq!(bucket_cost_usd(&usage, "opus"), 40.5);
        assert_eq!(bucket_cost_usd(&usage, "gpt"), 41.75);
        assert_eq!(bucket_cost_usd(&usage, "astra"), 73.5);
        assert_eq!(bucket_cost_usd(&usage, "terra"), 20.875);
        assert_eq!(bucket_cost_usd(&usage, "luna"), 8.35);
    }

    #[test]
    fn astra_usage_has_its_own_chart_band() {
        let bucket = rho_ui_proto::AgentUsageBucket {
            bucket_start_ms: 10 * HOUR_MS,
            input_tokens: 1_000_000,
            requests: 1,
            ..Default::default()
        };
        let now = 40 * HOUR_MS + HOUR_MS / 2;
        let usage = vec![rho_ui_proto::AgentUsageSeries {
            model: "astra".to_owned(),
            buckets: vec![bucket.clone()],
        }];

        let cost = cost_summary(&usage, 7, now, 100);
        assert_eq!(cost.total, 10.0);
        assert_eq!(cost.columns.last().unwrap().1[1], 0.0);
        assert_eq!(cost.columns.last().unwrap().1[2], 10.0);
        assert!(
            usage_share_points(&usage, now, 7)
                .iter()
                .any(|(_, shares, _)| shares[2] > 0.0)
        );

        let agent_id =
            rho_ui_proto::AgentId::from_counter(1, &rho_ui_proto::AgentIdDomain(0)).unwrap();
        let points = agent_cost_percentile_points(
            &[vec![rho_ui_proto::AgentCostSeries {
                agent_id,
                model: "astra".to_owned(),
                buckets: vec![bucket],
            }]],
            now,
            7,
        );
        assert!(points.last().unwrap().1[0] > 0.0);
    }

    #[test]
    fn agent_cost_percentiles_keep_same_counter_agents_separate_across_hosts() {
        let agent_id =
            rho_ui_proto::AgentId::from_counter(1, &rho_ui_proto::AgentIdDomain(0)).unwrap();
        let series = |output_tokens| {
            vec![rho_ui_proto::AgentCostSeries {
                agent_id,
                model: "gpt".to_owned(),
                buckets: vec![rho_ui_proto::AgentUsageBucket {
                    bucket_start_ms: 10 * HOUR_MS,
                    output_tokens,
                    requests: 1,
                    ..Default::default()
                }],
            }]
        };
        let now = 40 * HOUR_MS + HOUR_MS / 2;
        let points = agent_cost_percentile_points(&[series(1_000_000), series(10_000_000)], now, 7);
        let latest = points.last().unwrap().1;
        assert!(
            latest[0] < 100.0,
            "p50 merged colliding host ids: {latest:?}"
        );
        assert!(latest[2] > 250.0, "p99 lost the second host: {latest:?}");
        assert!(latest[0] <= latest[1] && latest[1] <= latest[2]);
    }

    #[test]
    fn agent_cost_percentiles_ignore_the_current_partial_hour() {
        let agent_id =
            rho_ui_proto::AgentId::from_counter(1, &rho_ui_proto::AgentIdDomain(0)).unwrap();
        let now = 40 * HOUR_MS + HOUR_MS / 2;
        let points = agent_cost_percentile_points(
            &[vec![rho_ui_proto::AgentCostSeries {
                agent_id,
                model: "gpt".to_owned(),
                buckets: vec![
                    rho_ui_proto::AgentUsageBucket {
                        bucket_start_ms: 10 * HOUR_MS,
                        output_tokens: 1_000_000,
                        requests: 1,
                        ..Default::default()
                    },
                    rho_ui_proto::AgentUsageBucket {
                        bucket_start_ms: 40 * HOUR_MS,
                        output_tokens: 10_000_000,
                        requests: 1,
                        ..Default::default()
                    },
                ],
            }]],
            now,
            7,
        );
        assert!(points.last().unwrap().1[2] < 100.0);
    }

    #[test]
    fn usage_share_is_weighted_before_smoothing_and_stays_stable_when_idle() {
        let series = vec![
            rho_ui_proto::AgentUsageSeries {
                model: "gpt".to_owned(),
                buckets: vec![rho_ui_proto::AgentUsageBucket {
                    bucket_start_ms: 0,
                    input_tokens: 10,
                    ..Default::default()
                }],
            },
            rho_ui_proto::AgentUsageSeries {
                model: "fable".to_owned(),
                buckets: vec![rho_ui_proto::AgentUsageBucket {
                    bucket_start_ms: 0,
                    output_tokens: 10,
                    ..Default::default()
                }],
            },
        ];

        let shares = usage_share_points(&series, 2 * HOUR_MS, 7);
        let first = shares[0].1;
        let idle = shares[1].1;
        assert!((first[0] - 0.75).abs() < f64::EPSILON);
        assert!((first[1] - 0.25).abs() < f64::EPSILON);
        assert_eq!(first, idle, "an idle hour must not change the share");
    }

    #[test]
    fn seven_day_share_reacts_faster_than_thirty_day_share() {
        let series = vec![
            rho_ui_proto::AgentUsageSeries {
                model: "gpt".to_owned(),
                buckets: vec![rho_ui_proto::AgentUsageBucket {
                    bucket_start_ms: 0,
                    input_tokens: 10,
                    ..Default::default()
                }],
            },
            rho_ui_proto::AgentUsageSeries {
                model: "fable".to_owned(),
                buckets: vec![rho_ui_proto::AgentUsageBucket {
                    bucket_start_ms: HOUR_MS,
                    input_tokens: 10,
                    ..Default::default()
                }],
            },
        ];

        let seven_day = usage_share_points(&series, 2 * HOUR_MS, 7)
            .last()
            .unwrap()
            .1[0];
        let thirty_day = usage_share_points(&series, 2 * HOUR_MS, 30)
            .last()
            .unwrap()
            .1[0];
        assert!(seven_day > thirty_day);
    }

    #[test]
    fn usage_share_height_uses_the_periods_p95_activity() {
        let points = (1..=20)
            .map(|total| (total, [0.0; 6], total as f64))
            .collect::<Vec<_>>();
        assert_eq!(usage_share_scale(&points), 19.0);
    }

    /// The whole point of a summary: a week of minute-by-minute quota polls
    /// is thousands of samples, and what a frame gets handed is the width of
    /// the chart.
    #[test]
    fn a_week_of_quota_samples_is_reduced_to_the_chart_width() {
        const MINUTE_MS: u64 = 60 * 1_000;
        let now = 30 * DAY_MS;
        let samples = (0..7 * 24 * 60)
            .map(|minute| rho_ui_proto::QuotaPoint {
                observed_at_ms: now - 7 * DAY_MS + minute * MINUTE_MS,
                // Falling headroom: no resets, so this is one segment and
                // the reduction has nowhere to hide.
                remaining_percent: 100 - (minute / 200) as u8,
                reset_at_unix: Some(1),
            })
            .collect::<Vec<_>>();
        let series = vec![rho_ui_proto::QuotaSeries {
            model: "opus".to_owned(),
            auth_namespace: None,
            points: samples,
        }];

        let summary = quota_summary(&series, &[], 7, now, 832);
        let drawn = summary
            .lines
            .iter()
            .flat_map(|line| &line.segments)
            .map(Vec::len)
            .sum::<usize>();
        assert_eq!(summary.lines.len(), 1);
        // The width, and the first sample, which is kept so the line still
        // starts where the week did.
        assert!(
            drawn <= 833,
            "a frame would still walk {drawn} points of 10080"
        );
        assert!(drawn > 800, "the line lost its shape: {drawn} points");
    }

    /// A reduced line still starts where it started and ends where it
    /// ended: the picture is narrower in samples, not in time.
    #[test]
    fn reduction_keeps_both_ends_of_a_line() {
        let points = (0..1_000)
            .map(|index| (index as f32 / 999.0, index as f32 / 999.0))
            .collect::<Vec<_>>();
        let reduced = reduce(points, 100);
        assert_eq!(reduced.first().unwrap().0, 0.0);
        assert_eq!(reduced.last().unwrap().0, 1.0);
        assert!(reduced.len() <= 101);
    }
}
