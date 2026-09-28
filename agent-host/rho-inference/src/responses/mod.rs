//! Host credentials, quota and route policy for the Responses transport.
pub(crate) mod oauth;
mod route;
pub(crate) mod ws;
pub use rho_agent::inference::PromptCacheKey;
pub use oauth::{InferenceAuth, ResolvedAuth, ResolvedOAuth};
pub(crate) use route::RouteSelector;
pub use route::{DialRoute, InferenceRouteProbe, RouteSelection};
pub(crate) const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";

#[derive(Clone, Copy, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct QuotaUpdate {
    pub(crate) weekly_used_percent: u8,
    pub(crate) weekly_reset_at_unix: Option<i64>,
    pub(crate) routing_used_percent: u8,
    pub(crate) routing_reset_at_unix: Option<i64>,
}

impl QuotaUpdate {
    pub(crate) fn from_event(event: &serde_json::Value) -> Option<Self> {
        use serde_json::Value;
        if event["type"] != "codex.rate_limits" {
            return None;
        }
        let limits = event.get("rate_limits")?;
        let windows = [limits.get("primary"), limits.get("secondary")]
            .into_iter()
            .flatten()
            .filter_map(|window| {
                let used = window.get("used_percent")?.as_f64()?;
                used.is_finite().then_some((window, used))
            })
            .collect::<Vec<_>>();
        let (weekly, weekly_used) = windows.iter().copied().find(|(window, _)| {
            window
                .get("window_minutes")
                .and_then(Value::as_u64)
                .is_some_and(|minutes| minutes.abs_diff(7 * 24 * 60) <= 7 * 24 * 3)
        })?;
        let (routing, routing_used) = windows
            .iter()
            .copied()
            .max_by(|(_, left), (_, right)| left.total_cmp(right))
            .unwrap_or((weekly, weekly_used));
        Some(Self {
            weekly_used_percent: weekly_used.clamp(0.0, 100.0).round() as u8,
            weekly_reset_at_unix: weekly.get("reset_at").and_then(Value::as_i64),
            routing_used_percent: routing_used.clamp(0.0, 100.0).round() as u8,
            routing_reset_at_unix: routing.get("reset_at").and_then(Value::as_i64),
        })
    }
}
