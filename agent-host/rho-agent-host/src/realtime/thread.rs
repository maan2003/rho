//! The agent thread a voice session belongs to, in the shapes the Codex
//! client gives the live model: the thread's latest turns as startup
//! context, each spoken request as a `<realtime_delegation>` message to the
//! agent, and each message the agent sends the user as a `[STATUS]` or
//! `[COMPLETE]` reply.
//!
//! The formats and budgets follow `codex-rs` (`realtime_context.rs`,
//! `context/realtime_delegation.rs`), so the model sees what it is tuned on.

use rho_agent_types::{Address, AgentId, Mail, SendKind};
use rho_openai_realtime::ContextChannel;

use crate::transcript::content_text;

const APPROX_BYTES_PER_TOKEN: usize = 4;
const THREAD_TOKEN_BUDGET: usize = 1_200;
const TURN_TOKEN_BUDGET: usize = 300;
/// The most of one agent message the live model is given.
const REPLY_TOKEN_BUDGET: usize = 2_000;
const DELEGATION_FIELD_BYTES: usize = 4 * 1024;

const STARTUP_CONTEXT_HEADER: &str = "Startup context from Rho.\nThis is background context about the agent thread the user opened voice from. It may be incomplete or stale. Use it to inform responses, and do not repeat it back unless relevant.";

/// One message of the conversation between the user and the agent.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Said {
    User(String),
    Agent(String),
}

impl Said {
    /// What a mail says in the conversation between the user and `agent`,
    /// if it is a part of it.
    pub(super) fn of(mail: &Mail, agent: AgentId) -> Option<Self> {
        match (mail.from, mail.to) {
            (Address::User, Address::Agent(to)) if to == agent => {
                Some(Self::User(content_text(&mail.content)))
            }
            (Address::Agent(from), Address::User) if from == agent => {
                Some(Self::Agent(content_text(&mail.content)))
            }
            _ => None,
        }
    }
}

/// The session's startup context: the thread's latest turns, newest first,
/// each cut to its own budget. A turn is a user message and the agent
/// messages after it. `None` when the thread has no turn yet.
pub(super) fn startup_context(said: impl IntoIterator<Item = Said>) -> Option<String> {
    let mut turns: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    for said in said {
        match said {
            Said::User(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    turns.push((vec![text.to_owned()], Vec::new()));
                }
            }
            Said::Agent(text) => {
                let text = text.trim();
                if let Some((_, agent)) = turns.last_mut().filter(|_| !text.is_empty()) {
                    agent.push(text.to_owned());
                }
            }
        }
    }
    if turns.is_empty() {
        return None;
    }
    let heading = "## Current Thread\nMost recent user/assistant turns from this exact thread. Use them for continuity when responding.";
    let mut lines = vec![heading.to_owned()];
    let mut remaining = THREAD_TOKEN_BUDGET.saturating_sub(approx_tokens(heading));
    for (index, (user, agent)) in turns.into_iter().rev().enumerate() {
        if remaining == 0 {
            break;
        }
        let mut turn = vec![match index {
            0 => "### Latest turn".to_owned(),
            index => format!("### Previous turn {index}"),
        }];
        turn.push("User:".to_owned());
        turn.push(user.join("\n\n"));
        if !agent.is_empty() {
            turn.push(String::new());
            turn.push("Assistant:".to_owned());
            turn.push(agent.join("\n\n"));
        }
        let turn = clip(&turn.join("\n"), TURN_TOKEN_BUDGET.min(remaining));
        remaining = remaining.saturating_sub(approx_tokens(&turn));
        lines.push(String::new());
        lines.push(turn);
    }
    Some(format!(
        "<startup_context>\n{STARTUP_CONTEXT_HEADER}\n\n{}\n</startup_context>",
        lines.join("\n")
    ))
}

/// The message the agent gets for one spoken request: what the live model
/// asked for, and what was said since the last request.
pub(super) fn delegation(input: &str, transcript_delta: &str) -> String {
    let input = escape_bounded(input, Retain::Start);
    let delta = match transcript_delta.trim() {
        "" => String::new(),
        delta => format!(
            "  <transcript_delta>{}</transcript_delta>\n",
            escape_bounded(delta, Retain::End)
        ),
    };
    format!("<realtime_delegation>\n  <input>{input}</input>\n{delta}</realtime_delegation>")
}

