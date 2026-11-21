//! The settings owned by a project on its mail host.

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::{MessageId, one_line};

/// The root mail of a project, in its host's log.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct ProjectId(pub MessageId);

/// A repository address or a directory on the host, as the user wrote it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ProjectPlace(String);

impl ProjectPlace {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let value = value.trim();
        if value.is_empty() {
            return Err("enter a repository address or directory");
        }
        if value.chars().any(char::is_control) {
            return Err("the place must be one line");
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_repository(&self) -> bool {
        self.0.starts_with("git@")
            || self.0.split_once("://").is_some_and(|(scheme, _)| {
                !scheme.is_empty()
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
            })
    }

    /// The place's short name in the Desk's project heading.
    pub fn label(&self) -> &str {
        let place = self.0.trim_end_matches('/');
        let tail = if self.is_repository() {
            place.rsplit(['/', ':']).next().unwrap_or(place)
        } else {
            place.rsplit('/').next().unwrap_or(place)
        };
        if tail.is_empty() {
            self.as_str()
        } else if self.is_repository() {
            tail.strip_suffix(".git").unwrap_or(tail)
        } else {
            tail
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ProjectMode {
    One,
    PerIdea,
}

impl ProjectMode {
    pub const ALL: [Self; 2] = [Self::One, Self::PerIdea];

    pub fn label(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::PerIdea => "per idea",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ProjectConfig {
    pub place: ProjectPlace,
    pub mode: ProjectMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ProjectChange {
    Place(ProjectPlace),
    Mode(ProjectMode),
}

/// A validated name without the client-owned host alias.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ProjectName(String);

impl ProjectName {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let name = one_line(value);
        if name.is_empty() || name.contains(['@', '|']) {
            return Err("enter a project name without @ or |");
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_can_have_a_git_suffix_or_colon_without_becoming_a_repository() {
        for (input, repository, label) in [
            ("git@github.com:owner/tide.git", true, "tide"),
            ("ssh://box:2222/owner/strand.git/", true, "strand"),
            ("/src/box:reef.git/", false, "box:reef.git"),
            ("/", false, "/"),
        ] {
            let place = ProjectPlace::parse(input).unwrap();
            assert_eq!(place.is_repository(), repository, "{input}");
            assert_eq!(place.label(), label, "{input}");
        }
        assert!(ProjectPlace::parse("/one\n/two").is_err());
    }
}
