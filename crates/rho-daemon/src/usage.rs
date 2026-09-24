//! What the host tells clients about quota and usage: each provider's
//! remaining quota and how fast it burns, its history, and the token use
//! of every model and agent, read from what the database recorded.

use std::collections::BTreeMap;

use rho_agent::db::{
    AgentReadTxnExt as _, AgentUsageModel, AgentWriteTxnExt as _, QuotaModel,
    QuotaObservationRecord, QuotaProvider,
};
use rho_agent_host_proto::{
    AgentCostSeries, AgentUsageBucket as UiAgentUsageBucket, AgentUsageSeries, QuotaPoint,
    QuotaSeries, QuotaSummary,
};
use rho_agent_types::AgentId;
use rho_db::RhoDb;
use rho_inference::Inference;

pub(crate) fn quota_summaries(db: &RhoDb, inference: &Inference) -> Vec<QuotaSummary> {
    let mut summaries = claude_quota_summaries(db);
    summaries.extend(
        inference
            .state()
            .quotas
            .into_iter()
            .map(|summary| QuotaSummary {
                model: "gpt".to_owned(),
                auth_namespace: Some(summary.auth_namespace),
                remaining_percent: summary.remaining_percent,
                burn_10m: summary.burn_10m,
                burn_2h: summary.burn_2h,
                burn_1d: summary.burn_1d,
                burn_3d: summary.burn_3d,
                reset_at_unix: summary.reset_at_unix,
            }),
    );
    summaries
}

fn claude_quota_summaries(db: &RhoDb) -> Vec<QuotaSummary> {
    let now = rho_agent_types::UnixMs::now().0;
    let since = rho_agent_types::UnixMs(now.saturating_sub(3 * 24 * 60 * 60 * 1_000));
    quota_observation_groups(db, since)
        .into_iter()
        .filter_map(|((model, auth_namespace), observations)| {
            let samples = observations.iter().collect::<Vec<_>>();
            let latest = samples.last()?;
            let reset_expired = latest
                .reset_at_unix
                .is_some_and(|reset| reset <= (now / 1_000) as i64);
            let burn = |duration| {
                if reset_expired {
                    0
                } else {
                    quota_burn(&samples, now, duration)
                }
            };
            Some(QuotaSummary {
                model: model.name().to_owned(),
                auth_namespace,
                remaining_percent: if reset_expired {
                    100
                } else {
                    100u8.saturating_sub(latest.used_percent)
                },
                burn_10m: burn(10 * 60 * 1_000),
                burn_2h: burn(2 * 60 * 60 * 1_000),
                burn_1d: burn(24 * 60 * 60 * 1_000),
                burn_3d: burn(3 * 24 * 60 * 60 * 1_000),
                reset_at_unix: if reset_expired {
                    None
                } else {
                    latest.reset_at_unix
                },
            })
        })
        .collect()
}

fn ui_agent_usage_bucket(bucket: rho_agent::db::AgentUsageBucket) -> UiAgentUsageBucket {
    UiAgentUsageBucket {
        bucket_start_ms: bucket.bucket_start_ms,
        input_tokens: bucket.input_tokens,
        cache_read_tokens: bucket.cache_read_tokens,
        cache_write_tokens: bucket.cache_write_tokens,
        cache_write_1h_tokens: bucket.cache_write_1h_tokens,
        output_tokens: bucket.output_tokens,
        requests: bucket.requests,
        approximate: bucket.approximate,
    }
}

