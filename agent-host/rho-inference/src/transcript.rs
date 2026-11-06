//! Read-only provider interpretation for transcript and export consumers.
use rho_agent::entry::{Entry, RequestNotice, compose, plain_text};
use rho_agent::inference::{Carry, Image};
use rho_agent::record::{Event, Input};
use rho_agent::store::StoreRead as _;
use rho_agent_types::{ContentPart, Wpos};

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
/// What a request's call was answered with, as `compose` made it: `carry` is
/// the agent's newest step before it. A peer's message shows as its own block,
/// not here.
pub fn report_results(
    carry: Option<&Carry>,
    notices: &[RequestNotice],
    output: &[ContentPart],
    inputs: &[Input],
) -> Vec<ReportOutput> {
    let Some(carry) = carry else {
        return Vec::new();
    };
    let result = compose(carry.has_call(), notices, output, inputs).result;
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

/// The agent's newest standing step before `pos`, whose calls a request at
/// `pos` answers. A rewind before `pos` hides what it took back; one after does
/// not.
///
/// Read newest first, from the agent's own rows, and only as far back as the
/// answer needs. A rewind takes back the agent's rows from its `to` up to
/// itself, whether or not a later rewind took it back too, so a row is hidden
/// when any rewind after it starts at or before it.
pub fn carry_before(
    read: &rho_db::ReadTxn,
    agent: rho_agent_types::AgentId,
    pos: Wpos,
) -> Option<Carry> {
    const PAGE: usize = 64;
    let mut hidden_from = pos;
    let mut before = pos;
    loop {
        let page = read.agent_log_before(agent, before, PAGE);
        let (oldest, _) = page.last()?;
        before = *oldest;
        for (row, record) in page {
            if let Event::Rewound { to, .. } = &record.event {
                hidden_from = hidden_from.min(*to);
            }
            if row < hidden_from
                && let Event::Native {
                    entry: Entry::Step { carry, .. },
                    ..
                } = record.event
            {
                return Some(carry);
            }
        }
    }
}
