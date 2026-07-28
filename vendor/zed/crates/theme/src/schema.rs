#![allow(missing_docs)]

use gpui::Color;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The appearance of a theme in serialized content.
#[derive(Debug, PartialEq, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppearanceContent {
    Light,
    Dark,
}

/// Parses a color string into a [`Color`] value.
pub fn try_parse_color(color: &str) -> anyhow::Result<Color> {
    Color::try_from(color)
}
