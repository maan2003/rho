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
/// surface viewport is the chrome.
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

    fn wcag_relative_luminance(color: gpui::Color) -> f32 {
        let color = gpui::Rgba::from(color);
        let linear = |component: f32| {
            if component <= 0.04045 {
                component / 12.92
            } else {
                ((component + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    #[test]
    fn oksolar_body_foreground_is_quieter_and_remains_accessible() {
        let registry = theme::ThemeRegistry::new(Box::new(RhoAssets));
        theme_settings::load_bundled_themes(&registry);
        let theme = registry
            .get("Rho OKSolar P3")
            .expect("registered Rho OKSolar P3 theme");
        let colors = theme.colors();

        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../assets/themes/rho-oksolar-p3/rho-oksolar-p3.json"
        ))
        .expect("valid Rho OKSolar P3 source");
        let style = &source["themes"][0]["style"];
        assert_eq!(style["text"], "oklch(68.8% 0.012 95)");
        assert_eq!(style["editor.foreground"], "oklch(68.8% 0.012 95)");
        assert_eq!(
            colors.text, colors.editor_foreground,
            "general and editor body text use the same foreground"
        );

        let foreground = wcag_relative_luminance(colors.editor_foreground);
        let background = wcag_relative_luminance(colors.editor_background);
        let contrast = (foreground + 0.05) / (background + 0.05);
        assert!(
            contrast >= 6.0,
            "body text contrast must remain at least 6:1, got {contrast:.2}:1"
        );
        assert!(
            (contrast - 6.017).abs() < 0.005,
            "unexpected contrast {contrast}"
        );
    }

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
        for name in ["Rho OLED", "Rho OKSolar P3"] {
            let theme = registry
                .get(name)
                .unwrap_or_else(|_| panic!("registered {name} theme"));
            let strike = theme
                .syntax()
                .style_for_name("strikethrough")
                .unwrap_or_else(|| panic!("{name} maps Markdown strikethrough"));
            assert!(
                strike.strikethrough.is_some(),
                "{name} draws a strike rather than only dimming the text"
            );
        }
    }

    #[test]
    fn bundled_noto_color_emoji_is_used_by_automatic_fallback() -> anyhow::Result<()> {
        use gpui::{FontRun, PlatformTextSystem as _};
        use gpui_wgpu::CosmicTextSystem;

        let font_bytes = |path| {
            RhoEmbedded::get(path)
                .map(|file| file.data)
                .with_context(|| format!("loading embedded test font {path}"))
        };
        let text_system = CosmicTextSystem::new_without_system_fonts("sans-serif");
        text_system.add_fonts(vec![
            font_bytes("fonts/rho-font/RhoFont-Regular.ttf")?,
            font_bytes("fonts/noto-color-emoji/NotoColorEmoji.ttf")?,
        ])?;

        assert!(
            text_system
                .all_font_names()
                .iter()
                .any(|name| name == "Noto Color Emoji")
        );

        let primary_id = text_system.font_id(&gpui::font("Rho Font"))?;
        for (description, text) in [
            ("skin-tone emoji", "👍🏽"),
            ("skin-tone ZWJ emoji", "👩🏽‍💻"),
            ("standard emoji", "🎉"),
        ] {
            let layout = text_system.layout_line(
                text,
                gpui::px(16.0),
                &[FontRun {
                    len: text.len(),
                    font_id: primary_id,
                }],
            );
            let glyphs = layout
                .runs
                .iter()
                .flat_map(|run| run.glyphs.iter())
                .collect::<Vec<_>>();

            assert_eq!(glyphs.len(), 1, "{description} must shape as one glyph");
            assert!(glyphs[0].is_emoji, "{description} must use the emoji face");
            assert!(
                layout.runs.iter().all(|run| run.font_id != primary_id),
                "{description} must fall back from Rho Font"
            );
        }

        Ok(())
    }
}
