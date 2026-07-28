#![allow(missing_docs)]

use gpui::{App, Color, Hsla, SharedString, WindowBackgroundAppearance};
use refineable::Refineable;
use std::sync::Arc;
use strum::{AsRefStr, EnumIter, IntoEnumIterator};

use crate::{
    AccentColors, ActiveTheme, PlayerColors, StatusColors, StatusColorsRefinement, SyntaxTheme,
    SystemColors,
};

#[derive(Refineable, Clone, Debug, PartialEq)]
#[refineable(Debug, serde::Deserialize)]
pub struct ThemeColors {
    /// Border color. Used for most borders, is usually a high contrast color.
    pub border: Color,
    /// Border color. Used for deemphasized borders, like a visual divider between two sections
    pub border_variant: Color,
    /// Border color. Used for focused elements, like keyboard focused list item.
    pub border_focused: Color,
    /// Border color. Used for selected elements, like an active search filter or selected checkbox.
    pub border_selected: Color,
    /// Border color. Used for transparent borders. Used for placeholder borders when an element gains a border on state change.
    pub border_transparent: Color,
    /// Border color. Used for disabled elements, like a disabled input or button.
    pub border_disabled: Color,
    /// Border color. Used for elevated surfaces, like a context menu, popup, or dialog.
    pub elevated_surface_background: Color,
    /// Background Color. Used for grounded surfaces like a panel or tab.
    pub surface_background: Color,
    /// Background Color. Used for the app background and blank panels or windows.
    pub background: Color,
    /// Background Color. Used for the background of an element that should have a different background than the surface it's on.
    ///
    /// Elements might include: Buttons, Inputs, Checkboxes, Radio Buttons...
    ///
    /// For an element that should have the same background as the surface it's on, use `ghost_element_background`.
    pub element_background: Color,
    /// Background Color. Used for the hover state of an element that should have a different background than the surface it's on.
    ///
    /// Hover states are triggered by the mouse entering an element, or a finger touching an element on a touch screen.
    pub element_hover: Color,
    /// Background Color. Used for the active state of an element that should have a different background than the surface it's on.
    ///
    /// Active states are triggered by the mouse button being pressed down on an element, or the Return button or other activator being pressed.
    pub element_active: Color,
    /// Background Color. Used for the selected state of an element that should have a different background than the surface it's on.
    ///
    /// Selected states are triggered by the element being selected (or "activated") by the user.
    ///
    /// This could include a selected checkbox, a toggleable button that is toggled on, etc.
    pub element_selected: Color,
    /// Background Color. Used for the background of selections in a UI element.
    pub element_selection_background: Color,
    /// Background Color. Used for the disabled state of an element that should have a different background than the surface it's on.
    ///
    /// Disabled states are shown when a user cannot interact with an element, like a disabled button or input.
    pub element_disabled: Color,
    /// Background Color. Used for the area that shows where a dragged element will be dropped.
    pub drop_target_background: Color,
    /// Border Color. Used for the border that shows where a dragged element will be dropped.
    pub drop_target_border: Color,
    /// Used for the background of a ghost element that should have the same background as the surface it's on.
    ///
    /// Elements might include: Buttons, Inputs, Checkboxes, Radio Buttons...
    ///
    /// For an element that should have a different background than the surface it's on, use `element_background`.
    pub ghost_element_background: Color,
    /// Background Color. Used for the hover state of a ghost element that should have the same background as the surface it's on.
    ///
    /// Hover states are triggered by the mouse entering an element, or a finger touching an element on a touch screen.
    pub ghost_element_hover: Color,
    /// Background Color. Used for the active state of a ghost element that should have the same background as the surface it's on.
    ///
    /// Active states are triggered by the mouse button being pressed down on an element, or the Return button or other activator being pressed.
    pub ghost_element_active: Color,
    /// Background Color. Used for the selected state of a ghost element that should have the same background as the surface it's on.
    ///
    /// Selected states are triggered by the element being selected (or "activated") by the user.
    ///
    /// This could include a selected checkbox, a toggleable button that is toggled on, etc.
    pub ghost_element_selected: Color,
    /// Background Color. Used for the disabled state of a ghost element that should have the same background as the surface it's on.
    ///
    /// Disabled states are shown when a user cannot interact with an element, like a disabled button or input.
    pub ghost_element_disabled: Color,
    /// Text Color. Default text color used for most text.
    pub text: Color,
    /// Text Color. Color of muted or deemphasized text. It is a subdued version of the standard text color.
    pub text_muted: Color,
    /// Text Color. Color of the placeholder text typically shown in input fields to guide the user to enter valid data.
    pub text_placeholder: Color,
    /// Text Color. Color used for text denoting disabled elements. Typically, the color is faded or grayed out to emphasize the disabled state.
    pub text_disabled: Color,
    /// Text Color. Color used for emphasis or highlighting certain text, like an active filter or a matched character in a search.
    pub text_accent: Color,
    /// Fill Color. Used for the default fill color of an icon.
    pub icon: Color,
    /// Fill Color. Used for the muted or deemphasized fill color of an icon.
    ///
    /// This might be used to show an icon in an inactive pane, or to deemphasize a series of icons to give them less visual weight.
    pub icon_muted: Color,
    /// Fill Color. Used for the disabled fill color of an icon.
    ///
    /// Disabled states are shown when a user cannot interact with an element, like a icon button.
    pub icon_disabled: Color,
    /// Fill Color. Used for the placeholder fill color of an icon.
    ///
    /// This might be used to show an icon in an input that disappears when the user enters text.
    pub icon_placeholder: Color,
    /// Fill Color. Used for the accent fill color of an icon.
    ///
    /// This might be used to show when a toggleable icon button is selected.
    pub icon_accent: Color,
    /// Color used to accent some debugger elements
    /// Is used by breakpoints
    pub debugger_accent: Color,

