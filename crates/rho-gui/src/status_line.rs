pub(crate) struct StatusLine {
    pub(crate) prompt_chips: Vec<Chip>,
}

pub(crate) struct StatusLineInput<'a> {
    pub(crate) current_role: Option<&'a str>,
    pub(crate) current_model: Option<&'a tau_proto::ModelId>,
    pub(crate) baseline_params: Option<tau_proto::ModelParams>,
    pub(crate) current_params: tau_proto::ModelParams,
    pub(crate) role_default_effort: Option<tau_proto::Effort>,
    pub(crate) role_default_verbosity: Option<tau_proto::Verbosity>,
}

pub(crate) fn build(input: StatusLineInput<'_>) -> StatusLine {
    let prompt_chips = prompt_chips(
        left_identity(&input),
        input.baseline_params,
        input.current_params,
        input.role_default_effort,
        input.role_default_verbosity,
    );
    StatusLine { prompt_chips }
}

pub(crate) struct Chip {
    pub(crate) text: String,
    pub(crate) style: ChipStyle,
}

impl Chip {
    fn new(text: impl Into<String>, style: ChipStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChipStyle {
    Role,
    Model,
    Muted,
    Effort,
    Verbosity,
    ServiceTier,
}

enum LeftStatusIdentity<'a> {
    Role(&'a str),
    Model(&'a tau_proto::ModelId),
    NoRoleSelected,
}

fn left_identity<'a>(input: &'a StatusLineInput<'a>) -> Option<LeftStatusIdentity<'a>> {
    if let Some(role) = input.current_role {
        Some(LeftStatusIdentity::Role(role))
    } else if let Some(model) = input.current_model {
        Some(LeftStatusIdentity::Model(model))
    } else {
        Some(LeftStatusIdentity::NoRoleSelected)
    }
}

fn prompt_chips(
    identity: Option<LeftStatusIdentity<'_>>,
    baseline_params: Option<tau_proto::ModelParams>,
    current_params: tau_proto::ModelParams,
    role_default_effort: Option<tau_proto::Effort>,
    role_default_verbosity: Option<tau_proto::Verbosity>,
) -> Vec<Chip> {
    let mut chips = Vec::new();
    match identity {
        Some(LeftStatusIdentity::Role(role)) => chips.push(Chip::new(role, ChipStyle::Role)),
        Some(LeftStatusIdentity::Model(model)) => {
            chips.push(Chip::new(format!("={model}"), ChipStyle::Model))
        }
        Some(LeftStatusIdentity::NoRoleSelected) => {
            chips.push(Chip::new("no role selected", ChipStyle::Muted))
        }
        None => {}
    }
    if show_effort_status(baseline_params, current_params, role_default_effort) {
        chips.push(Chip::new(
            format!("^{}", current_params.effort.as_str()),
            ChipStyle::Effort,
        ));
    }
    if show_verbosity_status(baseline_params, current_params, role_default_verbosity) {
        chips.push(Chip::new(
            format!("~{}", current_params.verbosity.as_str()),
            ChipStyle::Verbosity,
        ));
    }
    if show_service_tier_status(baseline_params, current_params) {
        chips.push(Chip::new("⚡", ChipStyle::ServiceTier));
    }
    chips
}
fn show_effort_status(
    baseline_params: Option<tau_proto::ModelParams>,
    current_params: tau_proto::ModelParams,
    role_default_effort: Option<tau_proto::Effort>,
) -> bool {
    baseline_params.map_or_else(
        || {
            role_default_effort.map_or(!current_params.effort.is_default(), |default| {
                current_params.effort != default
            })
        },
        |default| current_params.effort != default.effort,
    )
}

fn show_verbosity_status(
    baseline_params: Option<tau_proto::ModelParams>,
    current_params: tau_proto::ModelParams,
    role_default_verbosity: Option<tau_proto::Verbosity>,
) -> bool {
    baseline_params.map_or_else(
        || {
            role_default_verbosity.map_or(!current_params.verbosity.is_default(), |default| {
                current_params.verbosity != default
            })
        },
        |default| current_params.verbosity != default.verbosity,
    )
}

fn show_service_tier_status(
    baseline_params: Option<tau_proto::ModelParams>,
    current_params: tau_proto::ModelParams,
) -> bool {
    baseline_params.map_or(current_params.service_tier.is_some(), |default| {
        current_params.service_tier != default.service_tier
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> StatusLineInput<'static> {
        StatusLineInput {
            current_role: Some("senior-engineer"),
            current_model: None,
            baseline_params: None,
            current_params: tau_proto::ModelParams::default(),
            role_default_effort: None,
            role_default_verbosity: None,
        }
    }

    #[test]
    fn build_renders_role_identity() {
        let status_line = build(input());

        assert_eq!(status_line.prompt_chips[0].text, "senior-engineer");
        assert_eq!(status_line.prompt_chips[0].style, ChipStyle::Role);
    }

    #[test]
    fn prompt_chips_can_omit_primary_identity() {
        let chips = prompt_chips(None, None, tau_proto::ModelParams::default(), None, None);

        assert!(
            chips.iter().all(|chip| chip.text != "no role selected"),
            "hidden active-agent identity must not fall through to no-role status"
        );
    }

    #[test]
    fn prompt_chips_render_no_role_when_identity_says_so() {
        let chips = prompt_chips(
            Some(LeftStatusIdentity::NoRoleSelected),
            None,
            tau_proto::ModelParams::default(),
            None,
            None,
        );

        assert_eq!(chips[0].text, "no role selected");
        assert_eq!(chips[0].style, ChipStyle::Muted);
    }
}
