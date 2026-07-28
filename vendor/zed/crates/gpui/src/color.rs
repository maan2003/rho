use anyhow::{Context as _, bail};
use schemars::{JsonSchema, json_schema};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, Visitor},
};
use std::borrow::Cow;
use std::{
    fmt::{self, Display, Formatter},
    hash::{Hash, Hasher},
};

/// Convert an RGB hex color code number to a color type
pub fn rgb(hex: u32) -> Rgba {
    let [_, r, g, b] = hex.to_be_bytes().map(|b| (b as f32) / 255.0);
    Rgba { r, g, b, a: 1.0 }
}

/// Convert an RGBA hex color code number to [`Rgba`]
pub fn rgba(hex: u32) -> Rgba {
    let [r, g, b, a] = hex.to_be_bytes().map(|b| (b as f32) / 255.0);
    Rgba { r, g, b, a }
}

/// Swap from RGBA with premultiplied alpha to BGRA
pub fn swap_rgba_pa_to_bgra(color: &mut [u8]) {
    color.swap(0, 2);
    if color[3] > 0 {
        let a = color[3] as f32 / 255.;
        color[0] = (color[0] as f32 / a) as u8;
        color[1] = (color[1] as f32 / a) as u8;
        color[2] = (color[2] as f32 / a) as u8;
    }
}

/// An RGBA color
#[derive(PartialEq, Clone, Copy, Default)]
#[repr(C)]
pub struct Rgba {
    /// The red component of the color, in the range 0.0 to 1.0
    pub r: f32,
    /// The green component of the color, in the range 0.0 to 1.0
    pub g: f32,
    /// The blue component of the color, in the range 0.0 to 1.0
    pub b: f32,
    /// The alpha component of the color, in the range 0.0 to 1.0
    pub a: f32,
}

/// A renderer-ready color in linear Display P3 space.
///
/// Components are linear-light Display P3 values. Alpha remains in
/// `0.0..=1.0`. Theme inputs in other color spaces are converted at the edge
/// so rendering and blending can use one wide-gamut working space.
#[derive(PartialEq, Clone, Copy, Default)]
#[repr(C)]
pub struct Color {
    /// Linear red component.
    pub r: f32,
    /// Linear green component.
    pub g: f32,
    /// Linear blue component.
    pub b: f32,
    /// Alpha component.
    pub a: f32,
}

impl fmt::Debug for Color {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "color(linear-display-p3 {:.4} {:.4} {:.4} / {:.4})",
            self.r, self.g, self.b, self.a
        )
    }
}

impl Color {
    /// Construct a color from linear Display P3 components.
    pub fn linear_display_p3(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self {
            r,
            g,
            b,
            a: a.clamp(0.0, 1.0),
        }
    }

    /// Construct a color from linear sRGB components.
    pub fn linear_srgb(r: f32, g: f32, b: f32, a: f32) -> Self {
        let [r, g, b] = linear_srgb_to_linear_display_p3([r, g, b]);
        Self::linear_display_p3(r, g, b, a)
    }

    /// Construct a color from Display P3 components.
    ///
    /// Components use the same transfer function as sRGB and are converted to
    /// linear Display P3 without gamut clipping.
    pub fn display_p3(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self::linear_display_p3(
            srgb_to_linear_component(r),
            srgb_to_linear_component(g),
            srgb_to_linear_component(b),
            a,
        )
    }

    /// Construct a color from OKLCH components.
    ///
    /// Lightness is in `0.0..=1.0`, chroma is non-negative, and hue is in degrees.
    pub fn oklch(l: f32, chroma: f32, hue_degrees: f32, a: f32) -> Self {
        let [r, g, b] = oklch_to_linear_srgb(l.clamp(0.0, 1.0), chroma.max(0.0), hue_degrees);
        Self::linear_srgb(r, g, b, a)
    }

    /// Returns this color with alpha multiplied by `factor`.
    pub fn opacity(self, factor: f32) -> Self {
        Self {
            a: self.a * factor.clamp(0.0, 1.0),
            ..self
        }
    }

    /// Returns this color with alpha replaced by `a`.
    pub fn alpha(self, a: f32) -> Self {
        Self {
            a: a.clamp(0.0, 1.0),
            ..self
        }
    }

    /// Returns true if this color is fully transparent.
    pub fn is_transparent(&self) -> bool {
        self.a == 0.0
    }

    /// Returns true if this color is fully opaque.
    pub fn is_opaque(&self) -> bool {
        self.a == 1.0
    }

    /// Alpha-blend `other` over this color in linear Display P3 space.
    pub fn blend(self, other: Color) -> Color {
        Color::linear_display_p3(
            self.r * (1.0 - other.a) + other.r * other.a,
            self.g * (1.0 - other.a) + other.g * other.a,
            self.b * (1.0 - other.a) + other.b * other.a,
            self.a,
        )
    }

    /// Fade out the color by `factor`, where `0.0` leaves it unchanged and
    /// `1.0` makes it fully transparent.
    pub fn fade_out(&mut self, factor: f32) {
        self.a *= 1.0 - factor.clamp(0.0, 1.0);
    }
}

impl fmt::Debug for Rgba {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "rgba({:#010x})", u32::from(*self))
    }
}