    // ===
    // UI Elements
    // ===
    pub status_bar_background: Color,
    pub title_bar_background: Color,
    pub title_bar_inactive_background: Color,
    pub toolbar_background: Color,
    pub tab_bar_background: Color,
    pub tab_inactive_background: Color,
    pub tab_active_background: Color,
    pub search_match_background: Color,
    pub search_active_match_background: Color,
    pub panel_background: Color,
    pub panel_focused_border: Color,
    pub panel_indent_guide: Color,
    pub panel_indent_guide_hover: Color,
    pub panel_indent_guide_active: Color,

    /// The color of the overlay surface on top of panel.
    pub panel_overlay_background: Color,
    /// The color of the overlay surface on top of panel when hovered over.
    pub panel_overlay_hover: Color,

    pub pane_focused_border: Color,
    pub pane_group_border: Color,
    /// The color of the scrollbar thumb.
    pub scrollbar_thumb_background: Color,
    /// The color of the scrollbar thumb when hovered over.
    pub scrollbar_thumb_hover_background: Color,
    /// The color of the scrollbar thumb whilst being actively dragged.
    pub scrollbar_thumb_active_background: Color,
    /// The border color of the scrollbar thumb.
    pub scrollbar_thumb_border: Color,
    /// The background color of the scrollbar track.
    pub scrollbar_track_background: Color,
    /// The border color of the scrollbar track.
    pub scrollbar_track_border: Color,
    /// The color of the minimap thumb.
    pub minimap_thumb_background: Color,
    /// The color of the minimap thumb when hovered over.
    pub minimap_thumb_hover_background: Color,
    /// The color of the minimap thumb whilst being actively dragged.
    pub minimap_thumb_active_background: Color,
    /// The border color of the minimap thumb.
    pub minimap_thumb_border: Color,

    /// Background color for Vim Normal mode indicator.
    pub vim_normal_background: Color,
    /// Background color for Vim Insert mode indicator.
    pub vim_insert_background: Color,
    /// Background color for Vim Replace mode indicator.
    pub vim_replace_background: Color,
    /// Background color for Vim Visual mode indicator.
    pub vim_visual_background: Color,
    /// Background color for Vim Visual Line mode indicator.
    pub vim_visual_line_background: Color,
    /// Background color for Vim Visual Block mode indicator.
    pub vim_visual_block_background: Color,
    /// Background color for Vim yank highlight.
    pub vim_yank_background: Color,
    /// Foreground color for Helix jump labels.
    pub vim_helix_jump_label_foreground: Color,
    /// Background color for Vim Helix Normal mode indicator.
    pub vim_helix_normal_background: Color,
    /// Background color for Vim Helix Select mode indicator.
    pub vim_helix_select_background: Color,
    /// Foreground color for Vim Normal mode indicator.
    pub vim_normal_foreground: Color,
    /// Foreground color for Vim Insert mode indicator.
    pub vim_insert_foreground: Color,
    /// Foreground color for Vim Replace mode indicator.
    pub vim_replace_foreground: Color,
    /// Foreground color for Vim Visual mode indicator.
    pub vim_visual_foreground: Color,
    /// Foreground color for Vim Visual Line mode indicator.
    pub vim_visual_line_foreground: Color,
    /// Foreground color for Vim Visual Block mode indicator.
    pub vim_visual_block_foreground: Color,
    /// Foreground color for Vim Helix Normal mode indicator.
    pub vim_helix_normal_foreground: Color,
    /// Foreground color for Vim Helix Select mode indicator.
    pub vim_helix_select_foreground: Color,

