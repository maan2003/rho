//! Chrome settings shared by rho's chat-style editors (the agent view and
//! the draft compose view): no gutters, no scrollbars, soft wrap, no code
//! assists.

use editor::Editor;
use gpui::{Context, Window};

pub fn configure(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    editor.set_show_gutter(false, cx);
    editor.set_show_compact_gutter(true, cx);
    editor.set_show_line_numbers(false, cx);
    editor.set_show_git_diff_gutter(false, cx);
    editor.set_show_code_actions(false, cx);
    editor.set_show_runnables(false, cx);
    editor.set_show_breakpoints(false, cx);
    editor.set_show_bookmarks(false, cx);
    editor.set_show_vertical_scrollbar(false, cx);
    editor.set_show_horizontal_scrollbar(false, cx);
    editor.set_offset_content(false, cx);
    editor.set_mouse_click_selection_enabled(false, cx);
    editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
    editor.set_show_wrap_guides(false, cx);
    editor.set_show_indent_guides(false, cx);
    editor.set_autoindent(false);
    editor.set_show_edit_predictions(Some(false), window, cx);
    editor.set_use_selection_highlight(false);
    editor.disable_expand_excerpt_buttons(cx);
}
