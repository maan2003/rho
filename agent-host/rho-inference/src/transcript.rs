//! Read-only provider interpretation for transcript and export consumers.
use rho_agent::entry::Report;
use rho_agent::inference::{Carry, Image};

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
pub fn report_results(
    report: &Report,
    prior: Option<&Carry>,
    imported: Option<&Carry>,
) -> Vec<ReportOutput> {
    let results: Vec<step::CallResult> = if let Some(imported) = imported {
        step::imported_results(imported)
    } else {
        let rendered = report.render();
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
