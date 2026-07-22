//! Chrome settings for rho's editors: bare buffers in the frame. Chat
//! editors (agent view, draft compose) also drop editing affordances;
//! file editors keep them but shed the same chrome.

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

/// Chrome for file buffers: a real code editor dressed as a rho buffer.
/// Unlike the chat editors it keeps editing behavior (autoindent, mouse
/// selection, no soft wrap), but sheds every gutter column and overlay —
/// no line numbers by choice, no scrollbars, no guides.
pub fn configure_file(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    editor.set_show_gutter(false, cx);
    editor.set_show_line_numbers(false, cx);
    editor.set_show_git_diff_gutter(false, cx);
    editor.set_show_code_actions(false, cx);
    editor.set_show_runnables(false, cx);
    editor.set_show_breakpoints(false, cx);
    editor.set_show_bookmarks(false, cx);
    editor.set_show_vertical_scrollbar(false, cx);
    editor.set_show_horizontal_scrollbar(false, cx);
    editor.set_offset_content(false, cx);
    editor.set_show_wrap_guides(false, cx);
    editor.set_show_indent_guides(false, cx);
    editor.set_show_edit_predictions(Some(false), window, cx);
    editor.disable_expand_excerpt_buttons(cx);
}

/// Chrome for a multi-file review surface. Diff rows need more orientation
/// than an ordinary rho file buffer: line numbers, a stable content inset,
/// and scrollbars make hunk and file boundaries legible.
pub fn configure_diff(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    configure_file(editor, window, cx);
    editor.set_show_gutter(true, cx);
    editor.set_show_line_numbers(true, cx);
    editor.set_show_vertical_scrollbar(true, cx);
    editor.set_show_horizontal_scrollbar(true, cx);
    editor.set_offset_content(true, cx);
}
