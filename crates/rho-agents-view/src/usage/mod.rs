//! What the agents have spent, as charts.
//!
//! Eight charts, each its own `usage:` address (`SPEC-usage-addresses`):
//! four pictures over the two windows worth looking at. The chart screen
//! switches window and picture in place ([`UsageSurface`]): the frame puts
//! the other chart where this one stood among the recent places
//! ([`rho_frame::Frame::replace`]), so switching adds no place to `tab`
//! back through, and what is restored after a restart is the chart in
//! view. The agents system
//! asks every attached host for the series a chart needs when it opens,
//! keeps each host's answer apart in [`Usage`], and merges them on the way
//! out; a [`UsageSurface`] observes the system and paints the merge.
//!
//! Every number painted comes from `rho_agents_client::usage`, which
//! reduces a series to the width of the chart once, when the series
//! arrives or the window resizes; a frame walks the reduced points and
//! nothing else.

mod paint;
mod surface;

use std::collections::{BTreeMap, HashMap};

use rho_agents_client::HostId;
use rho_agents_client::protocol::{AgentCostSeries, AgentUsageSeries};
use rho_frame::{Addressed, Spelling};
pub use surface::UsageSurface;

/// `open usage`: the first chart.
pub static OPEN_USAGE: Spelling = Spelling {
    name: "open usage",
    aliases: &[
        ("usage", 9),
        ("rate limit", 8),
        ("quota", 8),
        ("allowance", 7),
        ("spend", 6),
        ("cost", 6),
        ("chart", 5),
    ],
    argument: None,
};

const DAY_MS: u64 = 24 * 60 * 60 * 1_000;

/// Which picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Chart {
    RateLimit,
    ModelCost,
    UsageShare,
    AgentCost,
}

impl Chart {
    pub const ALL: [Self; 4] = [
        Self::RateLimit,
        Self::ModelCost,
        Self::UsageShare,
        Self::AgentCost,
    ];

    fn slug(self) -> &'static str {
        match self {
            Self::RateLimit => "rate-limit",
            Self::ModelCost => "model-cost",
            Self::UsageShare => "usage-share",
            Self::AgentCost => "agent-cost",
        }
    }

    /// The picture after this one, or before it, wrapping.
    pub fn step(self, forward: bool) -> Self {
        let at = Self::ALL.iter().position(|chart| *chart == self).unwrap();
        let len = Self::ALL.len();
        Self::ALL[if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        }]
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::RateLimit => "rate limit",
            Self::ModelCost => "model cost",
            Self::UsageShare => "model usage share",
            Self::AgentCost => "GPT agent cost",
        }
    }
}

/// How far back a chart looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Days {
    Seven,
    Thirty,
}

impl Days {
    pub fn count(self) -> u64 {
        match self {
            Self::Seven => 7,
            Self::Thirty => 30,
        }
    }

    pub fn other(self) -> Self {
        match self {
            Self::Seven => Self::Thirty,
            Self::Thirty => Self::Seven,
        }
    }

    /// How much before the window a smoothed chart asks for, so its left
    /// edge carries the usage before it rather than starting from nothing:
    /// seven half-lives of the smoothing.
    fn warmup(self) -> u64 {
        match self {
            Self::Seven => 4,
            Self::Thirty => 14,
        }
    }
}

/// One usage chart: `usage:<chart>/<days>` (`SPEC-usage-addresses`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UsageChart {
    pub chart: Chart,
    pub days: Days,
}

impl Addressed for UsageChart {
    const KIND: &'static str = "usage";

    fn rest(&self) -> String {
        format!("{}/{}", self.chart.slug(), self.days.count())
    }

    fn from_rest(rest: &str) -> Option<Self> {
        let (chart, days) = rest.split_once('/')?;
        let chart = Chart::ALL.into_iter().find(|known| known.slug() == chart)?;
        let days = match days {
            "7" => Days::Seven,
            "30" => Days::Thirty,
            _ => return None,
        };
        Some(Self { chart, days })
    }
}

impl UsageChart {
    /// What `open usage` opens, and what `usage` finds first in `Places`.
    pub const FIRST: Self = Self {
        chart: Chart::RateLimit,
        days: Days::Seven,
    };

    /// Every chart, in the order `Places` lists them.
    pub fn all() -> impl Iterator<Item = Self> {
        Chart::ALL.into_iter().flat_map(|chart| {
            [Days::Seven, Days::Thirty]
                .into_iter()
                .map(move |days| Self { chart, days })
        })
    }

