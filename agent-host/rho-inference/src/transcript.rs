//! Read-only provider interpretation for transcript and export consumers.
use rho_agent::entry::{Entry, Report, compose, output_content, plain_text};
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
    let mut tool = Vec::new();
    if !report.notebook.is_empty() {
        tool.push(output_content(&report.notebook));
    }
    tool.extend(before.output.iter().cloned());
    let others: &[Vec<ContentPart>] = if before.inputs { &[Vec::new()] } else { &[] };
    let result = compose(carry.has_call(), &report.notices, &tool, others).result;
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
/// calls the request answers, the notebook output that waits for an answer,
/// and whether messages went with it.
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
            } => {
                self.inputs = false;
                // With no call to answer, output waits for one.
                if self.carry.as_ref().is_some_and(Carry::has_call) {
                    self.output.clear();
                }
            }
            Event::Claude {
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
/// the answer needs: its newest standing step, whether an input came after
/// its newest request, and the output no answered call took. A rewind takes
/// back the agent's rows from its `to` up to itself, whether or not a later
/// rewind took it back too, so a row is hidden when any rewind after it starts
/// at or before it.
pub fn before_at(read: &rho_db::ReadTxn, agent: rho_agent_types::AgentId, pos: Wpos) -> Before {
    const PAGE: usize = 64;
    let mut carry: Option<Option<Carry>> = None;
    let mut inputs: Option<bool> = None;
    // Newest first. Output that a request picked up still waits if the step
    // before that request had no call, so it stays aside until that step.
    let mut output = Vec::new();
    let mut picked_up = Vec::new();
    let mut past_a_request = false;
    let mut output_done = false;
    let mut hidden_from = pos;
    let mut before = pos;
    while carry.is_none() || inputs.is_none() || !output_done {
        let page = read.agent_log_before(agent, before, PAGE);
        let Some((oldest, _)) = page.last() else {
            output.append(&mut picked_up);
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
                    if !output_done {
                        let into = if past_a_request {
                            &mut picked_up
                        } else {
                            &mut output
                        };
                        into.push(content.clone());
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
                    if past_a_request {
                        if step.has_call() {
                            output_done = true;
                            picked_up.clear();
                        } else {
                            output.append(&mut picked_up);
                            past_a_request = false;
                        }
                    }
                }
                Event::Native {
                    entry: Entry::RequestSent { .. },
                    ..
                } => {
                    inputs.get_or_insert(false);
                    past_a_request = true;
                }
                Event::Claude {
                    row: ClaudeRow::RequestSent { .. },
                    ..
                } => {
                    inputs.get_or_insert(false);
                    output_done = true;
                    picked_up.clear();
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