impl Rgba {
    /// Create a new [`Rgba`] color by blending this and another color together
    pub fn blend(&self, other: Rgba) -> Self {
        if other.a >= 1.0 {
            other
        } else if other.a <= 0.0 {
            *self
        } else {
            Rgba {
                r: (self.r * (1.0 - other.a)) + (other.r * other.a),
                g: (self.g * (1.0 - other.a)) + (other.g * other.a),
                b: (self.b * (1.0 - other.a)) + (other.b * other.a),
                a: self.a,
            }
        }
    }

    /// Returns a new RGBA color with the same red, green and blue channels, but
    /// with a new alpha value.
    ///
    /// Example:
    /// ```
    /// use gpui::rgba;
    /// let color = rgba(0xFF0000FF);
    /// let faded = color.alpha(0.25);
    /// assert_eq!(faded.a, 0.25);
    /// ```
    ///
    /// This will return a red color with 25% opacity.
    ///
    /// Example:
    /// ```
    /// use gpui::rgba;
    /// let color = rgba(0x3399FFCC);
    /// let transparent = color.alpha(0.0);
    /// assert_eq!(transparent.a, 0.0);
    /// ```
    ///
    /// This will return the same blue color, fully transparent.
    pub fn alpha(&self, a: f32) -> Self {
        Rgba {
            r: self.r,
            g: self.g,
            b: self.b,
            a: a.clamp(0., 1.),
        }
    }

    /// Returns a new RGBA color with the same red, green, and blue channels,
    /// but with the alpha channel multiplied by the given factor.
    ///
    /// Example:
    /// ```
    /// use gpui::rgba;
    /// let color = rgba(0xFF0000FF); // Fully opaque red
    /// let faded = color.opacity(0.5);
    /// assert_eq!(faded.a, 0.5);
    /// ```
    ///
    /// This will return a red color with 50% opacity.
    ///
    /// Example:
    /// ```
    /// use gpui::rgba;
    /// let color = rgba(0x3399FFCC); // A light blue with 80% opacity
    /// let faded = color.opacity(0.5);
    /// assert!((faded.a - 0.4).abs() < 1e-6);
    /// ```
    ///
    /// This will return the same blue color scaled down to 40% opacity.
    pub fn opacity(&self, factor: f32) -> Self {
        Rgba {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a * factor.clamp(0., 1.),
        }
    }
}

impl From<Rgba> for u32 {
    fn from(rgba: Rgba) -> Self {
        let r = (rgba.r * 255.0) as u32;
        let g = (rgba.g * 255.0) as u32;
        let b = (rgba.b * 255.0) as u32;
        let a = (rgba.a * 255.0) as u32;
        (r << 24) | (g << 16) | (b << 8) | a
    }
}