    /// The same picture over the other window.
    pub fn other_window(self) -> Self {
        Self {
            days: self.days.other(),
            ..self
        }
    }

    /// The next picture over the same window, or the one before.
    pub fn step(self, forward: bool) -> Self {
        Self {
            chart: self.chart.step(forward),
            ..self
        }
    }

    /// `rate limit · 7d`.
    pub fn title(self) -> String {
        format!("{} · {}d", self.chart.title(), self.days.count())
    }

    /// What every host is asked for to draw it, now.
    pub(crate) fn request(self, now_ms: u64) -> Request {
        let back = |days: u64| now_ms.saturating_sub(days * DAY_MS);
        match self.chart {
            Chart::RateLimit => Request::QuotaHistory,
            Chart::ModelCost => Request::GlobalUsage {
                since_ms: back(self.days.count()),
            },
            Chart::UsageShare => Request::GlobalUsage {
                since_ms: back(self.days.count() + self.days.warmup()),
            },
            Chart::AgentCost => Request::AgentCostDistribution {
                since_ms: back(self.days.count() + self.days.warmup()),
            },
        }
    }
}

/// What a chart asks every host for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    QuotaHistory,
    GlobalUsage { since_ms: u64 },
    AgentCostDistribution { since_ms: u64 },
}

/// What moved in what the system holds for the charts: the surfaces it
/// concerns draw again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageMoved {
    /// Quota history or the active accounts: every rate-limit chart.
    Quota,
    /// One chart's series.
    Series(UsageChart),
    /// A host went, and everything it said with it.
    Host,
    /// A host connected, or came back. A chart open now asks again: what it
    /// asked while the host was away failed, and nothing else asks for it.
    Connected,
}

impl UsageMoved {
    pub fn concerns(self, chart: UsageChart) -> bool {
        match self {
            Self::Quota => chart.chart == Chart::RateLimit,
            Self::Series(moved) => moved == chart,
            Self::Host | Self::Connected => true,
        }
    }
}

/// Every host's answer for the spend charts, kept per chart and per host.
///
/// Per chart because each asks for its own window: a seven-day answer is
/// not what a thirty-day chart shows. Per host because the hosts are
/// merged on the way out: unlike quota headroom, cost incurred on two
/// machines is cost incurred twice. Quota history lives in
/// [`rho_agents_client::quota::Quotas`] with the rest of what hosts say
/// about quota, since every rate-limit chart reads the same series.
#[derive(Default)]
pub(crate) struct Usage {
    /// Model cost and usage share: what `GlobalUsage` answered.
    spend: HashMap<UsageChart, BTreeMap<HostId, Vec<AgentUsageSeries>>>,
    /// Agent cost, by window: what `AgentCostDistribution` answered.
    agent_cost: HashMap<Days, BTreeMap<HostId, Vec<AgentCostSeries>>>,
}

impl Usage {
    pub(crate) fn record_spend(
        &mut self,
        chart: UsageChart,
        host: HostId,
        series: Vec<AgentUsageSeries>,
    ) {
        self.spend.entry(chart).or_default().insert(host, series);
    }

    pub(crate) fn record_agent_cost(
        &mut self,
        days: Days,
        host: HostId,
        series: Vec<AgentCostSeries>,
    ) {
        self.agent_cost
            .entry(days)
            .or_default()
            .insert(host, series);
    }

    /// Drops a host that has gone, so its last numbers stop being counted.
    pub(crate) fn forget(&mut self, host: HostId) {
        for hosts in self.spend.values_mut() {
            hosts.remove(&host);
        }
        for hosts in self.agent_cost.values_mut() {
            hosts.remove(&host);
        }
        self.spend.retain(|_, hosts| !hosts.is_empty());
        self.agent_cost.retain(|_, hosts| !hosts.is_empty());
    }

    /// A spend chart's series summed across hosts, model by model and
    /// bucket by bucket; `None` until a host has answered.
    pub(crate) fn spend(&self, chart: UsageChart) -> Option<Vec<AgentUsageSeries>> {
        let hosts = self.spend.get(&chart)?;
        let mut merged: Vec<AgentUsageSeries> = Vec::new();
        for series in hosts.values().flatten() {
            let Some(existing) = merged
                .iter_mut()
                .find(|existing| existing.model == series.model)
            else {
                merged.push(series.clone());
                continue;
            };
            for bucket in &series.buckets {
                match existing
                    .buckets
                    .iter_mut()
                    .find(|candidate| candidate.bucket_start_ms == bucket.bucket_start_ms)
                {
                    Some(candidate) => {
                        candidate.input_tokens += bucket.input_tokens;
                        candidate.cache_read_tokens += bucket.cache_read_tokens;
                        candidate.cache_write_tokens += bucket.cache_write_tokens;
                        candidate.cache_write_1h_tokens += bucket.cache_write_1h_tokens;
                        candidate.output_tokens += bucket.output_tokens;
                        candidate.requests += bucket.requests;
                        candidate.approximate |= bucket.approximate;
                    }
                    None => existing.buckets.push(bucket.clone()),
                }
            }
        }
        // In time order whichever host said what, and however one did.
        for series in &mut merged {
            series.buckets.sort_by_key(|bucket| bucket.bucket_start_ms);
        }
        Some(merged)
    }

