//! The native runtime's own rows in its workset's log: what the model was
//! shown and what it answered. With the agent's `Input` rows, the model's
//! context and the transcript are both projections of these.

use rho_agent_types::ContentPart;
use senax_encoder::{Decode, Encode};

use crate::inference::{Carry, Image, Usage};

/// Part of a message body. Text is kept as written; structure lives around
/// it, never inside it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Block {
    Text(String),
    Image(Image),
}

/// Why the model was woken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Wake {
    Message,
    AgentMessage,
    /// The latest exec's code returned.
    Returned,
    Notify,
    /// A task failed and nothing claimed the failure.
    Failure,
    Checkin,
    /// The last step wrote prose and made no call.
    Prose,
    /// The host restarted; everything running is gone.
    Restarted,
    /// The model's history branched; notebook state was not changed.
    Rewound,
    /// Ask the provider to compact the current context.
    Compaction,
    /// Carry on after a compaction that interrupted owed work.
    CompactionReply,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Notice {
    /// A model request failed, and the agent retries it by itself.
    Error(String),
    /// The agent stopped on an error and will not go on without the user.
    Stopped(String),
    Restarted,
    Archived,
    FreshNotebook,
}

/// What one inference attempt adds besides its inputs: Rho's notices. Retries
/// add more, merged until a Step closes the input group. `notebook` is on old
/// rows only: new output is a `tool` input.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Report {
    pub notices: Vec<RequestNotice>,
    pub notebook: rho_notebook::Report,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.notices.is_empty() && self.notebook.is_empty()
    }
}

/// What a model with no call open is told when `tool` text waits for one
/// ([DECISION-roles](../specs/DECISION-roles.md)).
pub const WAITING: &str =
    "Notebook output or agent messages are waiting. Your next exec result carries them.";

/// What one request says, by role.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Composed {
    /// The open call's answer. Empty with no call open.
    pub result: Vec<ContentPart>,
    /// What the model reads as `role: user`: Rho's lines as one block, then
    /// each input.
    pub user: Vec<Vec<ContentPart>>,
}

/// Splits what one request carries by role, for both runtimes
/// ([DECISION-roles](../specs/DECISION-roles.md)). `tool` holds every `tool`
/// input that still waits, `others` the `user` and `system` inputs of this
/// request. An open call always gets an answer, unless the request carries
/// nothing at all. With no call open, `tool` stays out and a fixed line says
/// so.
pub fn compose(
    call_open: bool,
    notices: &[RequestNotice],
    tool: &[Vec<ContentPart>],
    others: &[Vec<ContentPart>],
) -> Composed {
    let (filler, said): (Vec<_>, Vec<_>) = notices
        .iter()
        .partition(|notice| matches!(notice, RequestNotice::Checkin | RequestNotice::NothingNew));
    let filler = filler.first().map(|notice| notice.text().into_owned());
    let mut lines: Vec<String> = said
        .into_iter()
        .map(|notice| notice.text().into_owned())
        .collect();
    let mut result = Vec::new();
    if call_open {
        result = join(tool);
        if result.is_empty() {
            let text = if lines.is_empty() && others.is_empty() {
                filler
            } else {
                Some("New messages below.".to_owned())
            };
            result.extend(text.map(|text| ContentPart::Text { text }));
        }
    } else {
        if tool.iter().any(|content| !content.is_empty()) {
            lines.push(WAITING.to_owned());
        }
        if lines.is_empty() && others.is_empty() {
            lines.extend(filler);
        }
    }
    let mut user = Vec::new();
    if !lines.is_empty() {
        user.push(vec![ContentPart::Text {
            text: lines.join("\n\n"),
        }]);
    }
    user.extend(others.iter().cloned());
    Composed { result, user }
}

/// The text parts of some content, in order, with no mark for its images.
pub fn plain_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect()
}

/// One answer from many inputs: their text as one block, then their images.
fn join(contents: &[Vec<ContentPart>]) -> Vec<ContentPart> {
    let text = contents
        .iter()
        .map(|content| plain_text(content))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let images = contents
        .iter()
        .flatten()
        .filter(|part| matches!(part, ContentPart::Image { .. }))
        .cloned();
    (!text.is_empty())
        .then_some(ContentPart::Text { text })
        .into_iter()
        .chain(images)
        .collect()
}

