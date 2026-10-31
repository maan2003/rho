//! Read-only provider interpretation for transcript and export consumers.
use rho_agent::entry::{Entry, Report};
use rho_agent::inference::{Carry, Image};
use rho_agent::record::{ClaudeRow, Event};
use rho_agent::store::StoreRead as _;
use rho_agent_types::Wpos;

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
/// `inputs`: the request also delivered inputs, as `Report::render` takes it.
pub fn report_results(report: &Report, inputs: bool, prior: Option<&Carry>) -> Vec<ReportOutput> {
    let results: Vec<step::CallResult> = {
        let rendered = report.render(inputs);
        let images = rendered
            .images
            .into_iter()
            .map(|i| Image {
                media_type: i.media_type,
                data: i.data,
            })
            .collect::<Vec<_>>();
        prior.map_or_else(Vec::new, |carry| {
            step::display_results(carry, &rendered.text, &images)
        })
    };
    results
        .into_iter()
        .map(|r| ReportOutput {
            id: r.display_id().to_owned(),
            text: r.text,
            images: r.images,
        })
        .collect()
}

/// What a request's report is read against: the agent's newest step before
/// it, whose calls the report answers, and whether inputs went with it.
#[derive(Clone, Debug, Default)]
pub struct Before {
    pub carry: Option<Carry>,
    pub inputs: bool,
}

impl Before {
    /// The state after one more of the agent's visible rows.
    pub fn observe(&mut self, event: &Event<'_>) {
        match event {
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
            } => self.inputs = false,
            _ => {}
        }
    }
}

/// The agent's state just before `pos`: its visible rows before it, folded.
/// A rewind before `pos` hides what it took back; one after does not.
///
/// Read newest first, from the agent's own rows, and only as far back as
/// the answer needs: its newest standing step, and whether an input came
/// after its newest request. A rewind takes back the agent's rows from its
/// `to` up to itself, whether or not a later rewind took it back too, so
/// a row is hidden when any rewind after it starts at or before it.
pub fn before_at(read: &rho_db::ReadTxn, agent: rho_agent_types::AgentId, pos: Wpos) -> Before {
    const PAGE: usize = 64;
    let mut carry: Option<Option<Carry>> = None;
    let mut inputs: Option<bool> = None;
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
                }
                _ => {}
            }
        }
    }
    Before {
        carry: carry.flatten(),
        inputs: inputs.unwrap_or(false),
    }
}
