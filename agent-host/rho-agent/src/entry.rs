//! What a request says and what happens to an agent, in the words both
//! runtimes share.

use rho_agent_types::{ContentPart, Role};
use senax_encoder::{Decode, Encode};

use crate::inference::{Carry, Image, Usage};
use crate::record::Input;

/// Part of a message body. Text is kept as written; structure lives around
/// it, never inside it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Block {
    Text(String),
    Image(Image),
}

/// Why the model was woken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    Archived,
    FreshNotebook,
}

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
/// ([DECISION-roles](../specs/DECISION-roles.md)). An open call is answered
/// with `output` and the `tool` inputs, or a line that says why there are none.
/// With no call, old rows can still hold those, and they read as `user`.
pub fn compose(
    call_open: bool,
    notices: &[RequestNotice],
    output: &[ContentPart],
    inputs: &[Input],
) -> Composed {
    let mut tool = vec![output.to_vec()];
    let mut others = Vec::new();
    if !call_open && !output.is_empty() {
        others.push(tool.remove(0));
    }
    for input in inputs {
        if call_open && input.role == Role::Tool {
            tool.push(input.content.clone());
        } else {
            others.push(input.content.clone());
        }
    }
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
        result = join(&tool);
        if result.is_empty() {
            let text = if lines.is_empty() && others.is_empty() {
                filler
            } else {
                Some("New messages below.".to_owned())
            };
            result.extend(text.map(|text| ContentPart::Text { text }));
        }
    } else if lines.is_empty() && others.is_empty() {
        lines.extend(filler);
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

/// Output that waited, and output after it, as the one answer they make.
pub fn add_output(output: &mut Vec<ContentPart>, more: Vec<ContentPart>) {
    *output = join(&[std::mem::take(output), more]);
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

/// A notebook report as its request's `output`.
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
    /// `tool` text waits for a call: the model reads it in its next exec's
    /// result ([DECISION-roles](../specs/DECISION-roles.md)).
    Waiting,
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
            Self::Waiting => {
                "Notebook output or agent messages are waiting. Your next exec result carries them."
            }
            Self::Checkin => "Check-in: nothing new.",
            Self::NothingNew => "Nothing new.",
        };
        text.into()
    }
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
    /// A request went out: `says` is whether it had anything to tell the model.
    pub fn observe_request(&mut self, compact: bool, says: bool) {
        self.sent = compact;
        if compact {
            self.owes_reply |= says;
        } else {
            self.reply = false;
        }
    }

    pub fn observe_trigger(&mut self, manual: bool) {
        self.pending = true;
        self.owes_reply = !manual;
    }

    pub fn observe_step(&mut self, carry: &Carry, usage: Option<&ResponseUsage>) {
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

    fn input(role: Role, content: Vec<ContentPart>) -> Input {
        Input {
            role,
            message: None,
            content,
        }
    }
    fn person(said: &str) -> Input {
        input(Role::User, text(said))
    }

    /// An answer holds output and then `tool` text, whatever order they came
    /// in. The person's text stays out of it, and a notice reads as `user`.
    #[test]
    fn an_open_call_takes_the_tool_role_and_nothing_else() {
        let composed = compose(
            true,
            &[RequestNotice::Restarted, RequestNotice::NothingNew],
            &[text("printed"), image(1)].concat(),
            &[
                person("from the person"),
                input(Role::Tool, [text("from a peer"), image(2)].concat()),
            ],
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
            &[person("from the person")],
        );
        assert_eq!(pointer.result, text("New messages below."));
        assert_eq!(pointer.user, [text("from the person")]);

        let notice = compose(true, &[RequestNotice::Rewound], &[], &[]);
        assert_eq!(notice.result, text("New messages below."));
        assert_eq!(notice.user.len(), 1);

        // A manual compaction has nothing to say.
        assert_eq!(compose(true, &[], &[], &[]), Composed::default());
    }

    /// With no call open a request says what it has, and the filler is for
    /// when it has nothing else.
    #[test]
    fn with_no_call_open_everything_reads_as_user() {
        let composed = compose(
            false,
            &[
                RequestNotice::PreviousResponseHadNoExec,
                RequestNotice::Waiting,
                RequestNotice::NothingNew,
            ],
            &[],
            &[person("from the person")],
        );
        assert!(composed.result.is_empty());
        assert_eq!(composed.user.len(), 2);
        let line = plain_text(&composed.user[0]);
        assert!(
            line.contains("no exec call") && line.contains("are waiting"),
            "{line}"
        );
        assert_eq!(composed.user[1], text("from the person"));

        assert_eq!(
            compose(false, &[RequestNotice::Checkin], &[], &[]).user,
            [text("Check-in: nothing new.")]
        );
        assert_eq!(
            compose(false, &[RequestNotice::NothingNew], &[], &[person("hi")]).user,
            [text("hi")]
        );
    }

    /// An old row can hold output and a peer's text with no call: they read
    /// as `user`, in that order, and there is no answer.
    #[test]
    fn old_rows_with_no_call_read_as_user() {
        let composed = compose(
            false,
            &[],
            &text("printed"),
            &[input(Role::Tool, text("from a peer"))],
        );
        assert!(composed.result.is_empty());
        assert_eq!(composed.user, [text("printed"), text("from a peer")]);
    }

    /// Output that waits joins what follows with a gap, keeping every image.
    #[test]
    fn output_adds_up_to_one_answer() {
        let mut output = Vec::new();
        add_output(&mut output, [text("first"), image(1)].concat());
        add_output(&mut output, text("second"));
        assert_eq!(output, [text("first\n\nsecond"), image(1)].concat());
    }
}
