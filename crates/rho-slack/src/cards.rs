//! What of Slack wants the user: the claims the dealer ranks, and what a
//! Slack address is called on a card.
//!
//! The rule is Slack's own: a unit is a card while Slack would badge it
//! ([`Model::attention`]), and the curve it takes says how much it asks.
//! The cases this answers for are the S rows of the dealer's
//! `cases.md`; each is a test below under its name.

use jiff::Timestamp;
use rho_dealer::curve::{self, Curve};
use rho_dealer::facts::{Seen, slack_ts_order};
use rho_dealer::rank::Trace;
use rho_dealer::{Address, Addressed as _, Claim, Marks};

use crate::address::SlackAddress;
use crate::mirror::{Mirror, message_line, unit_summary};
use crate::model::{Attention, Model, Unit, reason_text};

fn at_ms(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH)
}

/// The address of a unit in `model`'s workspace.
pub fn address(model: &Model, unit: &Unit) -> Address {
    SlackAddress::Unit(model.workspace().clone(), unit.clone()).address()
}

/// Every unit Slack is badging that the user has not said they are done
/// with past its newest message, as a claim.
pub fn claims(
    model: &Model,
    mirror: Option<&Mirror>,
    marks: &Marks,
    mut trace: Option<&mut Trace>,
) -> Vec<Claim> {
    let mut claims = Vec::new();
    for (unit, attention) in model.asking() {
        let Some(facts) = model.unit(unit) else {
            continue;
        };
        let address = address(model, unit);
        // Done on another device, having seen this far (S3, S4).
        let seen = marks.get(&address).facts().seen_slack();
        if let Some(trace) = trace.as_deref_mut() {
            trace.input(&address, "attention", format!("{attention:?}"));
            trace.input(&address, "unit facts", format!("{facts:?}"));
            trace.input(&address, "seen through", format!("{seen:?}"));
        }
        if seen.is_some_and(|seen| slack_ts_order(&facts.newest.0, seen).is_le()) {
            if let Some(trace) = trace.as_deref_mut() {
                trace.outcome(&address, "no card: done through its newest".to_owned());
            }
            continue;
        }
        // The age is the ask's: a mention said two days ago reads `2.0d`
        // however recently rho first heard of it.
        let since = at_ms(facts.asks_with().millis());
        let waiting = |head_start| Curve::Waiting { head_start, since };
        let people = facts.people.len() + usize::from(!facts.people.contains(model.self_id()));
        let small = people <= curve::SLACK_SMALL_THREAD_PEOPLE;
        let (state, curve, direct) = match attention {
            Attention::ChannelTraffic => (
                "unread",
                Curve::Fading {
                    head_start: curve::CHANNEL_TRAFFIC_HEAD_START,
                    since,
                    gone_days: match facts.others_replied {
                        true => curve::CHANNEL_ANSWERED_GONE_DAYS,
                        false => curve::CHANNEL_TRAFFIC_GONE_DAYS,
                    },
                },
                false,
            ),
            Attention::DirectMessage => ("needs reply", waiting(curve::SLACK_DM_HEAD_START), true),
            Attention::Mentioned => (
                "needs reply",
                waiting(curve::SLACK_MENTION_HEAD_START),
                false,
            ),
            // A thread of a few people is a conversation with the user.
            Attention::FollowedThread if small => {
                ("needs reply", waiting(curve::SLACK_DM_HEAD_START), true)
            }
            Attention::FollowedThread => (
                "needs reply",
                waiting(curve::SLACK_THREAD_HEAD_START),
                false,
            ),
        };
        let label = model.label(&unit.channel);
        let why = reason_text(attention);
        let mut claim = Claim::new(
            address,
            curve,
            format!("{why} · {state} · {{age}}"),
            facts.newest.0.clone(),
        );
        claim.title = mirror
            .map(|mirror| unit_summary(model, mirror, unit))
            .unwrap_or_default();
        claim.context = label;
        // A snooze on the room holds every thread in it (S7).
        claim.under = unit
            .thread
            .is_some()
            .then(|| self::address(model, &Unit::conversation(&unit.channel)));
        // What a direct conversation slept through brings it back higher (Z5).
        if direct {
            claim.others = facts
                .from_others
                .iter()
                .map(|ts| at_ms(ts.millis()))
                .collect();
        }
        claims.push(claim);
    }
    claims
}

/// What a unit is called, and where it is: a conversation by its own
/// name, and a thread by the first line of the message it hangs from, in
/// its conversation. Never its newest words, which change with every
/// message and may be the reader's own reply: this is what the echo line
/// says a verdict was about, and what Find lists a thread as.
pub fn name(model: &Model, mirror: Option<&Mirror>, unit: &Unit) -> (String, String) {
    let label = model.label(&unit.channel);
    let root = match (&unit.thread, mirror) {
        // The root is held with its replies once the thread has been
        // opened, and in its conversation before that.
        (Some(root), Some(mirror)) => match message_line(model, mirror, unit, root) {
            line if line.is_empty() => {
                message_line(model, mirror, &Unit::conversation(&unit.channel), root)
            }
            line => line,
        },
        _ => String::new(),
    };
    match root.is_empty() {
        true => (label, String::new()),
        false => (root, label),
    }
}

/// How far the user has seen a unit once they have read it: its newest
/// message, whoever wrote it.
pub fn seen(model: &Model, unit: &Unit) -> Seen {
    model
        .unit(unit)
        .map_or(Seen::Whole, |facts| Seen::Slack(facts.newest.0.clone()))
}

#[cfg(test)]
mod tests;