/// A notebook report as the `tool` input that carries it.
pub fn output_content(report: &rho_notebook::Report) -> Vec<ContentPart> {
    let rendered = report.render();
    (!rendered.text.is_empty())
        .then_some(ContentPart::Text {
            text: rendered.text,
        })
        .into_iter()
        .chain(rendered.images.into_iter().map(|image| ContentPart::Image {
            media_type: image.media_type,
            data: image.data,
        }))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum RequestNotice {
    Restarted,
    Rewound,
    FreshNotebook,
    InterruptedExecution,
    PreviousResponseHadNoExec,
    Checkin,
    NothingNew,
}

impl RequestNotice {
    fn text(&self) -> std::borrow::Cow<'_, str> {
        let text = match self {
            Self::Restarted => {
                "rho restarted. Your notebook and everything running in it are gone, and their side effects may remain. Check the current state before carrying on."
            }
            Self::Rewound => {
                "The human rewound your visible history. Your Python notebook, running work, and side effects were not rewound. Check the current state before continuing."
            }
            Self::FreshNotebook => {
                "This agent was archived. You have a fresh notebook; earlier Python state and running work are gone."
            }
            Self::InterruptedExecution => {
                "Your response was cut off while you were writing its cell; only the code shown ran. Carry on from the notebook's state without replaying it."
            }
            Self::PreviousResponseHadNoExec => {
                "Your last response had no exec call. Text outside a call reaches nobody."
            }
            Self::Checkin => "Check-in: nothing new.",
            Self::NothingNew => "Nothing new.",
        };
        text.into()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Entry {
    Step {
        exec: Option<String>,
        prose: String,
        carry: Carry,
        /// Complete billable usage, absent for interrupted output.
        usage: Option<ResponseUsage>,
    },
    /// Logical send boundary, not provider acknowledgment. It delivers every
    /// input of the agent logged before it, except `tool` inputs when no call
    /// is open: those wait for the next call's answer.
    RequestSent {
        why: Wake,
        report: Report,
        compact: bool,
    },
    Notice {
        notice: Notice,
    },
    /// Durable queued intent; RequestSent records when an attempt picks it up.
    CompactionTrigger {
        manual: bool,
    },
}

/// Derived scheduling facts shared by the live loop and durable recovery.
/// The log, not this projection, owns compaction intent and completion.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct CompactionState {
    pub context_used: Option<u64>,
    pub pending: bool,
    pub sent: bool,
    pub owes_reply: bool,
    pub reply: bool,
}

impl CompactionState {
    pub fn observe(&mut self, entry: &Entry) {
        match entry {
            Entry::CompactionTrigger { manual, .. } => {
                self.pending = true;
                self.owes_reply = !manual;
            }
            Entry::RequestSent {
                report, compact, ..
            } => {
                self.sent = *compact;
                if *compact {
                    self.owes_reply |= !report.is_empty();
                } else {
                    self.reply = false;
                }
            }
            Entry::Step { carry, usage, .. } => {
                let compacted = carry.has_compaction();
                if compacted {
                    self.context_used = None;
                } else if let Some(usage) = usage {
                    self.context_used = Some(
                        usage
                            .input_tokens
                            .saturating_add(usage.cache_read_tokens)
                            .saturating_add(usage.output_tokens),
                    );
                }
                if self.sent {
                    self.reply = compacted && self.owes_reply;
                    self.sent = false;
                    self.pending = false;
                    self.owes_reply = false;
                }
            }
            _ => {}
        }
    }
}

/// Exact billable fields for one response.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ResponseUsage {
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
}

impl ResponseUsage {
    /// Responses API input includes cache reads; charts keep fresh input
    /// separate.
    pub fn rho(model: String, usage: Usage) -> Self {
        Self {
            model,
            input_tokens: usage.input_tokens.saturating_sub(usage.cached_tokens),
            cache_read_tokens: usage.cached_tokens,
            cache_write_tokens: 0,
            cache_write_1h_tokens: 0,
            output_tokens: usage.output_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: text.into() }]
    }
    fn image(byte: u8) -> Vec<ContentPart> {
        vec![ContentPart::Image {
            media_type: "image/png".into(),
            data: vec![byte],
        }]
    }

    /// An answer holds `tool` text, the person's text stays out of it, and a
    /// notice reads as `user`.
    #[test]
    fn an_open_call_takes_the_tool_role_and_nothing_else() {
        let composed = compose(
            true,
            &[RequestNotice::Restarted, RequestNotice::NothingNew],
            &[text("printed"), image(1), text("from a peer"), image(2)],
            &[text("from the person")],
        );
        assert_eq!(
            composed.result,
            [text("printed\n\nfrom a peer"), image(1), image(2)].concat()
        );
        assert_eq!(composed.user.len(), 2);
        assert!(
            rho_agent_types::transcript::text_content(&composed.user[0])
                .starts_with("rho restarted.")
        );
        assert_eq!(composed.user[1], text("from the person"));
    }

    /// A call must be answered: with the filler when there is no news, with
    /// a pointer when the news is only `user` text.
    #[test]
    fn an_open_call_is_always_answered() {
        let filler = compose(true, &[RequestNotice::Checkin], &[], &[]);
        assert_eq!(filler.result, text("Check-in: nothing new."));
        assert!(filler.user.is_empty());

        let pointer = compose(
            true,
            &[RequestNotice::Checkin],
            &[],
            &[text("from the person")],
        );
        assert_eq!(pointer.result, text("New messages below."));
        assert_eq!(pointer.user, [text("from the person")]);

        let notice = compose(true, &[RequestNotice::Rewound], &[], &[]);
        assert_eq!(notice.result, text("New messages below."));
        assert_eq!(notice.user.len(), 1);

        // A manual compaction has nothing to say.
        assert_eq!(compose(true, &[], &[], &[]), Composed::default());
    }

    /// With no call open, no `tool` text reaches the model. It hears only
    /// that some waits, and the filler is for when nothing else is said.
    #[test]
    fn with_no_call_open_tool_text_stays_out() {
        let composed = compose(
            false,
            &[RequestNotice::NothingNew],
            &[text("HOSTILE")],
            &[text("from the person")],
        );
        assert!(composed.result.is_empty());
        assert_eq!(composed.user, [text(WAITING), text("from the person")]);

        let composed = compose(
            false,
            &[RequestNotice::PreviousResponseHadNoExec],
            &[image(1)],
            &[],
        );
        let line = rho_agent_types::transcript::text_content(&composed.user.concat());
        assert!(
            line.contains("no exec call") && line.ends_with(WAITING),
            "{line}"
        );
        assert!(
            !composed
                .user
                .concat()
                .iter()
                .any(|part| matches!(part, ContentPart::Image { .. }))
        );

        assert_eq!(
            compose(false, &[RequestNotice::Checkin], &[], &[]).user,
            [text("Check-in: nothing new.")]
        );
        assert_eq!(
            compose(false, &[RequestNotice::NothingNew], &[], &[text("hi")]).user,
            [text("hi")]
        );
    }
}
