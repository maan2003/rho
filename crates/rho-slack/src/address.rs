//! What a `slack:` address names, and the way back from one.
//!
//! The grammar is [SPEC-slack-addresses](../specs/SPEC-slack-addresses.md):
//! the workspace, then the conversation, then the thread. The ledger keeps
//! the user's facts about a conversation or a thread under these keys, so
//! the form a unit takes here is the form it has always been written in.

use rho_dealer::Addressed;

use crate::config::WorkspaceName;
use crate::model::Unit;
use crate::session::Source;
use crate::types::{ChannelId, Ts};

/// One `slack:` address.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SlackAddress {
    /// `slack:<workspace>/<channel>` or `slack:<workspace>/<channel>/<ts>`:
    /// a conversation or a thread in it, the thing the dealer deals.
    Unit(WorkspaceName, Unit),
    /// `slack:<workspace>/~<listing>`: places gathered from the workspace.
    Listing(WorkspaceName, Listing),
}

/// The listings a workspace has: the last search of each kind. Each has one
/// surface, and asking again draws over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Listing {
    /// The last message search.
    Messages,
    /// The last file search.
    Files,
}

impl Listing {
    const ALL: [Listing; 2] = [Listing::Messages, Listing::Files];

    fn word(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::Files => "files",
        }
    }
}

impl SlackAddress {
    pub fn workspace(&self) -> &WorkspaceName {
        match self {
            Self::Unit(workspace, _) | Self::Listing(workspace, _) => workspace,
        }
    }

    /// The address a conversation surface reads.
    pub fn of_source(workspace: &WorkspaceName, source: &Source) -> Self {
        Self::Unit(workspace.clone(), unit_of(source))
    }
}

/// The unit a surface reads: the conversation, or the thread.
pub fn unit_of(source: &Source) -> Unit {
    match source {
        Source::Conversation(channel) => Unit::conversation(channel),
        Source::Thread(key) => Unit::thread(&key.channel, &key.thread_ts),
    }
}

/// What a channel id or a timestamp may be: Slack's own are letters and
/// digits, and a timestamp is seconds, a dot and a sequence.
fn is_id(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_ts(text: &str) -> bool {
    match text.split_once('.') {
        Some((seconds, sequence)) => {
            !seconds.is_empty()
                && seconds.chars().all(|c| c.is_ascii_digit())
                && sequence.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

impl Addressed for SlackAddress {
    const KIND: &'static str = "slack";

    fn rest(&self) -> String {
        match self {
            Self::Unit(workspace, unit) => match &unit.thread {
                None => format!("{workspace}/{}", unit.channel.as_str()),
                Some(ts) => format!("{workspace}/{}/{}", unit.channel.as_str(), ts.as_str()),
            },
            Self::Listing(workspace, listing) => format!("{workspace}/~{}", listing.word()),
        }
    }

    fn from_rest(rest: &str) -> Option<Self> {
        let mut parts = rest.split('/');
        let workspace = parts.next().filter(|name| !name.is_empty())?;
        let workspace = WorkspaceName(workspace.to_owned());
        let second = parts.next();
        let third = parts.next();
        if parts.next().is_some() {
            return None;
        }
        Some(match (second, third) {
            (None, _) => return None,
            (Some(word), None) if word.starts_with('~') => {
                let listing = Listing::ALL
                    .into_iter()
                    .find(|listing| word[1..] == *listing.word())?;
                Self::Listing(workspace, listing)
            }
            (Some(channel), thread) => {
                if !is_id(channel) || thread.is_some_and(|ts| !is_ts(ts)) {
                    return None;
                }
                let channel = ChannelId(channel.to_owned());
                Self::Unit(
                    workspace,
                    match thread {
                        None => Unit::conversation(&channel),
                        Some(ts) => Unit::thread(&channel, &Ts(ts.to_owned())),
                    },
                )
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use rho_dealer::Address;

    use super::*;

    fn acme() -> WorkspaceName {
        WorkspaceName("acme".into())
    }

    /// The keys rho has written facts under since Slack units were nodes:
    /// they must read back as the same units, or every snooze and done on
    /// a Slack thread is lost.
    #[test]
    fn a_unit_reads_back_from_the_key_its_facts_are_under() {
        let thread = Address::parse("slack:acme/C2/1700000000.000100").unwrap();
        assert_eq!(
            SlackAddress::from_address(&thread),
            Some(SlackAddress::Unit(
                acme(),
                Unit::thread(&ChannelId("C2".into()), &Ts("1700000000.000100".into()))
            ))
        );
        let room = SlackAddress::Unit(acme(), Unit::conversation(&ChannelId("D1".into())));
        assert_eq!(room.address().key(), "slack:acme/D1");
        assert_eq!(SlackAddress::from_address(&room.address()), Some(room));
    }

    #[test]
    fn the_listings_read_back() {
        for address in [
            SlackAddress::Listing(acme(), Listing::Messages),
            SlackAddress::Listing(acme(), Listing::Files),
        ] {
            assert_eq!(
                SlackAddress::from_address(&address.address()),
                Some(address)
            );
        }
        assert_eq!(
            SlackAddress::from_rest("acme/~messages"),
            Some(SlackAddress::Listing(acme(), Listing::Messages))
        );
    }

    #[test]
    fn what_slack_could_not_have_made_does_not_read() {
        for rest in [
            "",
            "/C1",
            "acme/",
            "acme/~nothing",
            "acme/C1/",
            "acme/C1/not-a-ts",
            "acme/C1/1700000000.1/more",
            "acme/~activity/1700000000.1",
            // The screens that went: the conversation list, activity and the
            // list saved on one device. An old history entry naming one reads
            // as nothing rather than as a screen that is not there.
            "acme",
            "acme/~activity",
            "acme/~saved",
        ] {
            assert_eq!(SlackAddress::from_rest(rest), None, "{rest:?}");
        }
    }
}
