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
