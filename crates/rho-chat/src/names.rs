//! What the window calls a project (`chat.Project`): its bare name, such
//! as `tide`, while no other host the window holds has a project of that
//! name, and `tide@lab` with the host's alias once two do. Names compare
//! without case, as the user types them.
//!
//! The user may always write the qualified form; a bare name answers
//! only while it is unique.

use std::collections::{HashMap, HashSet};

use crate::fold::HostChat;

/// The project names more than one held host uses, read off the hosts'
/// logs whenever a name is shown.
#[derive(Debug, Default)]
pub struct ProjectNames {
    shared: HashSet<String>,
}

impl ProjectNames {
    pub fn new<'a>(hosts: impl IntoIterator<Item = &'a HostChat>) -> Self {
        let mut hosts_with: HashMap<String, usize> = HashMap::new();
        for chat in hosts {
            let names: HashSet<String> =
                chat.projects().map(|project| fold(&project.name)).collect();
            for name in names {
                *hosts_with.entry(name).or_default() += 1;
            }
        }
        Self {
            shared: hosts_with
                .into_iter()
                .filter_map(|(name, hosts)| (hosts > 1).then_some(name))
                .collect(),
        }
    }

    /// Whether another held host has a project of this name too.
    pub fn is_shared(&self, name: &str) -> bool {
        self.shared.contains(&fold(name))
    }

    /// The project's address as the window shows it.
    pub fn address(&self, name: &str, host_alias: &str) -> String {
        if self.is_shared(name) {
            qualified(name, host_alias)
        } else {
            name.to_owned()
        }
    }
}

/// The address that names the host, `tide@lab`, which always answers.
pub fn qualified(name: &str, host_alias: &str) -> String {
    format!("{name}@{host_alias}")
}

/// A typed address as its name and, when written, its host's alias. A
/// project name never holds `@`, so the last one splits them.
pub fn split(address: &str) -> (&str, Option<&str>) {
    match address.rsplit_once('@') {
        Some((name, alias)) => (name.trim_end(), Some(alias.trim_start())),
        None => (address, None),
    }
}

fn fold(name: &str) -> String {
    name.to_lowercase()
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{
        Address, ChatEvent, ChatRecord, ChatSeq, ContentPart, MessageId, ProjectConfig, ProjectId,
        ProjectMode, ProjectPlace, RuntimeLabel, UnixMs, Voice,
    };

    use super::*;

    fn host(projects: &[&str]) -> HostChat {
        let mut chat = HostChat::default();
        let mut seq = 0;
        for name in projects {
            seq += 1;
            let root = MessageId(ChatSeq(seq));
            chat.tell(&ChatRecord {
                seq: root.0,
                at: UnixMs(seq),
                event: ChatEvent::Posted {
                    parent: None,
                    retry_of: None,
                    thread_reply: None,
                    from: Address::User,
                    voice: Voice::Message,
                    content: vec![ContentPart::Text {
                        text: (*name).into(),
                    }],
                },
            });
            seq += 1;
            chat.tell(&ChatRecord {
                seq: ChatSeq(seq),
                at: UnixMs(seq),
                event: ChatEvent::Project {
                    project: ProjectId(root),
                    config: ProjectConfig {
                        place: ProjectPlace::parse("/srv").unwrap(),
                        mode: ProjectMode::One,
                        runtime: RuntimeLabel::default(),
                    },
                },
            });
        }
        chat
    }

    #[test]
    fn a_name_one_host_has_twice_is_still_bare() {
        let lab = host(&["tide", "reef", "tide"]);
        let names = ProjectNames::new([&lab]);
        assert_eq!(names.address("tide", "lab"), "tide");
        assert_eq!(names.address("reef", "lab"), "reef");
    }

    #[test]
    fn only_a_name_two_hosts_share_names_its_host() {
        let lab = host(&["tide", "reef"]);
        let other = host(&["Tide", "skiff"]);
        let names = ProjectNames::new([&lab, &other]);
        assert_eq!(names.address("tide", "lab"), "tide@lab");
        assert_eq!(names.address("Tide", "box"), "Tide@box");
        assert_eq!(names.address("reef", "lab"), "reef");
        assert_eq!(names.address("skiff", "box"), "skiff");
        // A non-project root of the same name does not collide.
        let mut plain = HostChat::default();
        plain.tell(&ChatRecord {
            seq: ChatSeq(1),
            at: UnixMs(1),
            event: ChatEvent::Posted {
                parent: None,
                retry_of: None,
                thread_reply: None,
                from: Address::User,
                voice: Voice::Message,
                content: vec![ContentPart::Text {
                    text: "reef".into(),
                }],
            },
        });
        assert_eq!(
            ProjectNames::new([&lab, &plain]).address("reef", "lab"),
            "reef"
        );
    }

    #[test]
    fn a_typed_address_splits_at_its_last_at() {
        assert_eq!(split("tide"), ("tide", None));
        assert_eq!(split("tide @ lab"), ("tide", Some("lab")));
        assert_eq!(split("tide@"), ("tide", Some("")));
        assert_eq!(qualified("tide", "lab"), "tide@lab");
    }
}
