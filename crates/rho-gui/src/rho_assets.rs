use std::borrow::Cow;

use gpui::{App, AssetSource, Result, SharedString};

const RHO_MONOKAI_P3_THEME_PATH: &str = "themes/rho-monokai-p3/rho-monokai-p3.json";
const RHO_MONOKAI_P3_THEME: &[u8] =
    include_bytes!("../assets/themes/rho-monokai-p3/rho-monokai-p3.json");

pub struct RhoAssets;

impl AssetSource for RhoAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == RHO_MONOKAI_P3_THEME_PATH {
            return Ok(Some(Cow::Borrowed(RHO_MONOKAI_P3_THEME)));
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
