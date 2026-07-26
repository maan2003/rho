use std::borrow::Cow;

use gpui::{App, AssetSource, Result, SharedString};

const RHO_OKSOLAR_P3_THEME_PATH: &str = "themes/rho-oksolar-p3/rho-oksolar-p3.json";
const RHO_OKSOLAR_P3_THEME: &[u8] =
    include_bytes!("../assets/themes/rho-oksolar-p3/rho-oksolar-p3.json");

/// Vendored from zed's `assets/settings/default.json` (at the pinned fork
/// rev) with rho's chrome opinions applied: no line numbers, no gutter
/// buttons, no scrollbars, no indent guides. Editors are bare buffers; the
/// split tree is the chrome.
pub const RHO_DEFAULT_SETTINGS: &str = include_str!("../assets/settings/default.json");
const DEFAULT_SETTINGS_PATH: &str = "settings/default.json";

/// The transcript's typeface, bundled so rho reads the same everywhere
/// rather than depending on what a machine happens to have installed.
///
/// One `wght` axis from 400 to 700 in each of upright and italic, so a theme
/// can ask for a weight between the two ends (see `emphasis.strong`) instead
/// of choosing between regular and a bold that shouts. Renamed from iA
/// Writer Duo V under the OFL; see `assets/fonts/rho-font/README.md`.
const RHO_FONT_REGULAR: &[u8] = include_bytes!("../assets/fonts/rho-font/RhoFont-Regular.ttf");
const RHO_FONT_ITALIC: &[u8] = include_bytes!("../assets/fonts/rho-font/RhoFont-Italic.ttf");

pub struct RhoAssets;

impl AssetSource for RhoAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == RHO_OKSOLAR_P3_THEME_PATH {
            return Ok(Some(Cow::Borrowed(RHO_OKSOLAR_P3_THEME)));
        }
        if path == DEFAULT_SETTINGS_PATH {
            return Ok(Some(Cow::Borrowed(RHO_DEFAULT_SETTINGS.as_bytes())));
        }

        assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = assets::Assets.list(path)?;
        if RHO_OKSOLAR_P3_THEME_PATH.starts_with(path) {
            paths.push(RHO_OKSOLAR_P3_THEME_PATH.into());
        }
        Ok(paths)
    }
}

impl RhoAssets {
    pub fn load_fonts(&self, cx: &App) -> anyhow::Result<()> {
        assets::Assets.load_fonts(cx)?;
        cx.text_system().add_fonts(vec![
            Cow::Borrowed(RHO_FONT_REGULAR),
            Cow::Borrowed(RHO_FONT_ITALIC),
        ])
    }
}