fn srgb_to_linear_component(srgb: f32) -> f32 {
    if srgb <= 0.04045 {
        srgb / 12.92
    } else {
        ((srgb + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_srgb_to_linear_display_p3([r, g, b]: [f32; 3]) -> [f32; 3] {
    [
        0.8224621 * r + 0.177538 * g,
        0.0331941 * r + 0.9668058 * g,
        0.0170827 * r + 0.0723974 * g + 0.9105199 * b,
    ]
}

fn linear_display_p3_to_linear_srgb([r, g, b]: [f32; 3]) -> [f32; 3] {
    [
        1.2247455 * r - 0.22490445 * g,
        -0.04205808 * r + 1.042081 * g,
        -0.01964226 * r - 0.07865488 * g + 1.0985371 * b,
    ]
}

fn linear_to_srgb_component(linear: f32) -> f32 {
    if linear <= 0.0031308 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

fn oklch_to_linear_srgb(lightness: f32, chroma: f32, hue_degrees: f32) -> [f32; 3] {
    let hue = hue_degrees.to_radians();
    let oklab_l = lightness;
    let oklab_a = chroma * hue.cos();
    let oklab_b = chroma * hue.sin();

    let l_ = oklab_l + 0.39633778 * oklab_a + 0.21580376 * oklab_b;
    let m_ = oklab_l - 0.105561346 * oklab_a - 0.06385417 * oklab_b;
    let s_ = oklab_l - 0.08948418 * oklab_a - 1.2914855 * oklab_b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    [
        4.0767417 * l - 3.3077116 * m + 0.23096994 * s,
        -1.268438 * l + 2.6097574 * m - 0.3413194 * s,
        -0.0041960863 * l - 0.7034186 * m + 1.7076147 * s,
    ]
}

struct RgbaVisitor;

impl Visitor<'_> for RgbaVisitor {
    type Value = Rgba;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a string in the format #rrggbb or #rrggbbaa")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Rgba, E> {
        Rgba::try_from(value).map_err(E::custom)
    }
}

impl JsonSchema for Rgba {
    fn schema_name() -> Cow<'static, str> {
        "Rgba".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        json_schema!({
            "type": "string",
            "pattern": "^#([0-9a-fA-F]{3}|[0-9a-fA-F]{4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$"
        })
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(RgbaVisitor)
    }
}

impl Serialize for Rgba {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let r = (self.r * 255.0).round() as u8;
        let g = (self.g * 255.0).round() as u8;
        let b = (self.b * 255.0).round() as u8;
        let a = (self.a * 255.0).round() as u8;

        let s = format!("#{r:02x}{g:02x}{b:02x}{a:02x}");
        serializer.serialize_str(&s)
    }
}

impl From<Hsla> for Rgba {
    fn from(color: Hsla) -> Self {
        let h = color.h;
        let s = color.s;
        let l = color.l;

        let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
        let x = c * (1.0 - ((h * 6.0) % 2.0 - 1.0).abs());
        let m = l - c / 2.0;
        let cm = c + m;
        let xm = x + m;

        let (r, g, b) = match (h * 6.0).floor() as i32 {
            0 | 6 => (cm, xm, m),
            1 => (xm, cm, m),
            2 => (m, cm, xm),
            3 => (m, xm, cm),
            4 => (xm, m, cm),
            _ => (cm, m, xm),
        };

        Rgba {
            r: r.clamp(0., 1.),
            g: g.clamp(0., 1.),
            b: b.clamp(0., 1.),
            a: color.a,
        }
    }
}

impl From<Rgba> for Color {
    fn from(color: Rgba) -> Self {
        Self::linear_srgb(
            srgb_to_linear_component(color.r),
            srgb_to_linear_component(color.g),
            srgb_to_linear_component(color.b),
            color.a,
        )
    }
}

impl From<Hsla> for Color {
    fn from(color: Hsla) -> Self {
        Rgba::from(color).into()
    }
}

impl From<Color> for Rgba {
    fn from(color: Color) -> Self {
        let [r, g, b] = linear_display_p3_to_linear_srgb([color.r, color.g, color.b]);
        Rgba {
            r: linear_to_srgb_component(r).clamp(0.0, 1.0),
            g: linear_to_srgb_component(g).clamp(0.0, 1.0),
            b: linear_to_srgb_component(b).clamp(0.0, 1.0),
            a: color.a,
        }
    }
}

impl From<Color> for Hsla {
    fn from(color: Color) -> Self {
        Rgba::from(color).into()
    }
}

impl TryFrom<&'_ str> for Rgba {
    type Error = anyhow::Error;

    fn try_from(value: &'_ str) -> Result<Self, Self::Error> {
        const RGB: usize = "rgb".len();
        const RGBA: usize = "rgba".len();
        const RRGGBB: usize = "rrggbb".len();
        const RRGGBBAA: usize = "rrggbbaa".len();

        const EXPECTED_FORMATS: &str = "Expected #rgb, #rgba, #rrggbb, or #rrggbbaa";
        const INVALID_UNICODE: &str = "invalid unicode characters in color";

        let Some(("", hex)) = value.trim().split_once('#') else {
            bail!("invalid RGBA hex color: '{value}'. {EXPECTED_FORMATS}");
        };

        let (r, g, b, a) = match hex.len() {
            RGB | RGBA => {
                let r = u8::from_str_radix(
                    hex.get(0..1).with_context(|| {
                        format!("{INVALID_UNICODE}: r component of #rgb/#rgba for value: '{value}'")
                    })?,
                    16,
                )?;
                let g = u8::from_str_radix(
                    hex.get(1..2).with_context(|| {
                        format!("{INVALID_UNICODE}: g component of #rgb/#rgba for value: '{value}'")
                    })?,
                    16,
                )?;
                let b = u8::from_str_radix(
                    hex.get(2..3).with_context(|| {
                        format!("{INVALID_UNICODE}: b component of #rgb/#rgba for value: '{value}'")
                    })?,
                    16,
                )?;
                let a = if hex.len() == RGBA {
                    u8::from_str_radix(
                        hex.get(3..4).with_context(|| {
                            format!("{INVALID_UNICODE}: a component of #rgba for value: '{value}'")
                        })?,
                        16,
                    )?
                } else {
                    0xf
                };

                /// Duplicates a given hex digit.
                /// E.g., `0xf` -> `0xff`.
                const fn duplicate(value: u8) -> u8 {
                    (value << 4) | value
                }

                (duplicate(r), duplicate(g), duplicate(b), duplicate(a))
            }
            RRGGBB | RRGGBBAA => {
                let r = u8::from_str_radix(
                    hex.get(0..2).with_context(|| {
                        format!(
                            "{}: r component of #rrggbb/#rrggbbaa for value: '{}'",
                            INVALID_UNICODE, value
                        )
                    })?,
                    16,
                )?;
                let g = u8::from_str_radix(
                    hex.get(2..4).with_context(|| {
                        format!(
                            "{INVALID_UNICODE}: g component of #rrggbb/#rrggbbaa for value: '{value}'"
                        )
                    })?,
                    16,
                )?;
                let b = u8::from_str_radix(
                    hex.get(4..6).with_context(|| {
                        format!(
                            "{INVALID_UNICODE}: b component of #rrggbb/#rrggbbaa for value: '{value}'"
                        )
                    })?,
                    16,
                )?;
                let a = if hex.len() == RRGGBBAA {
                    u8::from_str_radix(
                        hex.get(6..8).with_context(|| {
                            format!(
                                "{INVALID_UNICODE}: a component of #rrggbbaa for value: '{value}'"
                            )
                        })?,
                        16,
                    )?
                } else {
                    0xff
                };
                (r, g, b, a)
            }
            _ => bail!("invalid RGBA hex color: '{value}'. {EXPECTED_FORMATS}"),
        };

        Ok(Rgba {
            r: r as f32 / 255.,
            g: g as f32 / 255.,
            b: b as f32 / 255.,
            a: a as f32 / 255.,
        })
    }
}

