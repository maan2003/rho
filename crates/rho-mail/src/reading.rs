//! Derive the recap from the thread's version history and user activity.

use std::collections::BTreeMap;

use rho_agent_types::{
    Address, ChatRecord, ChatSeq, ContentPart, Mention, Message, MessageId, Revision,
    THREAD_COLD_AFTER, UnixMs, recap_section,
};

fn project_mail(mail: &Message) -> bool {
    !matches!(mail.from, Address::User)
        && !mail
            .mentions
            .iter()
            .any(|mention| matches!(mention, Mention::Direct(_)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Recap {
    version: ChatSeq,
    at: UnixMs,
    text: String,
}

/// Only the recap excerpts are needed from old versions. Their eligibility
/// is derived from the mail and verdicts when read, not recorded by a view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Recaps(BTreeMap<MessageId, Vec<Recap>>);

impl Recaps {
    pub(crate) fn observe(&mut self, mail: &Message, event: &ChatRecord) {
        if matches!(
            mail.revision,
            Revision::Deleted | Revision::TakenBack { .. }
        ) {
            self.0.remove(&mail.id);
            return;
        }
        if !matches!(mail.from, Address::Agent(_)) || !project_mail(mail) {
            return;
        }
        let body = body(&mail.content);
        if let Some(range) = recap_section(&body) {
            self.0.entry(mail.id).or_default().push(Recap {
                version: event.seq,
                at: event.at,
                text: body[range].trim_end().to_owned(),
            });
        }
    }

    /// SPEC-mail-recap-anchor: only a recap supplied while cold qualifies.
    /// Later activity must not hide it, and a warm edit cannot replace it.
    pub(crate) fn anchor<'a>(
        &self,
        history: impl Iterator<Item = &'a Message> + Clone,
        verdicts: impl Iterator<Item = UnixMs>,
    ) -> Option<(MessageId, &str)> {
        let mut user = history
            .clone()
            .filter(|mail| mail.from == Address::User)
            .map(|mail| (mail.at.0, mail.id.0))
            .chain(verdicts.map(|at| (at.0, ChatSeq::ZERO)))
            .collect::<Vec<_>>();
        user.sort_unstable();
        history
            .filter_map(|mail| {
                self.0.get(&mail.id)?.iter().rev().find_map(|recap| {
                    let before = user.partition_point(|&mark| mark < (recap.at.0, recap.version));
                    let cold = before == 0
                        || recap.at.0 - user[before - 1].0 > THREAD_COLD_AFTER.as_millis() as u64;
                    cold.then_some((mail.id, recap))
                })
            })
            .max_by_key(|(_, recap)| recap.version)
            .map(|(mail, recap)| (mail, recap.text.as_str()))
    }
}

/// A message's text, with each picture named.
pub(crate) fn body(content: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in content {
        match part {
            ContentPart::Text { text: words } => text.push_str(words),
            ContentPart::Image { media_type, .. } => {
                text.push_str(&format!("[image · {media_type}]"))
            }
        }
        text.push('\n');
    }
    text.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentId, AgentIdDomain, ChatEvent, ContentPart};

    use super::*;
    use crate::HostMail;

    fn id(n: u64) -> MessageId {
        MessageId(ChatSeq(n))
    }

    fn at(minutes: u64) -> UnixMs {
        UnixMs(minutes * 60_000)
    }

    fn project() -> Address {
        Address::Agent(AgentId::from_counter(1, &AgentIdDomain(7)).unwrap())
    }

    fn post(log: &mut HostMail, n: u64, at: UnixMs, from: Address, text: &str) {
        log.tell(&ChatRecord {
            seq: id(n).0,
            at,
            event: ChatEvent::Posted {
                parent: (n != 1).then_some(id(1)),
                from,
                reply_to: None,
                mentions: Vec::new(),
                content: vec![ContentPart::Text { text: text.into() }],
            },
        });
    }

    #[test]
    fn the_new_cold_arrival_keeps_its_recap_after_reply_and_warm_revision() {
        let mut mail = HostMail::default();
        post(&mut mail, 1, at(10), Address::User, "Start here");
        for n in 2..=11 {
            post(
                &mut mail,
                n,
                at(60 + n),
                project(),
                "Old body\n\n## recap\nMarch",
            );
        }
        let today = 60 * 24 * 240;
        post(
            &mut mail,
            12,
            at(today),
            project(),
            "New body\n\n## recap\nToday",
        );
        post(&mut mail, 13, at(today + 1), Address::User, "Go ahead");
        post(
            &mut mail,
            14,
            at(today + 3),
            project(),
            "Reply\n\n## recap\nWarm",
        );
        let revise = |log: &mut HostMail, n, message, at, text: &str| {
            let Address::Agent(author) = project() else {
                unreachable!()
            };
            log.tell(&ChatRecord {
                seq: id(n).0,
                at,
                event: ChatEvent::Revised {
                    message: id(message),
                    author,
                    ping: None,
                    content: vec![ContentPart::Text { text: text.into() }],
                },
            });
        };
        revise(
            &mut mail,
            15,
            12,
            at(today + 4),
            "Body changed without a recap",
        );
        assert_eq!(
            mail.recap(id(1), std::iter::empty()),
            Some((id(12), "## recap\nToday"))
        );

        let verdict = at(today + 20);
        revise(
            &mut mail,
            16,
            14,
            at(today + 30),
            "Boundary edit\n\n## recap\nWarm at ten minutes",
        );
        assert_eq!(
            mail.recap(id(1), [verdict].into_iter()),
            Some((id(12), "## recap\nToday"))
        );
        revise(
            &mut mail,
            17,
            14,
            UnixMs(at(today + 30).0 + 1),
            "Cold edit\n\n## recap\nNew stretch",
        );
        assert_eq!(
            mail.recap(id(1), [verdict].into_iter()),
            Some((id(14), "## recap\nNew stretch"))
        );
        post(&mut mail, 18, at(today + 31), project(), "No recap here");
        assert_eq!(
            mail.recap(id(1), [verdict].into_iter()),
            Some((id(14), "## recap\nNew stretch"))
        );
    }
}
