use std::borrow::Cow;

use gpui::{App, AssetSource, Result, SharedString};

const RHO_MONOKAI_P3_THEME_PATH: &str = "themes/rho-monokai-p3/rho-monokai-p3.json";
const RHO_MONOKAI_P3_THEME: &[u8] =
    include_bytes!("../assets/themes/rho-monokai-p3/rho-monokai-p3.json");

/// Vendored from zed's `assets/settings/default.json` (at the pinned fork
/// rev) with rho's chrome opinions applied: no line numbers, no gutter
/// buttons, no scrollbars, no indent guides. Editors are bare buffers; the
/// split tree is the chrome.
pub const RHO_DEFAULT_SETTINGS: &str = include_str!("../assets/settings/default.json");
const DEFAULT_SETTINGS_PATH: &str = "settings/default.json";

pub struct RhoAssets;

impl AssetSource for RhoAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == RHO_MONOKAI_P3_THEME_PATH {
            return Ok(Some(Cow::Borrowed(RHO_MONOKAI_P3_THEME)));
        }
        if path == DEFAULT_SETTINGS_PATH {
            return Ok(Some(Cow::Borrowed(RHO_DEFAULT_SETTINGS.as_bytes())));
        }

        assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = assets::Assets.list(path)?;
        if RHO_MONOKAI_P3_THEME_PATH.starts_with(path) {
            paths.push(RHO_MONOKAI_P3_THEME_PATH.into());
        }
        Ok(paths)
    }
}

impl RhoAssets {
    pub fn load_fonts(&self, cx: &App) -> anyhow::Result<()> {
        assets::Assets.load_fonts(cx)
    }
}