impl JsonSchema for Color {
    fn schema_name() -> Cow<'static, str> {
        "Color".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        json_schema!({
            "type": "string",
            "description": "#rgb/#rgba/#rrggbb/#rrggbbaa, oklch(<lightness> <chroma> <hue> [/ <alpha>]), or color(display-p3 <red> <green> <blue> [/ <alpha>])"
        })
    }
}

impl Serialize for Color {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Rgba::from(*self).serialize(serializer)
    }
}

impl TryFrom<&'_ str> for Color {
    type Error = anyhow::Error;

    fn try_from(value: &'_ str) -> Result<Self, Self::Error> {
        let value = value.trim();
        if let Some(color) = parse_oklch(value)? {
            Ok(color)
        } else if let Some(color) = parse_display_p3(value)? {
            Ok(color)
        } else {
            Ok(Rgba::try_from(value)?.into())
        }
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ColorVisitor;

        impl Visitor<'_> for ColorVisitor {
            type Value = Color;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a hex color, oklch(<lightness> <chroma> <hue> [/ <alpha>]), or color(display-p3 <red> <green> <blue> [/ <alpha>])")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Color, E> {
                Color::try_from(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(ColorVisitor)
    }
}

fn parse_oklch(value: &str) -> anyhow::Result<Option<Color>> {
    let Some(mut body) = value.strip_prefix("oklch(") else {
        return Ok(None);
    };
    body = body
        .strip_suffix(')')
        .ok_or_else(|| anyhow::anyhow!("invalid OKLCH color: missing closing ')'"))?;
    let body = body.replace('/', " / ");
    let mut lightness = None;
    let mut chroma = None;
    let mut hue = None;
    let mut alpha = 1.0;
    let mut after_slash = false;

    for part in body.split_whitespace() {
        if part == "/" {
            after_slash = true;
            continue;
        }
        if after_slash {
            alpha = parse_css_number_or_percent(part)?;
            after_slash = false;
        } else if lightness.is_none() {
            lightness = Some(parse_css_number_or_percent(part)?);
        } else if chroma.is_none() {
            chroma = Some(parse_css_number(part)?);
        } else if hue.is_none() {
            hue = Some(parse_css_number(part.trim_end_matches("deg"))?);
        } else {
            anyhow::bail!("invalid OKLCH color: too many components");
        }
    }

    let lightness =
        lightness.ok_or_else(|| anyhow::anyhow!("invalid OKLCH color: missing lightness"))?;
    let chroma = chroma.ok_or_else(|| anyhow::anyhow!("invalid OKLCH color: missing chroma"))?;
    let hue = hue.ok_or_else(|| anyhow::anyhow!("invalid OKLCH color: missing hue"))?;
    Ok(Some(Color::oklch(lightness, chroma, hue, alpha)))
}

fn parse_display_p3(value: &str) -> anyhow::Result<Option<Color>> {
    let Some(mut body) = value.strip_prefix("color(") else {
        return Ok(None);
    };
    body = body
        .strip_suffix(')')
        .ok_or_else(|| anyhow::anyhow!("invalid color() value: missing closing ')'"))?;
    let Some(body) = body.trim().strip_prefix("display-p3") else {
        return Ok(None);
    };

    let body = body.replace('/', " / ");
    let mut red = None;
    let mut green = None;
    let mut blue = None;
    let mut alpha = 1.0;
    let mut after_slash = false;

    for part in body.split_whitespace() {
        if part == "/" {
            after_slash = true;
            continue;
        }
        if after_slash {
            alpha = parse_css_number_or_percent(part)?;
            after_slash = false;
        } else if red.is_none() {
            red = Some(parse_css_number_or_percent(part)?);
        } else if green.is_none() {
            green = Some(parse_css_number_or_percent(part)?);
        } else if blue.is_none() {
            blue = Some(parse_css_number_or_percent(part)?);
        } else {
            anyhow::bail!("invalid display-p3 color: too many components");
        }
    }

    let red = red.ok_or_else(|| anyhow::anyhow!("invalid display-p3 color: missing red"))?;
    let green = green.ok_or_else(|| anyhow::anyhow!("invalid display-p3 color: missing green"))?;
    let blue = blue.ok_or_else(|| anyhow::anyhow!("invalid display-p3 color: missing blue"))?;
    Ok(Some(Color::display_p3(red, green, blue, alpha)))
}

fn parse_css_number(value: &str) -> anyhow::Result<f32> {
    Ok(value.parse()?)
}

fn parse_css_number_or_percent(value: &str) -> anyhow::Result<f32> {
    if let Some(percent) = value.strip_suffix('%') {
        Ok(percent.parse::<f32>()? / 100.0)
    } else {
        parse_css_number(value)
    }
}

/// An HSLA color
#[derive(Default, Copy, Clone, Debug)]
#[repr(C)]
pub struct Hsla {
    /// Hue, in a range from 0 to 1
    pub h: f32,

    /// Saturation, in a range from 0 to 1
    pub s: f32,

    /// Lightness, in a range from 0 to 1
    pub l: f32,

    /// Alpha, in a range from 0 to 1
    pub a: f32,
}

#[cfg(feature = "proptest")]
mod property {
    use super::Hsla;
    use proptest::prelude::*;

