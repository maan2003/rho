use std::borrow::Cow;

use anyhow::Context as _;
use gpui::{App, AssetSource, Result, SharedString};
use rust_embed::RustEmbed;

/// Rho's own assets, embedded the way zed embeds its own: a directory
/// walked at build time rather than a list of paths kept in sync by hand.
/// Adding a theme or a font is then a matter of dropping the file in.
#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "fonts/**/*.ttf"]
#[include = "settings/**/*.json"]
#[include = "themes/**/*.json"]
struct RhoEmbedded;

/// Vendored from zed's `assets/settings/default.json` (at the pinned fork
/// rev) with rho's chrome opinions applied: no line numbers, no gutter
/// buttons, no scrollbars, no indent guides. Editors are bare buffers; the
/// split tree is the chrome.
pub const RHO_DEFAULT_SETTINGS: &str = include_str!("../assets/settings/default.json");

pub struct RhoAssets;

impl AssetSource for RhoAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match RhoEmbedded::get(path) {
            // Rho's assets shadow the fork's: a theme or setting of the
            // same name is rho's opinion, deliberately.
            Some(file) => Ok(Some(file.data)),
            None => assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = assets::Assets.list(path)?;
        paths.extend(
            RhoEmbedded::iter()
                .filter(|asset| asset.starts_with(path))
                .map(|asset| SharedString::from(asset.to_string())),
        );
        Ok(paths)
    }
}

impl RhoAssets {
    /// Loads the fork's bundled fonts and then rho's, so a transcript reads
    /// the same on a machine with nothing installed. See
    /// `assets/fonts/rho-font/README.md` for what rho ships and why.
    pub fn load_fonts(&self, cx: &App) -> anyhow::Result<()> {
        assets::Assets.load_fonts(cx)?;
        let fonts = RhoEmbedded::iter()
            .filter(|asset| asset.ends_with(".ttf"))
            .map(|asset| {
                RhoEmbedded::get(&asset)
                    .map(|file| file.data)
                    .with_context(|| format!("loading font at path {asset:?}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        cx.text_system().add_fonts(fonts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oled_theme_is_embedded_and_valid() {
        let path = "themes/rho-oled/rho-oled.json";
        assert!(
            RhoAssets
                .list("themes/")
                .unwrap()
                .iter()
                .any(|item| item == path)
        );

        let registry = theme::ThemeRegistry::new(Box::new(RhoAssets));
        theme_settings::load_bundled_themes(&registry);
        registry.get("Rho OLED").expect("registered OLED theme");
    }
}