/// Reduces the indexed five-minute usage records to the hourly samples the
/// usage-share chart renders. The persisted key begins with time, so the
/// preceding database query is already a bounded range scan.
fn hourly_global_usage_series(
    usage: Vec<(AgentUsageModel, rho_agent::db::AgentUsageBucket)>,
) -> Vec<AgentUsageSeries> {
    const HOUR_MS: u64 = 60 * 60 * 1_000;

    let mut hourly = BTreeMap::<(AgentUsageModel, u64), rho_agent::db::AgentUsageBucket>::new();
    for (model, bucket) in usage {
        let bucket_start_ms = bucket.bucket_start_ms / HOUR_MS * HOUR_MS;
        hourly
            .entry((model, bucket_start_ms))
            .or_insert_with(|| rho_agent::db::AgentUsageBucket {
                bucket_start_ms,
                model,
                ..rho_agent::db::AgentUsageBucket::default()
            })
            .add(&bucket);
    }

    [
        AgentUsageModel::FABLE,
        AgentUsageModel::GPT,
        AgentUsageModel::OPUS,
        AgentUsageModel::TERRA,
        AgentUsageModel::LUNA,
        AgentUsageModel::ASTRA,
    ]
    .into_iter()
    .map(|model| AgentUsageSeries {
        model: model.name().to_owned(),
        buckets: hourly
            .iter()
            .filter(|((candidate, _), _)| *candidate == model)
            .map(|(_, bucket)| ui_agent_usage_bucket(bucket.clone()))
            .collect(),
    })
    .collect()
}

fn hourly_agent_cost_series(
    db: &RhoDb,
    since: rho_agent_types::UnixMs,
) -> anyhow::Result<Vec<AgentCostSeries>> {
    const MAX_HOURLY_AGENT_COST_BUCKETS: usize = 500_000;

    let read = db.read();
    let mut hourly = BTreeMap::new();
    for agent_id in read.list_agent_ids() {
        for bucket in read.agent_usage(agent_id, since) {
            if !matches!(
                bucket.model,
                AgentUsageModel::GPT
                    | AgentUsageModel::ASTRA
                    | AgentUsageModel::TERRA
                    | AgentUsageModel::LUNA
                    | AgentUsageModel::UNKNOWN
            ) {
                continue;
            }
            merge_hourly_agent_cost_bucket(
                &mut hourly,
                agent_id,
                bucket,
                MAX_HOURLY_AGENT_COST_BUCKETS,
            )?;
        }
    }

    let mut series = BTreeMap::<(AgentId, AgentUsageModel), Vec<UiAgentUsageBucket>>::new();
    for ((agent_id, model, _), bucket) in hourly {
        series
            .entry((agent_id, model))
            .or_default()
            .push(ui_agent_usage_bucket(bucket));
    }
    Ok(series
        .into_iter()
        .map(|((agent_id, model), buckets)| AgentCostSeries {
            agent_id,
            model: model.name().to_owned(),
            buckets,
        })
        .collect())
}

fn merge_hourly_agent_cost_bucket(
    hourly: &mut BTreeMap<(AgentId, AgentUsageModel, u64), rho_agent::db::AgentUsageBucket>,
    agent_id: AgentId,
    bucket: rho_agent::db::AgentUsageBucket,
    max_buckets: usize,
) -> anyhow::Result<()> {
    const HOUR_MS: u64 = 60 * 60 * 1_000;

    let bucket_start_ms = bucket.bucket_start_ms / HOUR_MS * HOUR_MS;
    hourly
        .entry((agent_id, bucket.model, bucket_start_ms))
        .or_insert_with(|| rho_agent::db::AgentUsageBucket {
            bucket_start_ms,
            model: bucket.model,
            ..rho_agent::db::AgentUsageBucket::default()
        })
        .add(&bucket);
    anyhow::ensure!(
        hourly.len() <= max_buckets,
        "agent cost history exceeds {max_buckets} hourly buckets"
    );
    Ok(())
}

pub(crate) fn quota_history(db: &RhoDb, inference: &Inference) -> Vec<QuotaSeries> {
    let mut series = claude_quota_history(db);
    let since = rho_agent_types::UnixMs(
        rho_agent_types::UnixMs::now()
            .0
            .saturating_sub(30 * 24 * 60 * 60 * 1_000),
    );
    for history in inference.quota_history(since) {
        series.push(QuotaSeries {
            model: "gpt".to_owned(),
            auth_namespace: Some(history.auth_namespace),
            points: history
                .points
                .into_iter()
                .map(|point| rho_agent_host_proto::QuotaPoint {
                    observed_at_ms: point.observed_at.0,
                    remaining_percent: point.remaining_percent,
                    reset_at_unix: point.reset_at_unix,
                })
                .collect(),
        });
    }
    series
}