    impl Hsla {
        /// Proptest [`Strategy`] that produces opaque colors (i.e. alpha = 1).
        ///
        /// For truly arbitrary colors, use the [`Arbitrary`] implementation.
        pub fn opaque_strategy() -> impl Strategy<Value = Self> {
            (0.0f32..=1.0, 0.0f32..=1.0, 0.0f32..=1.0).prop_map(|(h, s, l)| Hsla { h, s, l, a: 1. })
        }
    }

    impl Arbitrary for Hsla {
        type Strategy = BoxedStrategy<Self>;
        type Parameters = ();

        fn arbitrary_with((): Self::Parameters) -> Self::Strategy {
            (0.0f32..=1.0, 0.0f32..=1.0, 0.0f32..=1.0, 0.0f32..=1.0)
                .prop_map(|(h, s, l, a)| Hsla { h, s, l, a })
                .boxed()
        }
    }
}

impl PartialEq for Hsla {
    fn eq(&self, other: &Self) -> bool {
        self.h
            .total_cmp(&other.h)
            .then(self.s.total_cmp(&other.s))
            .then(self.l.total_cmp(&other.l).then(self.a.total_cmp(&other.a)))
            .is_eq()
    }
}

impl PartialOrd for Hsla {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hsla {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.h
            .total_cmp(&other.h)
            .then(self.s.total_cmp(&other.s))
            .then(self.l.total_cmp(&other.l).then(self.a.total_cmp(&other.a)))
    }
}

impl Eq for Hsla {}

impl Hash for Hsla {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u32(u32::from_be_bytes(self.h.to_be_bytes()));
        state.write_u32(u32::from_be_bytes(self.s.to_be_bytes()));
        state.write_u32(u32::from_be_bytes(self.l.to_be_bytes()));
        state.write_u32(u32::from_be_bytes(self.a.to_be_bytes()));
    }
}

impl Display for Hsla {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hsla({:.2}, {:.2}%, {:.2}%, {:.2})",
            self.h * 360.,
            self.s * 100.,
            self.l * 100.,
            self.a
        )
    }
}

/// Construct an [`Hsla`] object from plain values
pub const fn hsla(h: f32, s: f32, l: f32, a: f32) -> Hsla {
    Hsla {
        h: h.clamp(0., 1.),
        s: s.clamp(0., 1.),
        l: l.clamp(0., 1.),
        a: a.clamp(0., 1.),
    }
}

/// Pure black in [`Hsla`]
pub const fn black() -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 0.,
        a: 1.,
    }
}

/// Transparent black in [`Hsla`]
pub const fn transparent_black() -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 0.,
        a: 0.,
    }
}

/// Transparent white in [`Hsla`]
pub const fn transparent_white() -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 1.,
        a: 0.,
    }
}

/// Opaque grey in [`Hsla`], values will be clamped to the range [0, 1]
pub const fn opaque_grey(lightness: f32, opacity: f32) -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: lightness.clamp(0., 1.),
        a: opacity.clamp(0., 1.),
    }
}

/// Pure white in [`Hsla`]
pub const fn white() -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 1.,
        a: 1.,
    }
}

/// The color red in [`Hsla`]
pub const fn red() -> Hsla {
    Hsla {
        h: 0.,
        s: 1.,
        l: 0.5,
        a: 1.,
    }
}

/// The color blue in [`Hsla`]
pub const fn blue() -> Hsla {
    Hsla {
        h: 0.6666666667,
        s: 1.,
        l: 0.5,
        a: 1.,
    }
}

/// The color green in [`Hsla`]
pub const fn green() -> Hsla {
    Hsla {
        h: 0.3333333333,
        s: 1.,
        l: 0.25,
        a: 1.,
    }
}

/// The color yellow in [`Hsla`]
pub const fn yellow() -> Hsla {
    Hsla {
        h: 0.1666666667,
        s: 1.,
        l: 0.5,
        a: 1.,
    }
}

impl Hsla {
    /// Converts this HSLA color to an RGBA color.
    pub fn to_rgb(self) -> Rgba {
        self.into()
    }

    /// The color red
    pub const fn red() -> Self {
        red()
    }

    /// The color green
    pub const fn green() -> Self {
        green()
    }

    /// The color blue
    pub const fn blue() -> Self {
        blue()
    }

    /// The color black
    pub const fn black() -> Self {
        black()
    }

    /// The color white
    pub const fn white() -> Self {
        white()
    }

    /// The color transparent black
    pub const fn transparent_black() -> Self {
        transparent_black()
    }

    /// Returns true if the HSLA color is fully transparent, false otherwise.
    pub fn is_transparent(&self) -> bool {
        self.a == 0.0
    }

    /// Returns true if the HSLA color is fully opaque, false otherwise.
    pub fn is_opaque(&self) -> bool {
        self.a == 1.0
    }