    // ===
    // Editor
    // ===
    pub editor_foreground: Color,
    pub editor_background: Color,
    pub editor_gutter_background: Color,
    pub editor_subheader_background: Color,
    pub editor_active_line_background: Color,
    pub editor_highlighted_line_background: Color,
    /// Line color of the line a debugger is currently stopped at
    pub editor_debugger_active_line_background: Color,
    /// Text Color. Used for the text of the line number in the editor gutter.
    pub editor_line_number: Color,
    /// Text Color. Used for the text of the line number in the editor gutter when the line is highlighted.
    pub editor_active_line_number: Color,
    /// Text Color. Used for the text of the line number in the editor gutter when the line is hovered over.
    pub editor_hover_line_number: Color,
    /// Text Color. Used to mark invisible characters in the editor.
    ///
    /// Example: spaces, tabs, carriage returns, etc.
    pub editor_invisible: Color,
    pub editor_wrap_guide: Color,
    pub editor_active_wrap_guide: Color,
    pub editor_indent_guide: Color,
    pub editor_indent_guide_active: Color,
    /// Read-access of a symbol, like reading a variable.
    ///
    /// A document highlight is a range inside a text document which deserves
    /// special attention. Usually a document highlight is visualized by changing
    /// the background color of its range.
    pub editor_document_highlight_read_background: Color,
    /// Read-access of a symbol, like reading a variable.
    ///
    /// A document highlight is a range inside a text document which deserves
    /// special attention. Usually a document highlight is visualized by changing
    /// the background color of its range.
    pub editor_document_highlight_write_background: Color,
    /// Highlighted brackets background color.
    ///
    /// Matching brackets in the cursor scope are highlighted with this background color.
    pub editor_document_highlight_bracket_background: Color,
    /// Filled background color for added diff hunk row highlights in the editor.
    pub editor_diff_hunk_added_background: Color,
    /// Hollow background color for added diff hunk row highlights in the editor.
    pub editor_diff_hunk_added_hollow_background: Color,
    /// Hollow border color for added diff hunk row highlights in the editor.
    pub editor_diff_hunk_added_hollow_border: Color,
    /// Filled background color for deleted diff hunk row highlights in the editor.
    pub editor_diff_hunk_deleted_background: Color,
    /// Hollow background color for deleted diff hunk row highlights in the editor.
    pub editor_diff_hunk_deleted_hollow_background: Color,
    /// Hollow border color for deleted diff hunk row highlights in the editor.
    pub editor_diff_hunk_deleted_hollow_border: Color,

