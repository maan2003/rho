use std::collections::HashMap;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RoleDetails {
    effort: Option<String>,
    verbosity: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RoleCycleKind {
    InnerGroup,
    Group,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RoleCycleOutcome {
    Selected(String),
    NoRolesAvailable,
    Noop,
}

#[derive(Default)]
pub(crate) struct RoleState {
    roles: Vec<String>,
    groups: Vec<tau_proto::HarnessRoleGroup>,
    group_memory: HashMap<String, String>,
    details: HashMap<String, RoleDetails>,
}

impl RoleState {
    pub(crate) fn update_available(&mut self, roles: &tau_proto::HarnessRolesAvailable) {
        self.roles = roles.roles.iter().map(|role| role.name.clone()).collect();
        self.groups = roles.groups.clone();
        self.details = roles
            .roles
            .iter()
            .map(|role| {
                (
                    role.name.clone(),
                    RoleDetails::from_description(&role.description),
                )
            })
            .collect();
    }

    pub(crate) fn cycle_role(
        &mut self,
        current: Option<&str>,
        kind: RoleCycleKind,
    ) -> RoleCycleOutcome {
        match kind {
            RoleCycleKind::InnerGroup => {
                if self.groups.is_empty() {
                    RoleCycleOutcome::Noop
                } else {
                    self.cycle_role_in_groups(current, true)
                }
            }
            RoleCycleKind::Group => {
                if self.groups.is_empty() {
                    self.cycle_role_flat(current)
                } else {
                    self.cycle_role_in_groups(current, false)
                }
            }
        }
    }

    pub(crate) fn default_effort(&self, role: Option<&str>) -> Option<tau_proto::Effort> {
        self.details.get(role?)?.effort.as_deref()?.parse().ok()
    }

    pub(crate) fn default_verbosity(&self, role: Option<&str>) -> Option<tau_proto::Verbosity> {
        self.details.get(role?)?.verbosity.as_deref()?.parse().ok()
    }

    fn cycle_role_flat(&self, current: Option<&str>) -> RoleCycleOutcome {
        if self.roles.is_empty() {
            return RoleCycleOutcome::NoRolesAvailable;
        }
        let next =
            match current.and_then(|current| self.roles.iter().position(|role| role == current)) {
                Some(index) => self.roles[(index + 1) % self.roles.len()].clone(),
                None => self.roles[0].clone(),
            };
        RoleCycleOutcome::Selected(next)
    }

    fn cycle_role_in_groups(&mut self, current: Option<&str>, alternate: bool) -> RoleCycleOutcome {
        if self.groups.is_empty() {
            return RoleCycleOutcome::NoRolesAvailable;
        }
        remember_group_role(&mut self.group_memory, &self.groups, current);
        let Some(next) = next_role_in_groups(current, &self.groups, alternate, &self.group_memory)
        else {
            return RoleCycleOutcome::NoRolesAvailable;
        };
        remember_group_role(&mut self.group_memory, &self.groups, Some(&next));
        RoleCycleOutcome::Selected(next)
    }
}

fn remember_group_role(
    memory: &mut HashMap<String, String>,
    groups: &[tau_proto::HarnessRoleGroup],
    role: Option<&str>,
) {
    let Some(role) = role else {
        return;
    };
    if let Some(group) = groups
        .iter()
        .find(|group| group.roles.iter().any(|candidate| candidate == role))
    {
        memory.insert(group.name.clone(), role.to_owned());
    }
}

fn next_role_in_groups(
    current: Option<&str>,
    groups: &[tau_proto::HarnessRoleGroup],
    alternate: bool,
    memory: &HashMap<String, String>,
) -> Option<String> {
    let current_pos = current.and_then(|current| {
        groups.iter().enumerate().find_map(|(group_index, group)| {
            group
                .roles
                .iter()
                .position(|role| role == current)
                .map(|role_index| (group_index, role_index))
        })
    });
    if alternate {
        let (group_index, role_index) = current_pos.unwrap_or((0, 0));
        let roles = groups.get(group_index)?.roles.as_slice();
        return roles.get((role_index + 1) % roles.len()).cloned();
    }
    let next_group = current_pos.map_or(0, |(group_index, _)| (group_index + 1) % groups.len());
    let group = groups.get(next_group)?;
    memory
        .get(&group.name)
        .filter(|role| group.roles.iter().any(|candidate| candidate == *role))
        .cloned()
        .or_else(|| group.roles.first().cloned())
}
impl RoleDetails {
    fn from_description(description: &str) -> Self {
        let mut details = Self::default();
        if description == "no model" {
            return details;
        }

        for part in description.split(',').map(str::trim) {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key {
                "effort" => details.effort = Some(value.to_owned()),
                "verbosity" => details.verbosity = Some(value.to_owned()),
                _ => {}
            }
        }
        details
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles_available(names: &[&str]) -> tau_proto::HarnessRolesAvailable {
        roles_available_with_groups(names, Vec::new())
    }

    fn roles_available_with_groups(
        names: &[&str],
        groups: Vec<tau_proto::HarnessRoleGroup>,
    ) -> tau_proto::HarnessRolesAvailable {
        tau_proto::HarnessRolesAvailable {
            roles: names
                .iter()
                .map(|name| tau_proto::HarnessRoleInfo {
                    name: (*name).to_owned(),
                    description: "model=test/model".to_owned(),
                    role_description: None,
                    details: None,
                })
                .collect(),
            groups,
            custom_prompts: Vec::new(),
        }
    }

    fn group(name: &str, roles: &[&str]) -> tau_proto::HarnessRoleGroup {
        tau_proto::HarnessRoleGroup {
            name: name.to_owned(),
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
        }
    }

    fn assert_selected(outcome: RoleCycleOutcome, expected: &str) {
        assert_eq!(outcome, RoleCycleOutcome::Selected(expected.to_owned()));
    }

    #[test]
    fn group_cycle_falls_back_to_flat_roles_without_groups() {
        let mut roles = RoleState::default();
        roles.update_available(&roles_available(&["engineer", "reviewer", "writer"]));

        assert_selected(
            roles.cycle_role(Some("engineer"), RoleCycleKind::Group),
            "reviewer",
        );
        assert_selected(
            roles.cycle_role(Some("writer"), RoleCycleKind::Group),
            "engineer",
        );
        assert_selected(roles.cycle_role(None, RoleCycleKind::Group), "engineer");
    }

    #[test]
    fn inner_group_cycle_noops_without_groups() {
        let mut roles = RoleState::default();
        roles.update_available(&roles_available(&["engineer", "reviewer"]));

        assert_eq!(
            roles.cycle_role(Some("engineer"), RoleCycleKind::InnerGroup),
            RoleCycleOutcome::Noop
        );
    }

    #[test]
    fn inner_group_cycle_stays_in_current_group() {
        let mut roles = RoleState::default();
        roles.update_available(&roles_available_with_groups(
            &["engineer", "reviewer", "architect"],
            vec![
                group("primary", &["engineer", "reviewer"]),
                group("special", &["architect"]),
            ],
        ));

        assert_selected(
            roles.cycle_role(Some("engineer"), RoleCycleKind::InnerGroup),
            "reviewer",
        );
        assert_selected(
            roles.cycle_role(Some("reviewer"), RoleCycleKind::InnerGroup),
            "engineer",
        );
    }

    #[test]
    fn group_cycle_remembers_last_role_per_group() {
        let mut roles = RoleState::default();
        roles.update_available(&roles_available_with_groups(
            &["engineer", "reviewer", "architect", "debugger"],
            vec![
                group("primary", &["engineer", "reviewer"]),
                group("special", &["architect", "debugger"]),
            ],
        ));

        assert_selected(
            roles.cycle_role(Some("reviewer"), RoleCycleKind::Group),
            "architect",
        );
        assert_selected(
            roles.cycle_role(Some("architect"), RoleCycleKind::InnerGroup),
            "debugger",
        );
        assert_selected(
            roles.cycle_role(Some("debugger"), RoleCycleKind::Group),
            "reviewer",
        );
    }

    #[test]
    fn parses_effort_and_verbosity_defaults_from_role_descriptions() {
        let details = RoleDetails::from_description(
            "model=gpt-5, effort=high, verbosity=low, service-tier=fast",
        );

        assert_eq!(details.effort.as_deref(), Some("high"));
        assert_eq!(details.verbosity.as_deref(), Some("low"));
    }

    #[test]
    fn ignores_no_model_descriptions() {
        let details = RoleDetails::from_description("no model");

        assert_eq!(details, RoleDetails::default());
    }
}