    /// Blends `other` on top of `self` based on `other`'s alpha value. The resulting color is a combination of `self`'s and `other`'s colors.
    ///
    /// If `other`'s alpha value is 1.0 or greater, `other` color is fully opaque, thus `other` is returned as the output color.
    /// If `other`'s alpha value is 0.0 or less, `other` color is fully transparent, thus `self` is returned as the output color.
    /// Else, the output color is calculated as a blend of `self` and `other` based on their weighted alpha values.
    ///
    /// Assumptions:
    /// - Alpha values are contained in the range [0, 1], with 1 as fully opaque and 0 as fully transparent.
    /// - The relative contributions of `self` and `other` is based on `self`'s alpha value (`self.a`) and `other`'s  alpha value (`other.a`), `self` contributing `self.a * (1.0 - other.a)` and `other` contributing its own alpha value.
    /// - RGB color components are contained in the range [0, 1].
    /// - If `self` and `other` colors are out of the valid range, the blend operation's output and behavior is undefined.
    pub fn blend(self, other: Hsla) -> Hsla {
        let alpha = other.a;

        if alpha >= 1.0 {
            other
        } else if alpha <= 0.0 {
            self
        } else {
            let converted_self = Rgba::from(self);
            let converted_other = Rgba::from(other);
            let blended_rgb = converted_self.blend(converted_other);
            Hsla::from(blended_rgb)
        }
    }

    /// Returns a new HSLA color with the same hue, and lightness, but with no saturation.
    pub fn grayscale(&self) -> Self {
        Hsla {
            h: self.h,
            s: 0.,
            l: self.l,
            a: self.a,
        }
    }

    /// Fade out the color by a given factor. This factor should be between 0.0 and 1.0.
    /// Where 0.0 will leave the color unchanged, and 1.0 will completely fade out the color.
    pub fn fade_out(&mut self, factor: f32) {
        self.a *= 1.0 - factor.clamp(0., 1.);
    }

    /// Multiplies the alpha value of the color by a given factor
    /// and returns a new HSLA color.
    ///
    /// Useful for transforming colors with dynamic opacity,
    /// like a color from an external source.
    ///
    /// Example:
    /// ```
    /// let color = gpui::red();
    /// let faded_color = color.opacity(0.5);
    /// assert_eq!(faded_color.a, 0.5);
    /// ```
    ///
    /// This will return a red color with half the opacity.
    ///
    /// Example:
    /// ```
    /// use gpui::hsla;
    /// let color = hsla(0.7, 1.0, 0.5, 0.7); // A saturated blue
    /// let faded_color = color.opacity(0.16);
    /// assert!((faded_color.a - 0.112).abs() < 1e-6);
    /// ```
    ///
    /// This will return a blue color with around ~10% opacity,
    /// suitable for an element's hover or selected state.
    ///
    pub fn opacity(&self, factor: f32) -> Self {
        Hsla {
            h: self.h,
            s: self.s,
            l: self.l,
            a: self.a * factor.clamp(0., 1.),
        }
    }

    /// Returns a new HSLA color with the same hue, saturation,
    /// and lightness, but with a new alpha value.
    ///
    /// Example:
    /// ```
    /// let color = gpui::red();
    /// let red_color = color.alpha(0.25);
    /// assert_eq!(red_color.a, 0.25);
    /// ```
    ///
    /// This will return a red color with 25% opacity.
    ///
    /// Example:
    /// ```
    /// use gpui::hsla;
    /// let color = hsla(0.7, 1.0, 0.5, 0.7); // A saturated blue
    /// let faded_color = color.alpha(0.25);
    /// assert_eq!(faded_color.a, 0.25);
    /// ```
    ///
    /// This will return a blue color with 25% opacity.
    pub fn alpha(&self, a: f32) -> Self {
        Hsla {
            h: self.h,
            s: self.s,
            l: self.l,
            a: a.clamp(0., 1.),
        }
    }
}

impl From<Rgba> for Hsla {
    fn from(color: Rgba) -> Self {
        let r = color.r;
        let g = color.g;
        let b = color.b;

        let max = r.max(g.max(b));
        let min = r.min(g.min(b));
        let delta = max - min;

        let l = (max + min) / 2.0;
        let s = if l == 0.0 || l == 1.0 {
            0.0
        } else if l < 0.5 {
            delta / (2.0 * l)
        } else {
            delta / (2.0 - 2.0 * l)
        };

        let h = if delta == 0.0 {
            0.0
        } else if max == r {
            ((g - b) / delta).rem_euclid(6.0) / 6.0
        } else if max == g {
            ((b - r) / delta + 2.0) / 6.0
        } else {
            ((r - g) / delta + 4.0) / 6.0
        };

        Hsla {
            h,
            s,
            l,
            a: color.a,
        }
    }
}

impl JsonSchema for Hsla {
    fn schema_name() -> Cow<'static, str> {
        Rgba::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        Rgba::json_schema(generator)
    }
}

impl Serialize for Hsla {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Rgba::from(*self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Hsla {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Rgba::deserialize(deserializer)?.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub(crate) enum BackgroundTag {
    Solid = 0,
    LinearGradient = 1,
    PatternSlash = 2,
    Checkerboard = 3,
}

/// A color space for color interpolation.
///
/// References:
/// - <https://developer.mozilla.org/en-US/docs/Web/CSS/color-interpolation-method>
/// - <https://www.w3.org/TR/css-color-4/#typedef-color-space>
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub enum ColorSpace {
    #[default]
    /// The sRGB color space.
    Srgb = 0,
    /// The Oklab color space.
    Oklab = 1,
}

impl Display for ColorSpace {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ColorSpace::Srgb => write!(f, "sRGB"),
            ColorSpace::Oklab => write!(f, "Oklab"),
        }
    }
}

/// A background color, which can be either a solid color or a linear gradient.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub struct Background {
    pub(crate) tag: BackgroundTag,
    pub(crate) color_space: ColorSpace,
    pub(crate) solid: Color,
    pub(crate) gradient_angle_or_pattern_height: f32,
    pub(crate) colors: [LinearColorStop; 2],
    /// Padding for alignment for repr(C) layout.
    pad: u32,
}