    // ===
    // Terminal
    // ===
    /// Terminal layout background color.
    pub terminal_background: Color,
    /// Terminal foreground color.
    pub terminal_foreground: Color,
    /// Bright terminal foreground color.
    pub terminal_bright_foreground: Color,
    /// Dim terminal foreground color.
    pub terminal_dim_foreground: Color,
    /// Terminal ANSI background color.
    pub terminal_ansi_background: Color,
    /// Black ANSI terminal color.
    pub terminal_ansi_black: Color,
    /// Bright black ANSI terminal color.
    pub terminal_ansi_bright_black: Color,
    /// Dim black ANSI terminal color.
    pub terminal_ansi_dim_black: Color,
    /// Red ANSI terminal color.
    pub terminal_ansi_red: Color,
    /// Bright red ANSI terminal color.
    pub terminal_ansi_bright_red: Color,
    /// Dim red ANSI terminal color.
    pub terminal_ansi_dim_red: Color,
    /// Green ANSI terminal color.
    pub terminal_ansi_green: Color,
    /// Bright green ANSI terminal color.
    pub terminal_ansi_bright_green: Color,
    /// Dim green ANSI terminal color.
    pub terminal_ansi_dim_green: Color,
    /// Yellow ANSI terminal color.
    pub terminal_ansi_yellow: Color,
    /// Bright yellow ANSI terminal color.
    pub terminal_ansi_bright_yellow: Color,
    /// Dim yellow ANSI terminal color.
    pub terminal_ansi_dim_yellow: Color,
    /// Blue ANSI terminal color.
    pub terminal_ansi_blue: Color,
    /// Bright blue ANSI terminal color.
    pub terminal_ansi_bright_blue: Color,
    /// Dim blue ANSI terminal color.
    pub terminal_ansi_dim_blue: Color,
    /// Magenta ANSI terminal color.
    pub terminal_ansi_magenta: Color,
    /// Bright magenta ANSI terminal color.
    pub terminal_ansi_bright_magenta: Color,
    /// Dim magenta ANSI terminal color.
    pub terminal_ansi_dim_magenta: Color,
    /// Cyan ANSI terminal color.
    pub terminal_ansi_cyan: Color,
    /// Bright cyan ANSI terminal color.
    pub terminal_ansi_bright_cyan: Color,
    /// Dim cyan ANSI terminal color.
    pub terminal_ansi_dim_cyan: Color,
    /// White ANSI terminal color.
    pub terminal_ansi_white: Color,
    /// Bright white ANSI terminal color.
    pub terminal_ansi_bright_white: Color,
    /// Dim white ANSI terminal color.
    pub terminal_ansi_dim_white: Color,

    /// Represents a link text hover color.
    pub link_text_hover: Color,

    /// Represents an added entry or hunk in vcs, like git.
    pub version_control_added: Color,
    /// Represents a deleted entry in version control systems.
    pub version_control_deleted: Color,
    /// Represents a modified entry in version control systems.
    pub version_control_modified: Color,
    /// Represents a renamed entry in version control systems.
    pub version_control_renamed: Color,
    /// Represents a conflicting entry in version control systems.
    pub version_control_conflict: Color,
    /// Represents an ignored entry in version control systems.
    pub version_control_ignored: Color,
    /// Represents an added word in a word diff.
    pub version_control_word_added: Color,
    /// Represents a deleted word in a word diff.
    pub version_control_word_deleted: Color,
    /// Represents the "ours" region of a merge conflict.
    pub version_control_conflict_marker_ours: Color,
    /// Represents the "theirs" region of a merge conflict.
    pub version_control_conflict_marker_theirs: Color,
}

#[derive(EnumIter, Debug, Clone, Copy, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum ThemeColorField {
    Border,
    BorderVariant,
    BorderFocused,
    BorderSelected,
    BorderTransparent,
    BorderDisabled,
    ElevatedSurfaceBackground,
    SurfaceBackground,
    Background,
    ElementBackground,
    ElementHover,
    ElementActive,
    ElementSelected,
    ElementDisabled,
    DropTargetBackground,
    DropTargetBorder,
    GhostElementBackground,
    GhostElementHover,
    GhostElementActive,
    GhostElementSelected,
    GhostElementDisabled,
    Text,
    TextMuted,
    TextPlaceholder,
    TextDisabled,
    TextAccent,
    Icon,
    IconMuted,
    IconDisabled,
    IconPlaceholder,
    IconAccent,
    StatusBarBackground,
    TitleBarBackground,
    TitleBarInactiveBackground,
    ToolbarBackground,
    TabBarBackground,
    TabInactiveBackground,
    TabActiveBackground,
    SearchMatchBackground,
    SearchActiveMatchBackground,
    PanelBackground,
    PanelFocusedBorder,
    PanelIndentGuide,
    PanelIndentGuideHover,
    PanelIndentGuideActive,
    PanelOverlayBackground,
    PanelOverlayHover,
    PaneFocusedBorder,
    PaneGroupBorder,
    ScrollbarThumbBackground,
    ScrollbarThumbHoverBackground,
    ScrollbarThumbActiveBackground,
    ScrollbarThumbBorder,
    ScrollbarTrackBackground,
    ScrollbarTrackBorder,
    MinimapThumbBackground,
    MinimapThumbHoverBackground,
    MinimapThumbActiveBackground,
    MinimapThumbBorder,
    EditorForeground,
    EditorBackground,
    EditorGutterBackground,
    EditorSubheaderBackground,
    EditorActiveLineBackground,
    EditorHighlightedLineBackground,
    EditorLineNumber,
    EditorActiveLineNumber,
    EditorInvisible,
    EditorWrapGuide,
    EditorActiveWrapGuide,
    EditorIndentGuide,
    EditorIndentGuideActive,
    EditorDocumentHighlightReadBackground,
    EditorDocumentHighlightWriteBackground,
    EditorDocumentHighlightBracketBackground,
    TerminalBackground,
    TerminalForeground,
    TerminalBrightForeground,
    TerminalDimForeground,
    TerminalAnsiBackground,
    TerminalAnsiBlack,
    TerminalAnsiBrightBlack,
    TerminalAnsiDimBlack,
    TerminalAnsiRed,
    TerminalAnsiBrightRed,
    TerminalAnsiDimRed,
    TerminalAnsiGreen,
    TerminalAnsiBrightGreen,
    TerminalAnsiDimGreen,
    TerminalAnsiYellow,
    TerminalAnsiBrightYellow,
    TerminalAnsiDimYellow,
    TerminalAnsiBlue,
    TerminalAnsiBrightBlue,
    TerminalAnsiDimBlue,
    TerminalAnsiMagenta,
    TerminalAnsiBrightMagenta,
    TerminalAnsiDimMagenta,
    TerminalAnsiCyan,
    TerminalAnsiBrightCyan,
    TerminalAnsiDimCyan,
    TerminalAnsiWhite,
    TerminalAnsiBrightWhite,
    TerminalAnsiDimWhite,
    LinkTextHover,
    VersionControlAdded,
    VersionControlDeleted,
    VersionControlModified,
    VersionControlRenamed,
    VersionControlConflict,
    VersionControlIgnored,
}

