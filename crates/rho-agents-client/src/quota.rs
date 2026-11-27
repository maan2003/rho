//! What each host says about its model accounts: which auth namespaces
//! it has and which is active, and how much quota is left on each. Kept
//! per host, and merged into one view where the user sees it.

use std::collections::HashMap;

use rho_agent_hosts::{HostId, Hosts};

use crate::map::AgentMap;
use crate::protocol::{AuthState, QuotaSeries, QuotaSummary};

/// What a merge needs of the attached hosts: each one's id and the name
/// the user gave it, in a stable order. The frame's [`Hosts`] and the
/// agents' [`AgentMap`] both know it.
pub trait HostNames {
    fn names(&self) -> Vec<(HostId, String)>;
}

impl HostNames for Hosts {
    fn names(&self) -> Vec<(HostId, String)> {
        self.iter()
            .map(|host| (host.id, host.name.clone()))
            .collect()
    }
}

impl HostNames for AgentMap {
    fn names(&self) -> Vec<(HostId, String)> {
        self.hosts()
            .map(|(host, name)| (host, name.to_owned()))
            .collect()
    }
}

fn label_of(names: &[(HostId, String)], host: HostId) -> &str {
    names
        .iter()
        .find(|(id, _)| *id == host)
        .map_or("", |(_, name)| name.as_str())
}

#[derive(Default)]
pub struct Quotas {
    auth: HashMap<HostId, AuthState>,
    summaries: HashMap<HostId, Vec<QuotaSummary>>,
    history: HashMap<HostId, Vec<QuotaSeries>>,
}

impl Quotas {
    pub fn set_auth(&mut self, host: HostId, auth: AuthState) {
        self.auth.insert(host, auth);
    }

    pub fn auth(&self, host: HostId) -> Option<&AuthState> {
        self.auth.get(&host)
    }

    pub fn set_summaries(&mut self, host: HostId, summaries: Vec<QuotaSummary>) {
        self.summaries.insert(host, summaries);
    }

    /// What `host` last said is left of each allowance; nothing until it
    /// has said.
    pub fn summaries(&self, host: HostId) -> &[QuotaSummary] {
        self.summaries.get(&host).map_or(&[], Vec::as_slice)
    }

    pub fn set_history(&mut self, host: HostId, series: Vec<QuotaSeries>) {
        self.history.insert(host, series);
    }

    /// Drops what a detached host said.
    pub fn forget(&mut self, host: HostId) {
        self.auth.remove(&host);
        self.summaries.remove(&host);
        self.history.remove(&host);
    }

    /// Whether any host has answered for its quota history yet.
    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn merged_summaries(&self, hosts: &impl HostNames) -> Vec<QuotaSummary> {
        let names = hosts.names();
        let mut merged: Vec<QuotaSummary> = Vec::new();
        for (host, summaries) in &self.summaries {
            for summary in summaries {
                let Some(namespace) = &summary.auth_namespace else {
                    match merged.iter_mut().find(|existing| {
                        existing.model == summary.model && existing.auth_namespace.is_none()
                    }) {
                        Some(existing)
                            if summary.remaining_percent < existing.remaining_percent =>
                        {
                            *existing = summary.clone();
                        }
                        Some(_) => {}
                        None => merged.push(summary.clone()),
                    }
                    continue;
                };
                let mut summary = summary.clone();
                if names.len() > 1 {
                    summary.auth_namespace =
                        Some(format!("{}/{}", label_of(&names, *host), namespace));
                }
                merged.push(summary);
            }
        }
        merged.sort_by(|a, b| (&a.model, &a.auth_namespace).cmp(&(&b.model, &b.auth_namespace)));
        // An unnamed legacy entry and a named namespace can describe the
        // same account; showing identical numbers twice says nothing.
        merged.dedup_by(|a, b| {
            a.model == b.model
                && a.remaining_percent == b.remaining_percent
                && a.reset_at_unix == b.reset_at_unix
        });
        merged
    }

    /// Every host's allowances, kept separate by account. A configured GPT
    /// account that has not reported yet can still be named beside the ones
    /// that have, but don't treat a host's unanswered request as a report.
    pub fn merged_history(&self, hosts: &impl HostNames) -> Vec<QuotaSeries> {
        let names = hosts.names();
        let qualify = names.len() > 1;
        let mut merged = Vec::new();
        for (host, name) in names {
            let Some(history) = self.history.get(&host) else {
                continue;
            };
            let qualified = |namespace: &str| {
                if qualify {
                    format!("{name}/{namespace}")
                } else {
                    namespace.to_owned()
                }
            };
            merged.extend(history.iter().filter_map(|series| {
                let mut series = series.clone();
                series.auth_namespace = match &series.auth_namespace {
                    Some(namespace) => Some(qualified(namespace)),
                    None if series.model == "gpt" => return None,
                    None if qualify => Some(name.clone()),
                    None => None,
                };
                Some(series)
            }));
            if let Some(auth) = self.auth.get(&host) {
                merged.extend(auth.namespaces.iter().filter_map(|namespace| {
                    (!history.iter().any(|series| {
                        series.model == "gpt" && series.auth_namespace.as_ref() == Some(namespace)
                    }))
                    .then(|| QuotaSeries {
                        model: "gpt".to_owned(),
                        auth_namespace: Some(qualified(namespace)),
                        points: Vec::new(),
                    })
                }));
            }
        }
        merged.sort_by(|a, b| (&a.model, &a.auth_namespace).cmp(&(&b.model, &b.auth_namespace)));
        merged
    }