impl std::fmt::Debug for Background {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.tag {
            BackgroundTag::Solid => write!(f, "Solid({:?})", self.solid),
            BackgroundTag::LinearGradient => write!(
                f,
                "LinearGradient({}, {:?}, {:?})",
                self.gradient_angle_or_pattern_height, self.colors[0], self.colors[1]
            ),
            BackgroundTag::PatternSlash => write!(
                f,
                "PatternSlash({:?}, {})",
                self.solid, self.gradient_angle_or_pattern_height
            ),
            BackgroundTag::Checkerboard => write!(
                f,
                "Checkerboard({:?}, {})",
                self.solid, self.gradient_angle_or_pattern_height
            ),
        }
    }
}

impl Eq for Background {}
impl Default for Background {
    fn default() -> Self {
        Self {
            tag: BackgroundTag::Solid,
            solid: Color::default(),
            color_space: ColorSpace::default(),
            gradient_angle_or_pattern_height: 0.0,
            colors: [LinearColorStop::default(), LinearColorStop::default()],
            pad: 0,
        }
    }
}

/// Creates a hash pattern background
pub fn pattern_slash(color: impl Into<Color>, width: f32, interval: f32) -> Background {
    let width_scaled = (width * 255.0) as u32;
    let interval_scaled = (interval * 255.0) as u32;
    let height = ((width_scaled * 0xFFFF) + interval_scaled) as f32;

    Background {
        tag: BackgroundTag::PatternSlash,
        solid: color.into(),
        gradient_angle_or_pattern_height: height,
        ..Default::default()
    }
}

/// Creates a checkerboard pattern background
pub fn checkerboard(color: impl Into<Color>, size: f32) -> Background {
    Background {
        tag: BackgroundTag::Checkerboard,
        solid: color.into(),
        gradient_angle_or_pattern_height: size,
        ..Default::default()
    }
}

/// Creates a solid background color.
pub fn solid_background(color: impl Into<Color>) -> Background {
    Background {
        solid: color.into(),
        ..Default::default()
    }
}

/// Creates a LinearGradient background color.
///
/// The gradient line's angle of direction. A value of `0.` is equivalent to top; increasing values rotate clockwise from there.
///
/// The `angle` is in degrees value in the range 0.0 to 360.0.
///
/// <https://developer.mozilla.org/en-US/docs/Web/CSS/gradient/linear-gradient>
pub fn linear_gradient(
    angle: f32,
    from: impl Into<LinearColorStop>,
    to: impl Into<LinearColorStop>,
) -> Background {
    Background {
        tag: BackgroundTag::LinearGradient,
        gradient_angle_or_pattern_height: angle,
        colors: [from.into(), to.into()],
        ..Default::default()
    }
}

/// A color stop in a linear gradient.
///
/// <https://developer.mozilla.org/en-US/docs/Web/CSS/gradient/linear-gradient#linear-color-stop>
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub struct LinearColorStop {
    /// The color of the color stop.
    pub color: Color,
    /// The percentage of the gradient, in the range 0.0 to 1.0.
    pub percentage: f32,
}

/// Creates a new linear color stop.
///
/// The percentage of the gradient, in the range 0.0 to 1.0.
pub fn linear_color_stop(color: impl Into<Color>, percentage: f32) -> LinearColorStop {
    LinearColorStop {
        color: color.into(),
        percentage,
    }
}

impl LinearColorStop {
    /// Returns a new color stop with the same color, but with a modified alpha value.
    pub fn opacity(&self, factor: f32) -> Self {
        Self {
            percentage: self.percentage,
            color: self.color.opacity(factor),
        }
    }
}

impl Background {
    /// Returns the solid color if this is a solid background, None otherwise.
    pub fn as_solid(&self) -> Option<Color> {
        if self.tag == BackgroundTag::Solid {
            Some(self.solid)
        } else {
            None
        }
    }

    /// Use specified color space for color interpolation.
    ///
    /// <https://developer.mozilla.org/en-US/docs/Web/CSS/color-interpolation-method>
    pub fn color_space(mut self, color_space: ColorSpace) -> Self {
        self.color_space = color_space;
        self
    }

    /// Returns a new background color with the same hue, saturation, and lightness, but with a modified alpha value.
    pub fn opacity(&self, factor: f32) -> Self {
        let mut background = *self;
        background.solid = background.solid.opacity(factor);
        background.colors = [
            self.colors[0].opacity(factor),
            self.colors[1].opacity(factor),
        ];
        background
    }

    /// Returns whether the background color is transparent.
    pub fn is_transparent(&self) -> bool {
        match self.tag {
            BackgroundTag::Solid => self.solid.is_transparent(),
            BackgroundTag::LinearGradient => self.colors.iter().all(|c| c.color.is_transparent()),
            BackgroundTag::PatternSlash => self.solid.is_transparent(),
            BackgroundTag::Checkerboard => self.solid.is_transparent(),
        }
    }
}