    /// Each host's per-agent cost, kept apart: agent ids are each host's
    /// own, so two hosts' agent 1 are two agents. `None` until a host has
    /// answered.
    pub(crate) fn agent_cost(&self, days: Days) -> Option<Vec<Vec<AgentCostSeries>>> {
        Some(self.agent_cost.get(&days)?.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentId, AgentIdDomain};
    use rho_agents_client::protocol::AgentUsageBucket;
    use rho_frame::Address;

    use super::*;

    fn parse(key: &str) -> Option<UsageChart> {
        UsageChart::from_address(&Address::parse(key).unwrap())
    }

    #[test]
    fn the_eight_addresses_read_back() {
        let expected = [
            ("usage:rate-limit/7", Chart::RateLimit, Days::Seven),
            ("usage:rate-limit/30", Chart::RateLimit, Days::Thirty),
            ("usage:model-cost/7", Chart::ModelCost, Days::Seven),
            ("usage:model-cost/30", Chart::ModelCost, Days::Thirty),
            ("usage:usage-share/7", Chart::UsageShare, Days::Seven),
            ("usage:usage-share/30", Chart::UsageShare, Days::Thirty),
            ("usage:agent-cost/7", Chart::AgentCost, Days::Seven),
            ("usage:agent-cost/30", Chart::AgentCost, Days::Thirty),
        ];
        for (key, chart, days) in expected {
            assert_eq!(parse(key), Some(UsageChart { chart, days }), "{key}");
        }
        let written: Vec<String> = UsageChart::all()
            .map(|chart| chart.address().into_key())
            .collect();
        let keys: Vec<&str> = expected.iter().map(|(key, _, _)| *key).collect();
        assert_eq!(written, keys);
    }

    #[test]
    fn anything_else_is_not_a_usage_address() {
        for key in [
            // a chart that is not one
            "usage:rate/7",
            "usage:Rate-Limit/7",
            "usage:rate_limit/7",
            "usage:share/7",
            // a window that is not one
            "usage:rate-limit/14",
            "usage:rate-limit/07",
            "usage:rate-limit/7d",
            "usage:rate-limit/+7",
            "usage:rate-limit/ 7",
            // a part missing or extra
            "usage:rate-limit",
            "usage:rate-limit/",
            "usage:/7",
            "usage:",
            "usage:7",
            "usage:rate-limit/7/",
            "usage:rate-limit/7/30",
            // another kind
            "agent:rate-limit/7",
        ] {
            assert_eq!(parse(key), None, "{key}");
        }
    }

    fn bucket(start: u64, input: u64, requests: u64, approximate: bool) -> AgentUsageBucket {
        AgentUsageBucket {
            bucket_start_ms: start,
            input_tokens: input,
            cache_read_tokens: input * 2,
            cache_write_tokens: input * 3,
            cache_write_1h_tokens: input * 4,
            output_tokens: input * 5,
            requests,
            approximate,
        }
    }

    fn series(model: &str, buckets: Vec<AgentUsageBucket>) -> AgentUsageSeries {
        AgentUsageSeries {
            model: model.to_owned(),
            buckets,
        }
    }

    #[test]
    fn two_hosts_spend_sums_by_model_and_bucket() {
        let chart = UsageChart {
            chart: Chart::ModelCost,
            days: Days::Seven,
        };
        let mut usage = Usage::default();
        assert_eq!(usage.spend(chart), None);
        usage.record_spend(
            chart,
            HostId(0),
            vec![
                series(
                    "opus",
                    vec![bucket(10, 1, 1, false), bucket(30, 2, 1, false)],
                ),
                series("gpt", vec![bucket(10, 7, 3, false)]),
            ],
        );
        usage.record_spend(
            chart,
            HostId(1),
            vec![
                // out of order, one bucket shared with host 0 and one not
                series(
                    "opus",
                    vec![bucket(20, 10, 2, true), bucket(10, 100, 5, false)],
                ),
                series("fable", vec![bucket(40, 9, 1, false)]),
            ],
        );
        let merged = usage.spend(chart).unwrap();
        let opus = merged.iter().find(|series| series.model == "opus").unwrap();
        assert_eq!(
            opus.buckets,
            vec![
                bucket(10, 101, 6, false),
                bucket(20, 10, 2, true),
                bucket(30, 2, 1, false),
            ]
        );
        let gpt = merged.iter().find(|series| series.model == "gpt").unwrap();
        assert_eq!(gpt.buckets, vec![bucket(10, 7, 3, false)]);
        let fable = merged
            .iter()
            .find(|series| series.model == "fable")
            .unwrap();
        assert_eq!(fable.buckets, vec![bucket(40, 9, 1, false)]);
        assert_eq!(merged.len(), 3);

        // Another chart's window is another answer.
        let month = UsageChart {
            days: Days::Thirty,
            ..chart
        };
        assert_eq!(usage.spend(month), None);

        // A host that answers again replaces what it said, rather than
        // being counted twice.
        usage.record_spend(
            chart,
            HostId(0),
            vec![series("gpt", vec![bucket(10, 1, 1, false)])],
        );
        let merged = usage.spend(chart).unwrap();
        let opus = merged.iter().find(|series| series.model == "opus").unwrap();
        assert_eq!(
            opus.buckets,
            vec![bucket(10, 100, 5, false), bucket(20, 10, 2, true)]
        );

        // A host that goes takes its numbers with it.
        usage.forget(HostId(1));
        let merged = usage.spend(chart).unwrap();
        assert_eq!(merged, vec![series("gpt", vec![bucket(10, 1, 1, false)])]);
        usage.forget(HostId(0));
        assert_eq!(usage.spend(chart), None);
    }

    #[test]
    fn two_hosts_agent_cost_stays_apart() {
        let cost = |agent: u64, input: u64| AgentCostSeries {
            agent_id: AgentId::from_counter(agent, &AgentIdDomain(0)).unwrap(),
            model: "gpt".to_owned(),
            buckets: vec![bucket(10, input, 1, false)],
        };
        let mut usage = Usage::default();
        assert_eq!(usage.agent_cost(Days::Seven), None);
        usage.record_agent_cost(Days::Seven, HostId(1), vec![cost(1, 5)]);
        usage.record_agent_cost(Days::Seven, HostId(0), vec![cost(1, 3), cost(2, 4)]);
        assert_eq!(
            usage.agent_cost(Days::Seven),
            Some(vec![vec![cost(1, 3), cost(2, 4)], vec![cost(1, 5)]])
        );
        assert_eq!(usage.agent_cost(Days::Thirty), None);
    }

    #[test]
    fn a_chart_asks_for_its_window_and_the_warmup() {
        let now = 100 * DAY_MS;
        let ask = |chart, days| UsageChart { chart, days }.request(now);
        assert_eq!(ask(Chart::RateLimit, Days::Thirty), Request::QuotaHistory);
        assert_eq!(
            ask(Chart::ModelCost, Days::Thirty),
            Request::GlobalUsage {
                since_ms: 70 * DAY_MS
            }
        );
        assert_eq!(
            ask(Chart::UsageShare, Days::Seven),
            Request::GlobalUsage {
                since_ms: 89 * DAY_MS
            }
        );
        assert_eq!(
            ask(Chart::AgentCost, Days::Thirty),
            Request::AgentCostDistribution {
                since_ms: 56 * DAY_MS
            }
        );
    }

    /// Stepping walks all four pictures and wraps either way; the other
    /// window keeps the picture.
    #[test]
    fn switching_steps_through_every_picture_and_window() {
        let start = UsageChart {
            chart: Chart::RateLimit,
            days: Days::Seven,
        };
        let mut seen = vec![start];
        let mut at = start;
        for _ in 0..4 {
            at = at.step(true);
            seen.push(at);
        }
        assert_eq!(
            seen.iter().map(|chart| chart.chart).collect::<Vec<_>>(),
            [
                Chart::RateLimit,
                Chart::ModelCost,
                Chart::UsageShare,
                Chart::AgentCost,
                Chart::RateLimit,
            ]
        );
        assert!(seen.iter().all(|chart| chart.days == Days::Seven));
        assert_eq!(start.step(false).chart, Chart::AgentCost);
        let wider = start.step(true).other_window();
        assert_eq!(wider.address().key(), "usage:model-cost/30");
        assert_eq!(wider.other_window().address().key(), "usage:model-cost/7");
    }
}