    /// The account each host's agents run on, with the host named when the
    /// lines carry host names.
    pub fn active_accounts(&self, hosts: &impl HostNames) -> Vec<ActiveAccount> {
        let names = hosts.names();
        let qualify = names.len() > 1;
        names
            .iter()
            .filter_map(|(host, name)| {
                let namespace = self.auth.get(host)?.active_namespace.as_ref()?;
                Some(ActiveAccount {
                    host: qualify.then(|| name.clone()),
                    namespace: namespace.clone(),
                })
            })
            .collect()
    }
}

/// The GPT account a host's agents run on. `host` is named only when more
/// than one host is attached, and then the account's line is named
/// `host/namespace`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveAccount {
    pub host: Option<String>,
    pub namespace: String,
}

impl ActiveAccount {
    /// The name `merged_history` gives this account's line.
    pub fn line_name(&self) -> String {
        match &self.host {
            Some(host) => format!("{host}/{}", self.namespace),
            None => self.namespace.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::QuotaPoint;

    struct Named(Vec<(HostId, String)>);

    impl HostNames for Named {
        fn names(&self) -> Vec<(HostId, String)> {
            self.0.clone()
        }
    }

    fn auth(namespaces: &[&str], active: Option<&str>) -> AuthState {
        AuthState {
            namespaces: namespaces.iter().map(|name| (*name).to_owned()).collect(),
            disabled_namespaces: Vec::new(),
            active_namespace: active.map(str::to_owned),
        }
    }

    fn gpt(namespace: &str) -> QuotaSeries {
        QuotaSeries {
            model: "gpt".to_owned(),
            auth_namespace: Some(namespace.to_owned()),
            points: vec![QuotaPoint {
                observed_at_ms: 1,
                remaining_percent: 50,
                reset_at_unix: None,
            }],
        }
    }

    const LAB: HostId = HostId(1);
    const DESK: HostId = HostId(2);

    #[test]
    fn each_claude_account_keeps_its_own_history() {
        let hosts = Named(vec![(LAB, "lab".to_owned()), (DESK, "desk".to_owned())]);
        let mut quotas = Quotas::default();
        let opus = |host, name: &str, remaining_percent| QuotaSeries {
            model: "opus".to_owned(),
            auth_namespace: Some(name.to_owned()),
            points: vec![QuotaPoint {
                observed_at_ms: host,
                remaining_percent,
                reset_at_unix: None,
            }],
        };
        quotas.set_history(LAB, vec![opus(1, "work", 32), opus(1, "home", 75)]);
        quotas.set_history(DESK, vec![opus(2, "work", 52)]);

        let series = quotas.merged_history(&hosts);
        let summary = crate::usage::quota_summary(&series, &[], 7, 3, 20);
        let labels = summary
            .legend
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "opus/desk/work  52% left · reset time not reported",
                "opus/lab/home  75% left · reset time not reported",
                "opus/lab/work  32% left · reset time not reported",
            ]
        );
        assert_eq!(summary.lines.len(), 3);
        assert_ne!(summary.lines[0].color, summary.lines[1].color);
    }

    #[test]
    fn each_hosts_active_account_is_starred_with_its_host() {
        let hosts = Named(vec![(LAB, "lab".to_owned()), (DESK, "desk".to_owned())]);
        let mut quotas = Quotas::default();
        quotas.set_auth(LAB, auth(&["work", "home", "fresh"], Some("work")));
        quotas.set_auth(DESK, auth(&["work"], None));
        quotas.set_history(LAB, vec![gpt("home"), gpt("work")]);
        quotas.set_history(DESK, vec![gpt("work")]);

        let series = quotas.merged_history(&hosts);
        let active = quotas.active_accounts(&hosts);
        let summary = crate::usage::quota_summary(&series, &active, 7, 2, 100);
        let labels = summary
            .legend
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "gpt/desk/work  50% left · reset time not reported",
                "gpt/lab/fresh  allowance not reported",
                "gpt/lab/home  50% left · reset time not reported",
                "★ lab · gpt/work  50% left · reset time not reported",
            ]
        );
    }
}
