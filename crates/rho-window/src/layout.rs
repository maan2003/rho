//! The window's one geometry (`design.token space`): the top row split
//! around the laptop's camera notch, the centered sheet every screen reads
//! on, and the measures prose and the prompt wrap at. Every screen lays
//! itself out from these, so a page, the prompt and the Desk share one
//! left edge and one top band.

use gpui::prelude::*;
use gpui::{Div, Pixels, TextStyle, Window, div, px};

/// The widest a screen's sheet is, in `ch` of the editor font: a calm
/// margin on a 13" laptop, one straight left edge for every block.
pub const SHEET_MAX_CH: f32 = 128.;
/// Where prose wraps inside the sheet; code, tables and images take the
/// whole sheet.
pub const PROSE_MAX_CH: f32 = 80.;
/// How wide the floating prompt panel (`Places`, `Commands`) is.
pub const PROMPT_MAX_CH: f32 = 104.;
/// How wide the Desk's table is: a narrower sheet, so a row's name and
/// its state read together.
pub const DESK_MAX_CH: f32 = 92.;
/// The black mask over the camera notch, centered on the top row.
pub const NOTCH_MASK_PX: f32 = 210.;
/// The top row: the page's identity left of the notch, the global status
/// bar right of it.
pub const TOP_ROW_PX: f32 = 34.;
/// How far the prompt panel floats below the top row: its top at `92 px`,
/// where the eye rests on the start of the agent's reply.
pub const PROMPT_GAP_PX: f32 = 58.;
/// Air between the top row and a sheet's first row.
pub const TOP_GAP_PX: f32 = 14.;
/// Breathing room between the window edge and the work.
pub const READING_INSET_PX: f32 = 24.;

/// The width of one `ch` of `text_style`: the advance of `0`.
pub fn ch(text_style: &TextStyle, window: &Window) -> Pixels {
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    let font_id = window.text_system().resolve_font(&text_style.font());
    window
        .text_system()
        .advance(font_id, font_size, '0')
        .map_or(font_size * 0.6, |advance| advance.width)
}

/// Where the notch mask spans across a window `width` wide: the top row
/// left of it belongs to the page, the rest right of it to the global
/// status bar. A screen that draws its own top-left corner keeps its words
/// left of `start`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Notch {
    pub start: Pixels,
    pub end: Pixels,
}

impl Notch {
    pub fn of(width: Pixels) -> Self {
        let mask = px(NOTCH_MASK_PX).min(width);
        let start = ((width - mask) / 2.).max(px(0.));
        Self {
            start,
            end: start + mask,
        }
    }
}

/// Where a centered sheet `sheet` wide starts in a window `viewport`
/// wide: a screen's heading starts there too, and never nearer the
/// window's edge than the reading inset.
pub fn sheet_left(viewport: Pixels, sheet: Pixels) -> Pixels {
    ((viewport - sheet) / 2.).max(px(READING_INSET_PX))
}

/// The room a heading starting at `left` has before the notch, less the
/// band gap: zero when the sheet starts past it.
pub fn heading_room(left: Pixels, notch: Notch) -> Pixels {
    (notch.start - px(BAND_GAP_PX) - left).max(px(0.))
}

/// `same band gap`: air between items in a top ear, and before the notch.
pub const BAND_GAP_PX: f32 = 12.;

/// The centered sheet: at most [`SHEET_MAX_CH`] wide, with the reading
/// inset on either side when the window is narrower than that.
pub fn sheet(ch: Pixels) -> Div {
    div()
        .w_full()
        .max_w(ch * SHEET_MAX_CH + px(READING_INSET_PX) * 2.)
        .mx_auto()
        .px(px(READING_INSET_PX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notch_is_centered_and_never_wider_than_the_window() {
        let laptop = Notch::of(px(1470.));
        assert_eq!(laptop.start, px(630.));
        assert_eq!(laptop.end, px(840.));
        let tiny = Notch::of(px(100.));
        assert_eq!((tiny.start, tiny.end), (px(0.), px(100.)));
    }

    #[test]
    fn a_heading_starts_where_the_sheet_does_and_stops_before_the_notch() {
        let laptop = Notch::of(px(1470.));
        // A 1100 px sheet on a 1470 px window starts 185 px in.
        assert_eq!(sheet_left(px(1470.), px(1100.)), px(185.));
        assert_eq!(heading_room(px(185.), laptop), px(630. - 12. - 185.));
        // A window narrower than the sheet keeps the reading inset.
        assert_eq!(sheet_left(px(800.), px(1100.)), px(READING_INSET_PX));
        // A sheet that starts past the notch leaves the heading no room.
        assert_eq!(heading_room(px(700.), laptop), px(0.));
    }
}