impl ThemeColors {
    pub fn color(&self, field: ThemeColorField) -> Color {
        match field {
            ThemeColorField::Border => self.border,
            ThemeColorField::BorderVariant => self.border_variant,
            ThemeColorField::BorderFocused => self.border_focused,
            ThemeColorField::BorderSelected => self.border_selected,
            ThemeColorField::BorderTransparent => self.border_transparent,
            ThemeColorField::BorderDisabled => self.border_disabled,
            ThemeColorField::ElevatedSurfaceBackground => self.elevated_surface_background,
            ThemeColorField::SurfaceBackground => self.surface_background,
            ThemeColorField::Background => self.background,
            ThemeColorField::ElementBackground => self.element_background,
            ThemeColorField::ElementHover => self.element_hover,
            ThemeColorField::ElementActive => self.element_active,
            ThemeColorField::ElementSelected => self.element_selected,
            ThemeColorField::ElementDisabled => self.element_disabled,
            ThemeColorField::DropTargetBackground => self.drop_target_background,
            ThemeColorField::DropTargetBorder => self.drop_target_border,
            ThemeColorField::GhostElementBackground => self.ghost_element_background,
            ThemeColorField::GhostElementHover => self.ghost_element_hover,
            ThemeColorField::GhostElementActive => self.ghost_element_active,
            ThemeColorField::GhostElementSelected => self.ghost_element_selected,
            ThemeColorField::GhostElementDisabled => self.ghost_element_disabled,
            ThemeColorField::Text => self.text,
            ThemeColorField::TextMuted => self.text_muted,
            ThemeColorField::TextPlaceholder => self.text_placeholder,
            ThemeColorField::TextDisabled => self.text_disabled,
            ThemeColorField::TextAccent => self.text_accent,
            ThemeColorField::Icon => self.icon,
            ThemeColorField::IconMuted => self.icon_muted,
            ThemeColorField::IconDisabled => self.icon_disabled,
            ThemeColorField::IconPlaceholder => self.icon_placeholder,
            ThemeColorField::IconAccent => self.icon_accent,
            ThemeColorField::StatusBarBackground => self.status_bar_background,
            ThemeColorField::TitleBarBackground => self.title_bar_background,
            ThemeColorField::TitleBarInactiveBackground => self.title_bar_inactive_background,
            ThemeColorField::ToolbarBackground => self.toolbar_background,
            ThemeColorField::TabBarBackground => self.tab_bar_background,
            ThemeColorField::TabInactiveBackground => self.tab_inactive_background,
            ThemeColorField::TabActiveBackground => self.tab_active_background,
            ThemeColorField::SearchMatchBackground => self.search_match_background,
            ThemeColorField::SearchActiveMatchBackground => self.search_active_match_background,
            ThemeColorField::PanelBackground => self.panel_background,
            ThemeColorField::PanelFocusedBorder => self.panel_focused_border,
            ThemeColorField::PanelIndentGuide => self.panel_indent_guide,
            ThemeColorField::PanelIndentGuideHover => self.panel_indent_guide_hover,
            ThemeColorField::PanelIndentGuideActive => self.panel_indent_guide_active,
            ThemeColorField::PanelOverlayBackground => self.panel_overlay_background,
            ThemeColorField::PanelOverlayHover => self.panel_overlay_hover,
            ThemeColorField::PaneFocusedBorder => self.pane_focused_border,
            ThemeColorField::PaneGroupBorder => self.pane_group_border,
            ThemeColorField::ScrollbarThumbBackground => self.scrollbar_thumb_background,
            ThemeColorField::ScrollbarThumbHoverBackground => self.scrollbar_thumb_hover_background,
            ThemeColorField::ScrollbarThumbActiveBackground => {
                self.scrollbar_thumb_active_background
            }
            ThemeColorField::ScrollbarThumbBorder => self.scrollbar_thumb_border,
            ThemeColorField::ScrollbarTrackBackground => self.scrollbar_track_background,
            ThemeColorField::ScrollbarTrackBorder => self.scrollbar_track_border,
            ThemeColorField::MinimapThumbBackground => self.minimap_thumb_background,
            ThemeColorField::MinimapThumbHoverBackground => self.minimap_thumb_hover_background,
            ThemeColorField::MinimapThumbActiveBackground => self.minimap_thumb_active_background,
            ThemeColorField::MinimapThumbBorder => self.minimap_thumb_border,
            ThemeColorField::EditorForeground => self.editor_foreground,
            ThemeColorField::EditorBackground => self.editor_background,
            ThemeColorField::EditorGutterBackground => self.editor_gutter_background,
            ThemeColorField::EditorSubheaderBackground => self.editor_subheader_background,
            ThemeColorField::EditorActiveLineBackground => self.editor_active_line_background,
            ThemeColorField::EditorHighlightedLineBackground => {
                self.editor_highlighted_line_background
            }
            ThemeColorField::EditorLineNumber => self.editor_line_number,
            ThemeColorField::EditorActiveLineNumber => self.editor_active_line_number,
            ThemeColorField::EditorInvisible => self.editor_invisible,
            ThemeColorField::EditorWrapGuide => self.editor_wrap_guide,
            ThemeColorField::EditorActiveWrapGuide => self.editor_active_wrap_guide,
            ThemeColorField::EditorIndentGuide => self.editor_indent_guide,
            ThemeColorField::EditorIndentGuideActive => self.editor_indent_guide_active,
            ThemeColorField::EditorDocumentHighlightReadBackground => {
                self.editor_document_highlight_read_background
            }
            ThemeColorField::EditorDocumentHighlightWriteBackground => {
                self.editor_document_highlight_write_background
            }
            ThemeColorField::EditorDocumentHighlightBracketBackground => {
                self.editor_document_highlight_bracket_background
            }
            ThemeColorField::TerminalBackground => self.terminal_background,
            ThemeColorField::TerminalForeground => self.terminal_foreground,
            ThemeColorField::TerminalBrightForeground => self.terminal_bright_foreground,
            ThemeColorField::TerminalDimForeground => self.terminal_dim_foreground,
            ThemeColorField::TerminalAnsiBackground => self.terminal_ansi_background,
            ThemeColorField::TerminalAnsiBlack => self.terminal_ansi_black,
            ThemeColorField::TerminalAnsiBrightBlack => self.terminal_ansi_bright_black,
            ThemeColorField::TerminalAnsiDimBlack => self.terminal_ansi_dim_black,
            ThemeColorField::TerminalAnsiRed => self.terminal_ansi_red,
            ThemeColorField::TerminalAnsiBrightRed => self.terminal_ansi_bright_red,
            ThemeColorField::TerminalAnsiDimRed => self.terminal_ansi_dim_red,
            ThemeColorField::TerminalAnsiGreen => self.terminal_ansi_green,
            ThemeColorField::TerminalAnsiBrightGreen => self.terminal_ansi_bright_green,
            ThemeColorField::TerminalAnsiDimGreen => self.terminal_ansi_dim_green,
            ThemeColorField::TerminalAnsiYellow => self.terminal_ansi_yellow,
            ThemeColorField::TerminalAnsiBrightYellow => self.terminal_ansi_bright_yellow,
            ThemeColorField::TerminalAnsiDimYellow => self.terminal_ansi_dim_yellow,
            ThemeColorField::TerminalAnsiBlue => self.terminal_ansi_blue,
            ThemeColorField::TerminalAnsiBrightBlue => self.terminal_ansi_bright_blue,
            ThemeColorField::TerminalAnsiDimBlue => self.terminal_ansi_dim_blue,
            ThemeColorField::TerminalAnsiMagenta => self.terminal_ansi_magenta,
            ThemeColorField::TerminalAnsiBrightMagenta => self.terminal_ansi_bright_magenta,
            ThemeColorField::TerminalAnsiDimMagenta => self.terminal_ansi_dim_magenta,
            ThemeColorField::TerminalAnsiCyan => self.terminal_ansi_cyan,
            ThemeColorField::TerminalAnsiBrightCyan => self.terminal_ansi_bright_cyan,
            ThemeColorField::TerminalAnsiDimCyan => self.terminal_ansi_dim_cyan,
            ThemeColorField::TerminalAnsiWhite => self.terminal_ansi_white,
            ThemeColorField::TerminalAnsiBrightWhite => self.terminal_ansi_bright_white,
            ThemeColorField::TerminalAnsiDimWhite => self.terminal_ansi_dim_white,
            ThemeColorField::LinkTextHover => self.link_text_hover,
            ThemeColorField::VersionControlAdded => self.version_control_added,
            ThemeColorField::VersionControlDeleted => self.version_control_deleted,
            ThemeColorField::VersionControlModified => self.version_control_modified,
            ThemeColorField::VersionControlRenamed => self.version_control_renamed,
            ThemeColorField::VersionControlConflict => self.version_control_conflict,
            ThemeColorField::VersionControlIgnored => self.version_control_ignored,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (ThemeColorField, Color)> + '_ {
        ThemeColorField::iter().map(move |field| (field, self.color(field)))
    }

    pub fn to_vec(&self) -> Vec<(ThemeColorField, Color)> {
        self.iter().collect()
    }
}

pub fn all_theme_colors(cx: &mut App) -> Vec<(Hsla, SharedString)> {
    let theme = cx.theme();
    ThemeColorField::iter()
        .map(|field| {
            let color = theme.colors().color(field);
            let name = field.as_ref().to_string();
            (color.into(), SharedString::from(name))
        })
        .collect()
}

#[derive(Refineable, Clone, Debug, PartialEq)]
pub struct ThemeStyles {
    /// The background appearance of the window.
    pub window_background_appearance: WindowBackgroundAppearance,
    pub system: SystemColors,
    /// An array of colors used for theme elements that iterate through a series of colors.
    ///
    /// Example: Player colors, rainbow brackets and indent guides, etc.
    pub accents: AccentColors,

