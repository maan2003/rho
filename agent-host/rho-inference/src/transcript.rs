//! Read-only provider interpretation for transcript and export consumers.
use rho_agent::entry::{Entry, Report, compose, plain_text};
use rho_agent::inference::{Carry, Image};
use rho_agent::record::{ClaudeRow, Event};
use rho_agent::store::StoreRead as _;
use rho_agent_types::{ContentPart, Role, Wpos};

use crate::step;

/// Display evidence only; never accepted as input to an inference session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportOutput {
    pub id: String,
    pub text: String,
    pub images: Vec<Image>,
}
impl ReportOutput {
    pub fn display_id(&self) -> &str {
        &self.id
    }
}
/// What a request's call was answered with, as `compose` made it. A peer's
/// message shows as its own block, not here.
pub fn report_results(report: &Report, before: &Before) -> Vec<ReportOutput> {
    let Some(carry) = &before.carry else {
        return Vec::new();
    };
    let others: &[Vec<ContentPart>] = if before.inputs { &[Vec::new()] } else { &[] };
    let result = compose(carry.has_call(), &report.notices, &before.output, others).result;
    let images = result
        .iter()
        .filter_map(|part| match part {
            ContentPart::Image { media_type, data } => Some(Image {
                media_type: media_type.clone(),
                data: data.clone(),
            }),
            ContentPart::Text { .. } => None,
        })
        .collect::<Vec<_>>();
    step::display_results(carry, &plain_text(&result), &images)
        .into_iter()
        .map(|r| ReportOutput {
            id: r.display_id().to_owned(),
            text: r.text,
            images: r.images,
        })
        .collect()
}

/// What a request is read against: the agent's newest step before it, whose
/// calls the request answers, the notebook output that goes with it, and
/// whether messages do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Before {
    pub carry: Option<Carry>,
    pub output: Vec<Vec<ContentPart>>,
    pub inputs: bool,
}

impl Before {
    /// The state after one more of the agent's visible rows.
    pub fn observe(&mut self, event: &Event<'_>) {
        match event {
            Event::Input {
                role: Role::Tool,
                message: None,
                content,
                ..
            } => self.output.push(content.clone()),
            Event::Input { .. } => self.inputs = true,
            Event::Native {
                entry: Entry::Step { carry, .. },
                ..
            } => self.carry = Some(carry.clone()),
            Event::Native {
                entry: Entry::RequestSent { .. },
                ..
            }
            | Event::Claude {
                row: ClaudeRow::RequestSent { .. },
                ..
            } => {
                self.inputs = false;
                self.output.clear();
            }
            _ => {}
        }
    }
}

/// The agent's state just before `pos`: its visible rows before it, folded.
/// A rewind before `pos` hides what it took back; one after does not.
///
/// Read newest first, from the agent's own rows, and only as far back as
/// the answer needs: its newest standing step, and the inputs and output
/// after its newest request. A rewind takes
/// back the agent's rows from its `to` up to itself, whether or not a later
/// rewind took it back too, so a row is hidden when any rewind after it starts
/// at or before it.
pub fn before_at(read: &rho_db::ReadTxn, agent: rho_agent_types::AgentId, pos: Wpos) -> Before {
    const PAGE: usize = 64;
    let mut carry: Option<Option<Carry>> = None;
    let mut inputs: Option<bool> = None;
    // Newest first, up to the newest request.
    let mut output = Vec::new();
    let mut past = false;
    let mut hidden_from = pos;
    let mut before = pos;
    while carry.is_none() || inputs.is_none() {
        let page = read.agent_log_before(agent, before, PAGE);
        let Some((oldest, _)) = page.last() else {
            break;
        };
        before = *oldest;
        for (row, record) in page {
            if let Event::Rewound { to, .. } = &record.event {
                hidden_from = hidden_from.min(*to);
            }
            if row >= hidden_from {
                continue;
            }
            match &record.event {
                Event::Input {
                    role: Role::Tool,
                    message: None,
                    content,
                    ..
                } => {
                    if !past {
                        output.push(content.clone());
                    }
                }
                Event::Input { .. } => {
                    inputs.get_or_insert(true);
                }
                Event::Native {
                    entry: Entry::Step { carry: step, .. },
                    ..
                } => {
                    carry.get_or_insert_with(|| Some(step.clone()));
                }
                Event::Native {
                    entry: Entry::RequestSent { .. },
                    ..
                }
                | Event::Claude {
                    row: ClaudeRow::RequestSent { .. },
                    ..
                } => {
                    inputs.get_or_insert(false);
                    past = true;
                }
                _ => {}
            }
        }
    }
    output.reverse();
    Before {
        carry: carry.flatten(),
        output,
        inputs: inputs.unwrap_or(false),
    }
}
