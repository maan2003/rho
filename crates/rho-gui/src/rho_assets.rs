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

/// Rho's default choices over the current editor's defaults. User settings
/// still override both; they are loaded by the settings store separately.
pub fn default_settings() -> String {
    let family = std::env::var("RHO_GUI_FONT_FAMILY").ok();
    let number = |name| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
    };
    settings_with_font(
        family.as_deref(),
        number("RHO_GUI_FONT_SCALE"),
        number("RHO_GUI_FONT_WEIGHT"),
    )
    .to_string()
}

fn settings_with_font(
    family: Option<&str>,
    scale: Option<f32>,
    weight: Option<f32>,
) -> serde_json::Value {
    let mut base = settings::parse_json_with_comments(&settings::default_settings())
        .expect("bundled editor defaults are valid JSONC");
    let overrides = serde_json::from_str(include_str!("../assets/settings/rho-defaults.json"))
        .expect("bundled rho overrides are valid JSON");
    util::merge_json_value_into(overrides, &mut base);
    for prefix in ["buffer_font", "ui_font"] {
        if let Some(family) = family {
            base[&format!("{prefix}_family")] = family.into();
        }
        if let Some(scale) = scale {
            let size = &mut base[&format!("{prefix}_size")];
            *size =
                (size.as_f64().expect("default font size is numeric") * f64::from(scale)).into();
        }
        if let Some(weight) = weight {
            base[&format!("{prefix}_weight")] = weight.into();
        }
    }
    base
}

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
    ///
    /// Then every `.ttf` and `.otf` in the directory `RHO_GUI_FONTS` names:
    /// fonts licensed to the user rather than to rho, which the deployment
    /// supplies and must never enter the repo or the published build.
    /// Settings pick them by family like any other font, and
    /// `RHO_GUI_FONT_BOLD_WEIGHT` sets the weight bold and semibold text get.
    pub fn load_fonts(&self, cx: &App) -> anyhow::Result<()> {
        assets::Assets.load_fonts(cx)?;
        let mut fonts = RhoEmbedded::iter()
            .filter(|asset| asset.ends_with(".ttf"))
            .map(|asset| {
                RhoEmbedded::get(&asset)
                    .map(|file| file.data)
                    .with_context(|| format!("loading font at path {asset:?}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if let Some(dir) = std::env::var_os("RHO_GUI_FONTS") {
            let entries = std::fs::read_dir(&dir)
                .with_context(|| format!("reading RHO_GUI_FONTS {dir:?}"))?;
            for entry in entries {
                let path = entry?.path();
                if path
                    .extension()
                    .is_some_and(|ext| ext == "ttf" || ext == "otf")
                {
                    let data = std::fs::read(&path)
                        .with_context(|| format!("loading font at path {path:?}"))?;
                    fonts.push(Cow::Owned(data));
                }
            }
        }
        cx.text_system().add_fonts(fonts)?;
        if let Some(weight) = std::env::var("RHO_GUI_FONT_BOLD_WEIGHT")
            .ok()
            .and_then(|weight| weight.parse().ok())
        {
            cx.text_system().set_bold_weight(gpui::FontWeight(weight));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_override_replaces_both_defaults() {
        let settings = settings_with_font(Some("ABC \"Quoted\" Sans"), Some(1.5), Some(300.0));
        for key in ["buffer_font_family", "ui_font_family"] {
            assert_eq!(settings[key], "ABC \"Quoted\" Sans");
        }
        assert_eq!(settings["buffer_font_size"].as_f64(), Some(22.5));
        assert_eq!(settings["ui_font_size"].as_f64(), Some(21.0));
        assert_eq!(
            settings["agent_buffer_font_size"], 12,
            "only two default sizes scale"
        );
        assert_eq!(settings["buffer_font_weight"].as_f64(), Some(300.0));
        assert_eq!(settings["ui_font_weight"].as_f64(), Some(300.0));

        let unscaled = settings_with_font(Some("ABC Sans"), None, None);
        assert_eq!(unscaled["buffer_font_size"], 15);
        assert_eq!(unscaled["ui_font_weight"], 400);
        let unrenamed = settings_with_font(None, Some(1.5), None);
        assert_eq!(unrenamed["ui_font_family"], "Rho Font");
    }

    #[test]
    fn bold_weight_override_resolves_bold_and_semibold_to_that_weight() -> anyhow::Result<()> {
        use gpui::{FontWeight, TextSystem};
        use gpui_wgpu::CosmicTextSystem;

        let text_system = TextSystem::new(std::sync::Arc::new(
            CosmicTextSystem::new_without_system_fonts("sans-serif"),
        ));
        let regular = RhoEmbedded::get("fonts/rho-font/RhoFont-Regular.ttf")
            .context("embedded Rho Font")?
            .data;
        text_system.add_fonts(vec![regular])?;
        let weighted = |weight| gpui::Font {
            weight,
            ..gpui::font("Rho Font")
        };

        let bold = text_system.resolve_font(&weighted(FontWeight::BOLD));
        let medium = text_system.resolve_font(&weighted(FontWeight(550.0)));
        assert_ne!(bold, medium, "Rho Font varies its weight");

        let semibold = text_system.resolve_font(&weighted(FontWeight::SEMIBOLD));
        assert_ne!(semibold, medium);

        text_system.set_bold_weight(FontWeight(550.0));
        assert_eq!(
            text_system.resolve_font(&weighted(FontWeight::BOLD)),
            medium
        );
        assert_eq!(
            text_system.resolve_font(&weighted(FontWeight::SEMIBOLD)),
            medium,
            "Markdown's strong emphasis is semibold"
        );
        assert_ne!(
            text_system.resolve_font(&weighted(FontWeight::NORMAL)),
            medium,
            "only bold moves"
        );
        Ok(())
    }

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
        assert_eq!(style["text"], "oklch(70.9% 0.012 95)");
        assert_eq!(style["editor.foreground"], "oklch(70.9% 0.012 95)");
        assert_eq!(
            colors.text, colors.editor_foreground,
            "general and editor body text use the same foreground"
        );

        let foreground = wcag_relative_luminance(colors.editor_foreground);
        let background = wcag_relative_luminance(colors.editor_background);
        let contrast = (foreground + 0.05) / (background + 0.05);
        assert!(
            contrast >= 6.5,
            "body text contrast must remain at least 6.5:1, got {contrast:.2}:1"
        );
        assert!(
            (contrast - 6.509).abs() < 0.005,
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
        use gpui::{FontFallbacks, FontRun, PlatformTextSystem as _};
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

        for (description, text, explicit_fallback) in [
            ("skin-tone emoji", "👍🏽", false),
            ("skin-tone ZWJ emoji", "👩🏽‍💻", false),
            ("standard emoji", "🎉", false),
            ("emoji-presentation skull", "☠️", true),
            ("emoji-presentation frown", "☹️", true),
        ] {
            let mut font = gpui::font("Rho Font");
            if explicit_fallback {
                font.fallbacks = Some(FontFallbacks::from_fonts(vec!["Noto Color Emoji".into()]));
            }
            let primary_id = text_system.font_id(&font)?;
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