/// What a reader is shown of a user message that is a spoken request: the
/// request itself, marked as spoken, in place of the markup the agent reads.
/// `None` for any other message.
pub(crate) fn shown_to_reader(text: &str) -> Option<String> {
    let start = text.find("<realtime_delegation>")?;
    let input = text[start..]
        .split_once("<input>")?
        .1
        .split_once("</input>")?
        .0
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    Some(format!("{}🎤 {}", &text[..start], input.trim()))
}

/// What the live model is told of a mail `agent` sent the user, on which
/// channel, and whether it ends the request. Progress is a `[STATUS]` the
/// model may keep to itself; a result or a question is the `[COMPLETE]`
/// answer.
pub(super) fn reply(mail: &Mail, agent: AgentId) -> Option<(ContextChannel, String, bool)> {
    if mail.from != Address::Agent(agent) || mail.to != Address::User {
        return None;
    }
    let complete = matches!(mail.kind, Some(SendKind::Result | SendKind::Ask));
    Some(told(&content_text(&mail.content), complete))
}

/// What the live model is told when the agent stopped on an error: the
/// `[COMPLETE]` answer.
pub(super) fn stopped(error: &str) -> (ContextChannel, String, bool) {
    told(error, true)
}

fn told(text: &str, complete: bool) -> (ContextChannel, String, bool) {
    let text = clip(text.trim(), REPLY_TOKEN_BUDGET);
    match complete {
        true => (
            ContextChannel::Speakable,
            format!("[COMPLETE] {text}"),
            true,
        ),
        false => (
            ContextChannel::Commentary,
            format!("[STATUS] {text}"),
            false,
        ),
    }
}

fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(APPROX_BYTES_PER_TOKEN)
}

/// `text` within `tokens`, keeping its start and its end.
fn clip(text: &str, tokens: usize) -> String {
    let budget = tokens * APPROX_BYTES_PER_TOKEN;
    if text.len() <= budget {
        return text.to_owned();
    }
    let omitted = approx_tokens(&text[..text.len() - budget]);
    let marker = format!("\n…{omitted} tokens truncated…\n");
    let available = budget.saturating_sub(marker.len());
    let mut head = available / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (available - available / 2);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{marker}{}", &text[..head], &text[tail..])
}

#[derive(Clone, Copy)]
enum Retain {
    Start,
    End,
}