impl From<Color> for Background {
    fn from(value: Color) -> Self {
        Background {
            tag: BackgroundTag::Solid,
            solid: value,
            ..Default::default()
        }
    }
}

impl From<Hsla> for Background {
    fn from(value: Hsla) -> Self {
        Color::from(value).into()
    }
}

impl From<Rgba> for Background {
    fn from(value: Rgba) -> Self {
        Color::from(value).into()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_deserialize_three_value_hex_to_rgba() {
        let actual: Rgba = serde_json::from_value(json!("#f09")).unwrap();

        assert_eq!(actual, rgba(0xff0099ff))
    }

    #[test]
    fn test_deserialize_four_value_hex_to_rgba() {
        let actual: Rgba = serde_json::from_value(json!("#f09f")).unwrap();

        assert_eq!(actual, rgba(0xff0099ff))
    }

    #[test]
    fn test_deserialize_six_value_hex_to_rgba() {
        let actual: Rgba = serde_json::from_value(json!("#ff0099")).unwrap();

        assert_eq!(actual, rgba(0xff0099ff))
    }

    #[test]
    fn test_deserialize_eight_value_hex_to_rgba() {
        let actual: Rgba = serde_json::from_value(json!("#ff0099ff")).unwrap();

        assert_eq!(actual, rgba(0xff0099ff))
    }

    #[test]
    fn test_deserialize_eight_value_hex_with_padding_to_rgba() {
        let actual: Rgba = serde_json::from_value(json!(" #f5f5f5ff   ")).unwrap();

        assert_eq!(actual, rgba(0xf5f5f5ff))
    }

    #[test]
    fn test_deserialize_eight_value_hex_with_mixed_case_to_rgba() {
        let actual: Rgba = serde_json::from_value(json!("#DeAdbEeF")).unwrap();

        assert_eq!(actual, rgba(0xdeadbeef))
    }

    #[test]
    fn test_deserialize_oklch_to_color() {
        let actual: Color = serde_json::from_value(json!("oklch(72.5% 0.18 42 / 0.8)")).unwrap();
        let expected = Color::oklch(0.725, 0.18, 42.0, 0.8);

        assert!((actual.r - expected.r).abs() < 1e-6);
        assert!((actual.g - expected.g).abs() < 1e-6);
        assert!((actual.b - expected.b).abs() < 1e-6);
        assert!((actual.a - expected.a).abs() < 1e-6);
    }

    #[test]
    fn test_deserialize_hex_to_linear_color() {
        let actual: Color = serde_json::from_value(json!("#80808080")).unwrap();

        assert!((actual.r - 0.21586053).abs() < 1e-6);
        assert!((actual.g - 0.21586053).abs() < 1e-6);
        assert!((actual.b - 0.21586053).abs() < 1e-6);
        assert!((actual.a - 0.5019608).abs() < 1e-6);
    }

    #[test]
    fn test_deserialize_display_p3_to_linear_display_p3_color() {
        let actual: Color = serde_json::from_value(json!("color(display-p3 1 0 0 / 75%)")).unwrap();
        let expected = Color::display_p3(1.0, 0.0, 0.0, 0.75);

        assert!((actual.r - expected.r).abs() < 1e-6);
        assert!((actual.g - expected.g).abs() < 1e-6);
        assert!((actual.b - expected.b).abs() < 1e-6);
        assert!((actual.a - expected.a).abs() < 1e-6);
        assert!(actual.r > 1.0);
        assert!(actual.g < 0.0);
    }

    #[test]
    fn test_background_solid() {
        let color = Color::from(rgba(0xff0099ff));
        let mut background = Background::from(color);
        assert_eq!(background.tag, BackgroundTag::Solid);
        assert_eq!(background.solid, color);

        assert_eq!(background.opacity(0.5).solid, color.opacity(0.5));
        assert!(!background.is_transparent());
        background.solid = Color::from(hsla(0.0, 0.0, 0.0, 0.0));
        assert!(background.is_transparent());
    }

    #[test]
    fn test_background_linear_gradient() {
        let from = linear_color_stop(rgba(0xff0099ff), 0.0);
        let to = linear_color_stop(rgba(0x00ff99ff), 1.0);
        let background = linear_gradient(90.0, from, to);
        assert_eq!(background.tag, BackgroundTag::LinearGradient);
        assert_eq!(background.colors[0], from);
        assert_eq!(background.colors[1], to);

        assert_eq!(background.opacity(0.5).colors[0], from.opacity(0.5));
        assert_eq!(background.opacity(0.5).colors[1], to.opacity(0.5));
        assert!(!background.is_transparent());
        assert!(background.opacity(0.0).is_transparent());
    }

    #[test]
    fn test_rgba_alpha() {
        let color = Rgba {
            r: 0.2,
            g: 0.6,
            b: 1.0,
            a: 0.8,
        };

        assert_eq!(color.alpha(0.25).a, 0.25);
        assert_eq!(color.alpha(1.5).a, 1.0);
    }

    #[test]
    fn test_rgba_opacity() {
        let color = Rgba {
            r: 0.2,
            g: 0.6,
            b: 1.0,
            a: 0.8,
        };
        assert!((color.opacity(0.5).a - 0.4).abs() < 1e-6);
        assert_eq!(color.opacity(2.0).a, 0.8);
    }
}