fn claude_quota_history(db: &RhoDb) -> Vec<QuotaSeries> {
    let now = rho_agent_types::UnixMs::now().0;
    let since = rho_agent_types::UnixMs(now.saturating_sub(30 * 24 * 60 * 60 * 1_000));
    quota_observation_groups(db, since)
        .into_iter()
        .filter_map(|((model, auth_namespace), observations)| {
            let points = observations
                .into_iter()
                .map(|sample| QuotaPoint {
                    observed_at_ms: sample.observed_at.0,
                    remaining_percent: 100u8.saturating_sub(sample.used_percent),
                    reset_at_unix: sample.reset_at_unix,
                })
                .collect::<Vec<_>>();
            (!points.is_empty()).then(|| QuotaSeries {
                model: model.name().to_owned(),
                auth_namespace,
                points,
            })
        })
        .collect()
}

fn quota_observation_groups(
    db: &RhoDb,
    since: rho_agent_types::UnixMs,
) -> BTreeMap<(QuotaModel, Option<String>), Vec<QuotaObservationRecord>> {
    let read = db.read();
    let mut groups = BTreeMap::new();
    for model in [QuotaModel::OPUS, QuotaModel::FABLE] {
        for observation in read.quota_observations(model, since) {
            groups
                .entry((model, observation.auth_namespace.clone()))
                .or_insert_with(Vec::new)
                .push(observation);
        }
    }
    groups
}

fn quota_burn(samples: &[&QuotaObservationRecord], now: u64, duration_ms: u64) -> u16 {
    let cutoff = now.saturating_sub(duration_ms);
    let start = samples
        .partition_point(|sample| sample.observed_at.0 < cutoff)
        .saturating_sub(1);
    let Some((first, rest)) = samples
        .get(start..)
        .and_then(|samples| samples.split_first())
    else {
        return 0;
    };

    let mut epoch_start = *first;
    let mut epoch_end = *first;
    let mut burn = 0u16;
    for sample in rest {
        let same_epoch = match (epoch_end.reset_at_unix, sample.reset_at_unix) {
            (Some(old), Some(new)) => old.abs_diff(new) <= 60,
            (None, None) => true,
            _ => false,
        };
        if same_epoch {
            epoch_end = sample;
        } else {
            burn += epoch_end
                .used_percent
                .saturating_sub(epoch_start.used_percent) as u16;
            epoch_start = sample;
            epoch_end = sample;
        }
    }
    burn + epoch_end
        .used_percent
        .saturating_sub(epoch_start.used_percent) as u16
}

/// Every model's use since `since_ms`, an hour to a sample.
pub(crate) fn global_usage(db: &RhoDb, since_ms: u64) -> Vec<AgentUsageSeries> {
    let usage = db
        .read()
        .global_agent_usage(rho_agent_types::UnixMs(since_ms));
    hourly_global_usage_series(usage)
}

/// Each agent's hourly use, from far enough before `since_ms` that the
/// first window is whole, and no further back than the chart ever shows.
pub(crate) fn agent_costs(db: &RhoDb, since_ms: u64) -> anyhow::Result<Vec<AgentCostSeries>> {
    const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
    const MAX_HISTORY_DAYS: u64 = 30 + 14 + rho_agent_host_proto::AGENT_COST_WINDOW_DAYS;

    let now = rho_agent_types::UnixMs::now().0;
    let earliest = since_ms
        .saturating_sub(rho_agent_host_proto::AGENT_COST_WINDOW_DAYS * DAY_MS)
        .max(now.saturating_sub(MAX_HISTORY_DAYS * DAY_MS));
    hourly_agent_cost_series(db, rho_agent_types::UnixMs(earliest))
}