fn escape_bounded(text: &str, retain: Retain) -> String {
    let text = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if text.len() <= DELEGATION_FIELD_BYTES {
        return text;
    }
    let kept = DELEGATION_FIELD_BYTES - '…'.len_utf8();
    match retain {
        Retain::Start => {
            let mut end = kept;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &text[..end])
        }
        Retain::End => {
            let mut start = text.len() - kept;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            format!("…{}", &text[start..])
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentIdDomain, ContentPart, MailId, UnixMs};

    use super::*;

    fn agent(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &AgentIdDomain(0)).unwrap()
    }

    fn mail(from: Address, to: Address, kind: Option<SendKind>, text: &str) -> Mail {
        Mail {
            id: MailId(1),
            at: UnixMs(1),
            from,
            to,
            re: None,
            kind,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
        }
    }

    fn sent(kind: SendKind, text: &str) -> Mail {
        mail(Address::Agent(agent(1)), Address::User, Some(kind), text)
    }

    fn channel_name(channel: ContextChannel) -> &'static str {
        match channel {
            ContextChannel::Commentary => "commentary",
            ContextChannel::Speakable => "speakable",
        }
    }

    #[test]
    fn the_newest_turn_comes_first_and_agent_words_before_any_ask_are_dropped() {
        let context = startup_context([
            Said::Agent("orphan hello".to_owned()),
            Said::User("first ask".to_owned()),
            Said::Agent("first answer".to_owned()),
            Said::User("second ask".to_owned()),
            Said::Agent("working on it".to_owned()),
            Said::Agent("second answer".to_owned()),
        ])
        .unwrap();
        assert!(!context.contains("orphan"));
        let latest = context.find("### Latest turn").unwrap();
        let previous = context.find("### Previous turn 1").unwrap();
        assert!(latest < context.find("second ask").unwrap());
        assert!(context.find("second answer").unwrap() < previous);
        assert!(previous < context.find("first ask").unwrap());
        assert!(context.contains("Assistant:\nworking on it\n\nsecond answer"));
        assert!(context.starts_with("<startup_context>\n"));
        assert!(context.ends_with("\n</startup_context>"));
        assert_eq!(startup_context([Said::Agent("only me".to_owned())]), None);
    }

    #[test]
    fn a_long_turn_keeps_its_start_and_end_and_old_turns_fall_off() {
        let long = format!("START {} END", "x".repeat(10_000));
        let mut said = Vec::new();
        for turn in 0..10 {
            said.push(Said::User(format!("ask {turn}")));
            said.push(Said::Agent(long.clone()));
        }
        let context = startup_context(said).unwrap();
        // Each turn is cut to 300 tokens and the thread to 1,200, so only
        // the newest four turns fit.
        assert!(context.contains("ask 9") && context.contains("ask 6"));
        assert!(!context.contains("ask 5"));
        assert_eq!(context.matches("START").count(), 4);
        assert_eq!(context.matches(" END").count(), 4);
        assert!(context.contains("tokens truncated"));
        assert!(approx_tokens(&context) < THREAD_TOKEN_BUDGET + 200);
    }

    #[test]
    fn a_delegation_escapes_markup_and_keeps_the_newest_speech() {
        assert_eq!(
            delegation("fix <b> & ship", "  "),
            "<realtime_delegation>\n  <input>fix &lt;b&gt; &amp; ship</input>\n</realtime_delegation>"
        );
        let speech = format!("user: OLD{}\nuser: newest words", "é".repeat(5_000));
        let message = delegation("go", &speech);
        assert!(message.contains("newest words</transcript_delta>"));
        assert!(!message.contains("OLD"));
        assert!(message.contains("<transcript_delta>…"));
    }

    #[test]
    fn a_reader_sees_the_spoken_request_and_not_the_markup() {
        let message = delegation("fix <b> & ship", "user: fix b and ship");
        assert_eq!(
            shown_to_reader(&message).as_deref(),
            Some("🎤 fix <b> & ship")
        );
        let after_notice = format!("Your workset moved.\n{message}");
        assert_eq!(
            shown_to_reader(&after_notice).as_deref(),
            Some("Your workset moved.\n🎤 fix <b> & ship")
        );
        assert_eq!(shown_to_reader("typed <input>x</input>"), None);
    }

    #[test]
    fn the_send_kind_picks_the_channel_and_whether_the_request_is_done() {
        let cases = [
            (SendKind::Status, "commentary", "[STATUS] looking", false),
            (SendKind::Fyi, "commentary", "[STATUS] looking", false),
            (SendKind::Result, "speakable", "[COMPLETE] looking", true),
            (SendKind::Ask, "speakable", "[COMPLETE] looking", true),
        ];
        for (kind, channel, text, complete) in cases {
            let (got_channel, got_text, got_complete) =
                reply(&sent(kind, " looking\n"), agent(1)).unwrap();
            assert_eq!(
                (channel_name(got_channel), got_text.as_str(), got_complete),
                (channel, text, complete),
                "{kind:?}"
            );
        }
        // Mail to another agent, or from another agent, is not this
        // agent's answer to the user.
        let to_agent = mail(
            Address::Agent(agent(1)),
            Address::Agent(agent(7)),
            Some(SendKind::Result),
            "x",
        );
        assert_eq!(reply(&to_agent, agent(1)), None);
        assert_eq!(reply(&sent(SendKind::Result, "x"), agent(7)), None);
        let (channel, text, complete) = stopped("quota");
        assert_eq!(
            (channel_name(channel), text.as_str(), complete),
            ("speakable", "[COMPLETE] quota", true)
        );
    }

    #[test]
    fn the_thread_is_the_mail_between_the_user_and_this_agent() {
        let me = Address::Agent(agent(1));
        let other = Address::Agent(agent(2));
        assert_eq!(
            Said::of(&mail(Address::User, me, None, "ask"), agent(1)),
            Some(Said::User("ask".to_owned()))
        );
        assert_eq!(
            Said::of(
                &mail(me, Address::User, Some(SendKind::Result), "done"),
                agent(1)
            ),
            Some(Said::Agent("done".to_owned()))
        );
        assert_eq!(
            Said::of(&mail(Address::User, other, None, "x"), agent(1)),
            None
        );
        assert_eq!(Said::of(&mail(other, me, None, "x"), agent(1)), None);
    }

    #[test]
    fn a_long_reply_is_cut_to_two_thousand_tokens() {
        let text = format!("HEAD{}TAIL", "ü".repeat(20_000));
        let (_, reply, _) = reply(&sent(SendKind::Result, &text), agent(1)).unwrap();
        assert!(reply.starts_with("[COMPLETE] HEAD") && reply.ends_with("TAIL"));
        assert!(reply.len() <= REPLY_TOKEN_BUDGET * APPROX_BYTES_PER_TOKEN + 20);
    }
}
