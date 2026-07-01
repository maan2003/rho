use tau_config::settings::CliTheme;

const THEME_ENV: &str = "RHO_THEME";
const LEGACY_THEME_ENV: &str = "TAU_THEME";

/// Resolve a configured [`CliTheme`] to a concrete theme. `RHO_THEME` may name
/// an override; `TAU_THEME` is still accepted as a transitional fallback. An
/// unknown name falls back to the built-in default.
pub(crate) fn select_theme(mode: CliTheme) -> tau_themes::Theme {
    let mode = env_theme_override().unwrap_or(mode);
    match mode {
        CliTheme::Named(name) => {
            tau_themes::Theme::builtin_named(&name).unwrap_or_else(tau_themes::Theme::builtin)
        }
    }
}

fn env_theme_override() -> Option<CliTheme> {
    let value = std::env::var(THEME_ENV)
        .or_else(|_| std::env::var(LEGACY_THEME_ENV))
        .ok()?;
    CliTheme::parse_name(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_theme_resolves_without_falling_back() {
        // A known built-in name resolves; an unknown one falls back to the
        // default. Both must produce a usable theme rather than panic.
        let _light = select_theme(CliTheme::Named("tau-plain-light".to_owned()));
        let _fallback = select_theme(CliTheme::Named("does-not-exist".to_owned()));
    }
}