pub(crate) fn spawn_claude_quota_recorder(
    mut updates: tokio::sync::mpsc::Receiver<anyhow::Result<rho_claude_usage::ClaudeUsage>>,
    account: String,
    db: RhoDb,
    quota: tokio::sync::watch::Sender<()>,
) {
    tokio::spawn(async move {
        while let Some(update) = updates.recv().await {
            let usage = match update {
                Ok(usage) => usage,
                Err(error) => {
                    tracing::warn!(%error, %account, "Claude quota probe failed");
                    continue;
                }
            };
            let observed_at = rho_agent_types::UnixMs::now();
            let mut write = db.write().await;
            let mut changed = write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::Claude,
                model: QuotaModel::OPUS,
                auth_namespace: Some(account.clone()),
                observed_at,
                used_percent: usage.all_models.used_percent,
                reset_at_unix: Some(usage.all_models.reset_at_unix),
            });
            changed |= write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::Claude,
                model: QuotaModel::FABLE,
                auth_namespace: Some(account.clone()),
                observed_at,
                used_percent: usage.fable.used_percent,
                reset_at_unix: Some(usage.fable.reset_at_unix),
            });
            write.commit();
            if changed {
                quota.send_replace(());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rho_agent::db::{
        AgentUsageModel, AgentWriteTxnExt, QuotaModel, QuotaObservationRecord, QuotaProvider,
    };
    use rho_db::RhoDb;

    use super::{
        claude_quota_history, claude_quota_summaries, hourly_global_usage_series,
        merge_hourly_agent_cost_bucket, quota_burn,
    };

    #[test]
    fn global_usage_response_rolls_five_minute_buckets_up_to_hours() {
        let bucket = |model, bucket_start_ms, input_tokens| rho_agent::db::AgentUsageBucket {
            bucket_start_ms,
            model,
            input_tokens,
            requests: 1,
            ..Default::default()
        };
        let series = hourly_global_usage_series(vec![
            (
                AgentUsageModel::FABLE,
                bucket(AgentUsageModel::FABLE, 5 * 60 * 1_000, 10),
            ),
            (
                AgentUsageModel::FABLE,
                bucket(AgentUsageModel::FABLE, 55 * 60 * 1_000, 20),
            ),
            (
                AgentUsageModel::GPT,
                bucket(AgentUsageModel::GPT, 60 * 60 * 1_000, 30),
            ),
        ]);

        assert_eq!(series.len(), 6);
        assert_eq!(series[0].model, "fable");
        assert_eq!(series[0].buckets.len(), 1);
        assert_eq!(series[0].buckets[0].bucket_start_ms, 0);
        assert_eq!(series[0].buckets[0].input_tokens, 30);
        assert_eq!(series[0].buckets[0].requests, 2);
        assert_eq!(series[1].model, "gpt");
        assert_eq!(series[1].buckets[0].bucket_start_ms, 60 * 60 * 1_000);
        assert_eq!(series[5].model, "astra");
        assert!(series[5].buckets.is_empty());
    }

    #[test]
    fn agent_cost_history_rejects_more_than_its_hourly_bucket_limit() {
        let agent_id =
            rho_agent_types::AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let bucket = |bucket_start_ms| rho_agent::db::AgentUsageBucket {
            bucket_start_ms,
            model: AgentUsageModel::GPT,
            requests: 1,
            ..Default::default()
        };
        let mut hourly = BTreeMap::new();
        merge_hourly_agent_cost_bucket(&mut hourly, agent_id, bucket(0), 1).unwrap();
        assert!(
            merge_hourly_agent_cost_bucket(&mut hourly, agent_id, bucket(60 * 60 * 1_000), 1,)
                .is_err()
        );
    }

    #[test]
    fn quota_burn_uses_net_change_within_each_reset_epoch() {
        let sample = |at, used_percent, reset_at_unix| QuotaObservationRecord {
            provider: QuotaProvider::ChatGpt,
            model: QuotaModel::GPT,
            auth_namespace: None,
            observed_at: rho_agent_types::UnixMs(at),
            used_percent,
            reset_at_unix,
        };
        let records = [
            sample(0, 10, Some(100)),
            sample(100, 15, Some(100)),
            sample(200, 13, Some(100)),
            sample(300, 3, Some(200)),
            sample(400, 6, Some(200)),
        ];
        let samples = records.iter().collect::<Vec<_>>();
        assert_eq!(quota_burn(&samples, 400, 1_000), 6);
        assert_eq!(quota_burn(&samples, 400, 150), 3);
    }

    #[test]
    fn quota_burn_does_not_sum_sample_jitter() {
        let sample = |at, used_percent| QuotaObservationRecord {
            provider: QuotaProvider::ChatGpt,
            model: QuotaModel::GPT,
            auth_namespace: None,
            observed_at: rho_agent_types::UnixMs(at),
            used_percent,
            reset_at_unix: Some(100),
        };
        let records = [
            sample(0, 50),
            sample(100, 48),
            sample(200, 50),
            sample(300, 49),
            sample(400, 50),
        ];
        let samples = records.iter().collect::<Vec<_>>();

        assert_eq!(quota_burn(&samples, 400, 1_000), 0);
    }

    #[test]
    fn quota_burn_tolerates_reset_target_jitter() {
        let sample = |at, used_percent, reset_at_unix| QuotaObservationRecord {
            provider: QuotaProvider::ChatGpt,
            model: QuotaModel::GPT,
            auth_namespace: None,
            observed_at: rho_agent_types::UnixMs(at),
            used_percent,
            reset_at_unix: Some(reset_at_unix),
        };
        let records = [
            sample(0, 17, 1_000),
            sample(100, 15, 1_001),
            sample(200, 17, 999),
            sample(300, 16, 1_000),
            sample(400, 17, 1_002),
        ];
        let samples = records.iter().collect::<Vec<_>>();

        assert_eq!(quota_burn(&samples, 400, 1_000), 0);
    }

    #[tokio::test]
    async fn claude_quota_history_includes_every_stored_point() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let now = rho_agent_types::UnixMs::now().0;
        let mut write = db.write().await;
        for index in 0..5 {
            assert!(write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::Claude,
                model: QuotaModel::OPUS,
                auth_namespace: Some("default".to_owned()),
                observed_at: rho_agent_types::UnixMs(now - (4 - index) * 1_000),
                used_percent: index as u8,
                reset_at_unix: Some(123),
            }));
        }
        assert!(write.record_quota_observation(QuotaObservationRecord {
            provider: QuotaProvider::Claude,
            model: QuotaModel::FABLE,
            auth_namespace: None,
            observed_at: rho_agent_types::UnixMs(now),
            used_percent: 25,
            reset_at_unix: Some(456),
        }));
        write.commit();

        let history = claude_quota_history(&db);
        let opus = history
            .iter()
            .find(|series| series.model == "opus")
            .unwrap();
        assert_eq!(opus.points.len(), 5);
        assert_eq!(
            opus.points
                .iter()
                .map(|point| point.remaining_percent)
                .collect::<Vec<_>>(),
            [100, 99, 98, 97, 96]
        );
        let fable = history
            .iter()
            .find(|series| series.model == "fable")
            .unwrap();
        assert_eq!(fable.points[0].remaining_percent, 75);
    }

    #[tokio::test]
    async fn quota_summary_expires_stale_provider_window() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let now = rho_agent_types::UnixMs::now();
        let mut write = db.write().await;
        assert!(write.record_quota_observation(QuotaObservationRecord {
            provider: QuotaProvider::Claude,
            model: QuotaModel::FABLE,
            auth_namespace: None,
            observed_at: now,
            used_percent: 99,
            reset_at_unix: Some(1),
        }));
        write.commit();

        let summary = claude_quota_summaries(&db)
            .into_iter()
            .find(|summary| summary.model == "fable")
            .unwrap();
        assert_eq!(summary.remaining_percent, 100);
        assert_eq!(summary.burn_10m, 0);
        assert_eq!(summary.reset_at_unix, None);
    }
}
