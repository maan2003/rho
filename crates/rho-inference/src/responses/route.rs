use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use redb::{TableDefinition, TableHandle as _};
use rho_core::UnixMs;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::oauth::ResolvedAuth;
use super::session::{ResponsesConfig, ResponsesModel, ServiceTier};
use super::wire::ResponsesRequest;
use super::ws::{self, WsResponseCreate};
use crate::accounts::SelectedAuth;
use crate::inference::Inference;

const PROBE_INTERVAL: Duration = Duration::from_secs(30 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBES_PER_ROUTE: usize = 2;
const HISTORY_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const PROBE_HISTORY: TableDefinition<String, Sen<RouteProbeRecord>> =
    TableDefinition::new("chatgpt_route_probe_history");

const ROUTES: [DialRoute; 3] = [DialRoute::Dns, DialRoute::PinnedA, DialRoute::PinnedB];

/// A bounded set of ChatGPT edge paths. Fixed routes still use chatgpt.com as
/// the WebSocket URL and TLS server name; only the TCP destination changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DialRoute {
    Dns,
    PinnedA,
    PinnedB,
}

impl DialRoute {
    pub(crate) const fn ip(self) -> Option<IpAddr> {
        match self {
            Self::Dns => None,
            Self::PinnedA => Some(IpAddr::V4(Ipv4Addr::new(103, 31, 4, 5))),
            Self::PinnedB => Some(IpAddr::V4(Ipv4Addr::new(162, 159, 224, 5))),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::PinnedA => "pinned-a",
            Self::PinnedB => "pinned-b",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RouteSelector {
    selected: watch::Sender<RouteSelection>,
    db: Option<RhoDb>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RouteSelection {
    route: DialRoute,
    account: Option<RouteAccount>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceRouteProbe {
    pub observed_at: UnixMs,
    pub auth_namespace: Option<String>,
    pub route: String,
    pub destination_ip: Option<String>,
    pub cloudflare_colo: Option<String>,
    pub latency_ms: Vec<u32>,
    pub succeeded: bool,
}

#[derive(Clone, Debug, Encode, Decode)]
struct RouteProbeRecord {
    observed_at_ms: u64,
    auth_namespace: Option<String>,
    route: String,
    destination_ip: Option<String>,
    cloudflare_colo: Option<String>,
    latency_ms: Vec<u32>,
    succeeded: bool,
}

impl From<RouteProbeRecord> for InferenceRouteProbe {
    fn from(record: RouteProbeRecord) -> Self {
        Self {
            observed_at: UnixMs(record.observed_at_ms),
            auth_namespace: record.auth_namespace,
            route: record.route,
            destination_ip: record.destination_ip,
            cloudflare_colo: record.cloudflare_colo,
            latency_ms: record.latency_ms,
            succeeded: record.succeeded,
        }
    }
}

#[derive(Debug)]
struct RouteMeasurement {
    cloudflare_colo: Option<String>,
    samples: Vec<Duration>,
}

impl RouteMeasurement {
    fn score(&self) -> Duration {
        self.samples.iter().copied().sum::<Duration>() / self.samples.len() as u32
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RouteAccount {
    namespace: Option<String>,
    account_id: Option<String>,
}

impl RouteAccount {
    fn from_selected(selected: &SelectedAuth) -> Self {
        Self {
            namespace: selected.namespace.clone(),
            account_id: selected.account_id.clone(),
        }
    }

    fn matches(&self, selected: &SelectedAuth) -> bool {
        match (&self.account_id, &selected.account_id) {
            (Some(probed), Some(requested)) => probed == requested,
            _ => self.namespace == selected.namespace,
        }
    }
}

impl RouteSelector {
    pub(crate) fn new(db: Option<RhoDb>) -> Self {
        let (selected, _) = watch::channel(RouteSelection {
            route: DialRoute::Dns,
            account: None,
        });
        Self { selected, db }
    }

    pub(crate) const fn probe_interval() -> Duration {
        PROBE_INTERVAL
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<RouteSelection> {
        self.selected.subscribe()
    }

    pub(crate) fn history(&self, since: UnixMs) -> Vec<InferenceRouteProbe> {
        let Some(db) = &self.db else {
            return Vec::new();
        };
        route_probe_records(db)
            .into_iter()
            .filter(|record| record.observed_at_ms >= since.0)
            .map(Into::into)
            .collect()
    }

    pub(crate) fn for_session(
        &self,
        config: &ResponsesConfig,
        selected: Option<&SelectedAuth>,
    ) -> DialRoute {
        if config.model == ResponsesModel::Gpt56Luna && config.service_tier == ServiceTier::Normal {
            let route = self.selected.borrow();
            if route.route == DialRoute::Dns
                || selected.is_some_and(|selected| {
                    route
                        .account
                        .as_ref()
                        .is_some_and(|account| account.matches(selected))
                })
            {
                route.route
            } else {
                DialRoute::Dns
            }
        } else {
            DialRoute::Dns
        }
    }

    pub(crate) async fn probe_once(&self, inference: &Inference) {
        let selected = match inference.select().await {
            Ok(selected) => selected,
            Err(error) => {
                tracing::debug!(%error, "skipping ChatGPT route probe without an account");
                return;
            }
        };
        let auth = selected.auth.clone();
        let resolved = match tokio::task::spawn_blocking(move || auth.resolve()).await {
            Ok(Ok(resolved)) => resolved,
            Ok(Err(error)) => {
                tracing::debug!(%error, "skipping ChatGPT route probe without resolved auth");
                return;
            }
            Err(error) => {
                tracing::debug!(%error, "ChatGPT route auth task failed");
                return;
            }
        };

        let prompt_cache_keys = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
        let probes = futures::future::join_all(ROUTES.into_iter().map(|route| {
            probe_route(
                inference.responses_base_url(),
                &resolved,
                route,
                &prompt_cache_keys,
            )
        }))
        .await;
        if let Some(db) = &self.db {
            store_probe_results(db, UnixMs::now(), selected.namespace.as_deref(), &probes).await;
        }
        let scores = ROUTES
            .into_iter()
            .zip(probes)
            .filter_map(|(route, result)| match result {
                Ok(measurement) => {
                    let score = measurement.score();
                    tracing::debug!(
                        route = route.name(),
                        latency_ms = score.as_millis(),
                        "ChatGPT route probe"
                    );
                    Some((route, score))
                }
                Err(error) => {
                    tracing::debug!(route = route.name(), %error, "ChatGPT route probe failed");
                    None
                }
            })
            .collect::<Vec<_>>();

        let account = RouteAccount::from_selected(&SelectedAuth {
            account_id: resolved.account_id.clone(),
            ..selected
        });
        let previous = self.selected.borrow().clone();
        let current = if previous.account.as_ref() == Some(&account) {
            previous.route
        } else {
            DialRoute::Dns
        };
        let next = choose_route(current, &scores);
        let selection = RouteSelection {
            route: next,
            account: Some(account),
        };
        if selection != previous {
            self.selected.send_replace(selection);
            if next != previous.route {
                tracing::info!(
                    from = previous.route.name(),
                    to = next.name(),
                    "switched ChatGPT inference route"
                );
            }
        }
    }

    pub(crate) fn report_connect_failure(&self, route: DialRoute, account: Option<&SelectedAuth>) {
        let selected = self.selected.borrow();
        let matches_account = account.is_some_and(|account| {
            selected
                .account
                .as_ref()
                .is_some_and(|probed| probed.matches(account))
        });
        if route != DialRoute::Dns && selected.route == route && matches_account {
            drop(selected);
            self.selected
                .send_modify(|selected| selected.route = DialRoute::Dns);
            tracing::warn!(
                route = route.name(),
                "ChatGPT inference route failed; falling back to DNS"
            );
        }
    }
}

async fn probe_route(
    base_url: &str,
    auth: &ResolvedAuth,
    route: DialRoute,
    prompt_cache_keys: &[uuid::Uuid; PROBES_PER_ROUTE],
) -> Result<RouteMeasurement> {
    let request = ws::build_ws_request_for_base_url(base_url, None, auth)?;
    let (mut socket, response) =
        tokio::time::timeout(PROBE_TIMEOUT, ws::connect(request, route)).await??;
    let cloudflare_colo = response
        .headers()
        .get("cf-ray")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit('-').next())
        .filter(|colo| {
            (colo.len() == 3 || colo.len() == 4)
                && colo.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .map(|colo| colo.to_ascii_uppercase());
    let mut samples = Vec::with_capacity(PROBES_PER_ROUTE);
    for prompt_cache_key in prompt_cache_keys {
        let body = serde_json::to_string(&WsResponseCreate {
            ty: "response.create",
            body: ResponsesRequest::luna_default_probe(*prompt_cache_key),
        })?;
        let started = tokio::time::Instant::now();
        socket.send(WsMessage::Text(body.into())).await?;
        let admission_latency = tokio::time::timeout(PROBE_TIMEOUT, async {
            let mut rate_limits_at = None;
            loop {
                match socket.next().await.transpose()? {
                    Some(WsMessage::Text(text)) => {
                        if text.len() > 256 * 1024 {
                            bail!("probe event exceeded size limit");
                        }
                        let event: serde_json::Value = serde_json::from_str(&text)?;
                        match event.get("type").and_then(serde_json::Value::as_str) {
                            Some("codex.rate_limits") => {
                                rate_limits_at.get_or_insert_with(|| started.elapsed());
                            }
                            Some("response.completed" | "response.done") => {
                                return rate_limits_at.ok_or_else(|| {
                                    anyhow::anyhow!("probe completed without codex.rate_limits")
                                });
                            }
                            Some("error" | "response.failed" | "response.incomplete") => {
                                bail!("probe response failed")
                            }
                            _ => {}
                        }
                    }
                    Some(WsMessage::Ping(payload)) => {
                        socket.send(WsMessage::Pong(payload)).await?;
                    }
                    Some(WsMessage::Close(_)) | None => bail!("probe socket closed"),
                    Some(_) => {}
                }
            }
        })
        .await??;
        samples.push(admission_latency);
    }
    Ok(RouteMeasurement {
        cloudflare_colo,
        samples,
    })
}

async fn store_probe_results(
    db: &RhoDb,
    observed_at: UnixMs,
    auth_namespace: Option<&str>,
    probes: &[Result<RouteMeasurement>],
) {
    let records = ROUTES
        .into_iter()
        .zip(probes)
        .map(|(route, result)| RouteProbeRecord {
            observed_at_ms: observed_at.0,
            auth_namespace: auth_namespace.map(str::to_owned),
            route: route.name().to_owned(),
            destination_ip: route.ip().map(|ip| ip.to_string()),
            cloudflare_colo: result
                .as_ref()
                .ok()
                .and_then(|measurement| measurement.cloudflare_colo.clone()),
            latency_ms: result
                .as_ref()
                .map(|measurement| {
                    measurement
                        .samples
                        .iter()
                        .map(|sample| sample.as_millis().min(u32::MAX.into()) as u32)
                        .collect()
                })
                .unwrap_or_default(),
            succeeded: result.is_ok(),
        })
        .collect::<Vec<_>>();
    let cutoff = observed_at.0.saturating_sub(HISTORY_RETENTION_MS);
    let mut write = db.write().await;
    let mut table = write.open_table(PROBE_HISTORY);
    let expired = table
        .iter()
        .filter_map(|(key, record)| {
            (record.value().as_ref().observed_at_ms < cutoff).then(|| key.value())
        })
        .collect::<Vec<_>>();
    for key in expired {
        table.remove(&key);
    }
    for record in records {
        let key = format!("{:020}:{}", record.observed_at_ms, record.route);
        table.insert(&key, SenValue::borrowed(&record));
    }
    drop(table);
    write.commit();
}

fn route_probe_records(db: &RhoDb) -> Vec<RouteProbeRecord> {
    let read = db.read();
    if !read.has_table(PROBE_HISTORY.name()) {
        return Vec::new();
    }
    read.open_table(PROBE_HISTORY)
        .iter()
        .map(|(_, record)| record.value().into_owned())
        .collect()
}

fn choose_route(current: DialRoute, scores: &[(DialRoute, Duration)]) -> DialRoute {
    scores
        .iter()
        .min_by_key(|(_, score)| *score)
        .map_or(current, |(route, _)| *route)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::InferenceAuth;
    use crate::responses::session::{ReasoningContext, ResponsesEffort, TextVerbosity};

    fn luna_config(service_tier: ServiceTier) -> ResponsesConfig {
        ResponsesConfig {
            model: ResponsesModel::Gpt56Luna,
            auto_compaction: None,
            reasoning_context: ReasoningContext::AllTurns,
            effort: ResponsesEffort::Medium,
            text_verbosity: TextVerbosity::Low,
            service_tier,
        }
    }

    fn selected(namespace: &str, account_id: &str) -> SelectedAuth {
        SelectedAuth {
            auth: InferenceAuth::oauth_file("unused-route-test-auth.json"),
            namespace: Some(namespace.to_owned()),
            account_id: Some(account_id.to_owned()),
        }
    }

    #[test]
    fn probe_is_luna_default_without_generation() {
        let body =
            serde_json::to_value(ResponsesRequest::luna_default_probe(uuid::Uuid::nil())).unwrap();
        assert_eq!(body["model"], "gpt-5.6-luna");
        assert_eq!(body["service_tier"], "default");
        assert_eq!(body["generate"], false);
    }

    #[test]
    fn selected_route_is_luna_default_and_account_scoped() {
        let routes = RouteSelector::new(None);
        routes.selected.send_replace(RouteSelection {
            route: DialRoute::PinnedA,
            account: Some(RouteAccount::from_selected(&selected("one", "account-1"))),
        });

        assert_eq!(
            routes.for_session(
                &luna_config(ServiceTier::Normal),
                Some(&selected("one", "account-1")),
            ),
            DialRoute::PinnedA
        );
        assert_eq!(
            routes.for_session(
                &luna_config(ServiceTier::Normal),
                Some(&selected("two", "account-2")),
            ),
            DialRoute::Dns
        );
        assert_eq!(
            routes.for_session(
                &luna_config(ServiceTier::Priority),
                Some(&selected("one", "account-1")),
            ),
            DialRoute::Dns
        );

        routes.report_connect_failure(DialRoute::PinnedA, Some(&selected("two", "account-2")));
        assert_eq!(routes.selected.borrow().route, DialRoute::PinnedA);
        routes.report_connect_failure(DialRoute::PinnedA, Some(&selected("one", "account-1")));
        assert_eq!(routes.selected.borrow().route, DialRoute::Dns);
    }

    #[test]
    fn route_switches_to_fastest_result() {
        assert_eq!(
            choose_route(
                DialRoute::Dns,
                &[
                    (DialRoute::Dns, Duration::from_millis(300)),
                    (DialRoute::PinnedA, Duration::from_millis(251)),
                ],
            ),
            DialRoute::PinnedA
        );
        assert_eq!(
            choose_route(
                DialRoute::Dns,
                &[
                    (DialRoute::Dns, Duration::from_millis(5_000)),
                    (DialRoute::PinnedA, Duration::from_millis(4_900)),
                ],
            ),
            DialRoute::PinnedA
        );
    }

    #[test]
    fn unavailable_current_route_switches_to_fastest_success() {
        assert_eq!(
            choose_route(
                DialRoute::PinnedA,
                &[(DialRoute::Dns, Duration::from_millis(300))],
            ),
            DialRoute::Dns
        );
    }

    #[tokio::test]
    async fn probe_history_persists_successes_and_failures() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let probes = vec![
            Ok(RouteMeasurement {
                cloudflare_colo: Some("FRA".to_owned()),
                samples: vec![Duration::from_millis(301), Duration::from_millis(303)],
            }),
            Err(anyhow::anyhow!("unavailable")),
            Ok(RouteMeasurement {
                cloudflare_colo: Some("EWR".to_owned()),
                samples: vec![Duration::from_millis(250), Duration::from_millis(252)],
            }),
        ];
        store_probe_results(&db, UnixMs(1_000), Some("second"), &probes).await;

        let history = RouteSelector::new(Some(db)).history(UnixMs(0));
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].route, "dns");
        assert_eq!(history[0].auth_namespace.as_deref(), Some("second"));
        assert_eq!(history[0].cloudflare_colo.as_deref(), Some("FRA"));
        assert_eq!(history[0].latency_ms, [301, 303]);
        assert!(history[0].succeeded);
        assert_eq!(history[1].destination_ip.as_deref(), Some("103.31.4.5"));
        assert!(history[1].latency_ms.is_empty());
        assert!(!history[1].succeeded);
        assert_eq!(history[2].cloudflare_colo.as_deref(), Some("EWR"));
    }
}
