pub(crate) struct StatusLine {
    pub(crate) prompt_chips: Vec<Chip>,
}

pub(crate) struct StatusLineInput<'a> {
    pub(crate) current_role: Option<&'a str>,
}

pub(crate) fn build(input: StatusLineInput<'_>) -> StatusLine {
    let text = input.current_role.unwrap_or("rho");
    StatusLine {
        prompt_chips: vec![Chip::new(text, ChipStyle::Role)],
    }
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
    Muted,
}
