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

    /// The names of the hosts that answered for their history, when every
    /// one of them answered with nothing because it has no account: what
    /// the rate-limit chart says in place of an empty grid. `None` when any
    /// of them has an account, a sample, or has not said which accounts it
    /// has, since then the empty grid only means nothing was read yet.
    pub fn without_accounts(&self, hosts: &impl HostNames) -> Option<Vec<String>> {
        let names = hosts
            .names()
            .into_iter()
            .filter(|(host, _)| self.history.contains_key(host))
            .map(|(host, name)| {
                let empty = self.history[&host].is_empty()
                    && self
                        .auth
                        .get(&host)
                        .is_some_and(|auth| auth.namespaces.is_empty());
                empty.then_some(name)
            })
            .collect::<Option<Vec<_>>>()?;
        (!names.is_empty()).then_some(names)
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

    /// ChatGPT history is one line per host/namespace. Claude history keeps
    /// the previous tightest-host merge because it has no named auth scope.
    pub fn merged_history(&self, hosts: &impl HostNames) -> Vec<QuotaSeries> {
        let names = hosts.names();
        let mut merged: Vec<QuotaSeries> = Vec::new();
        for (host, series_set) in &self.history {
            for series in series_set {
                if series.model == "gpt" {
                    let Some(namespace) = &series.auth_namespace else {
                        continue;
                    };
                    let mut series = series.clone();
                    if names.len() > 1 {
                        series.auth_namespace =
                            Some(format!("{}/{}", label_of(&names, *host), namespace));
                    }
                    merged.push(series);
                    continue;
                }
                let Some(existing) = merged
                    .iter_mut()
                    .find(|existing| existing.model == series.model)
                else {
                    merged.push(series.clone());
                    continue;
                };
                for point in &series.points {
                    match existing
                        .points
                        .iter_mut()
                        .find(|candidate| candidate.observed_at_ms == point.observed_at_ms)
                    {
                        Some(candidate)
                            if point.remaining_percent < candidate.remaining_percent =>
                        {
                            *candidate = *point;
                        }
                        Some(_) => {}
                        None => existing.points.push(*point),
                    }
                }
                existing.points.sort_by_key(|point| point.observed_at_ms);
            }
        }
        merged.sort_by(|a, b| (&a.model, &a.auth_namespace).cmp(&(&b.model, &b.auth_namespace)));
        merged
    }

    pub fn active_namespaces(&self, hosts: &impl HostNames) -> Vec<String> {
        let names = hosts.names();
        let qualify = names.len() > 1;
        names
            .iter()
            .filter_map(|(host, name)| {
                let namespace = self.auth.get(host)?.active_namespace.as_ref()?;
                Some(if qualify {
                    format!("{name}/{namespace}")
                } else {
                    namespace.clone()
                })
            })
            .collect()
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
    fn the_hosts_without_accounts_are_named_only_when_none_has_one() {
        let hosts = Named(vec![(LAB, "lab".to_owned()), (DESK, "desk".to_owned())]);
        let mut quotas = Quotas::default();
        quotas.set_history(LAB, Vec::new());
        assert_eq!(
            quotas.without_accounts(&hosts),
            None,
            "lab has not said which accounts it has"
        );

        quotas.set_auth(LAB, auth(&[], None));
        assert_eq!(
            quotas.without_accounts(&hosts),
            Some(vec!["lab".to_owned()])
        );

        quotas.set_auth(DESK, auth(&[], None));
        quotas.set_history(DESK, Vec::new());
        assert_eq!(
            quotas.without_accounts(&hosts),
            Some(vec!["lab".to_owned(), "desk".to_owned()])
        );

        quotas.set_auth(DESK, auth(&["work"], None));
        assert_eq!(
            quotas.without_accounts(&hosts),
            None,
            "desk has an account it has not read yet"
        );
    }

    #[test]
    fn the_active_account_is_named_the_way_its_line_is() {
        let hosts = Named(vec![(LAB, "lab".to_owned()), (DESK, "desk".to_owned())]);
        let mut quotas = Quotas::default();
        quotas.set_auth(LAB, auth(&["work", "home"], Some("work")));
        quotas.set_auth(DESK, auth(&["work"], None));
        quotas.set_history(LAB, vec![gpt("home"), gpt("work")]);
        quotas.set_history(DESK, vec![gpt("work")]);

        let series = quotas.merged_history(&hosts);
        let active = quotas.active_namespaces(&hosts);
        let summary = crate::usage::quota_summary(&series, &active, 7, 2, 100);
        let labels = summary
            .legend
            .iter()
            .map(|entry| entry.label.split(' ').take(2).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            ["gpt/desk/work 50%", "gpt/lab/home 50%", "★ gpt/lab/work"]
        );
    }
}