    #[refineable]
    pub colors: ThemeColors,

    #[refineable]
    pub status: StatusColors,

    pub player: PlayerColors,

    pub syntax: Arc<SyntaxTheme>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn override_a_single_theme_color() {
        let mut colors = ThemeColors::light();

        let magenta: Color = gpui::rgb(0xff00ff).into();

        assert_ne!(colors.text, magenta);

        let overrides = ThemeColorsRefinement {
            text: Some(magenta),
            ..Default::default()
        };

        colors.refine(&overrides);

        assert_eq!(colors.text, magenta);
    }

    #[test]
    fn override_multiple_theme_colors() {
        let mut colors = ThemeColors::light();

        let magenta: Color = gpui::rgb(0xff00ff).into();
        let green: Color = gpui::rgb(0x00ff00).into();

        assert_ne!(colors.text, magenta);
        assert_ne!(colors.background, green);

        let overrides = ThemeColorsRefinement {
            text: Some(magenta),
            background: Some(green),
            ..Default::default()
        };

        colors.refine(&overrides);

        assert_eq!(colors.text, magenta);
        assert_eq!(colors.background, green);
    }

    #[test]
    fn deserialize_theme_colors_refinement_from_json() {
        let colors: ThemeColorsRefinement = serde_json::from_value(json!({
            "background": "#ff00ff",
            "text": "#ff0000"
        }))
        .unwrap();

        assert_eq!(colors.background, Some(gpui::rgb(0xff00ff).into()));
        assert_eq!(colors.text, Some(gpui::rgb(0xff0000).into()));
    }
}
