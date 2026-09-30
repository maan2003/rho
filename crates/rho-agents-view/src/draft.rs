//! The draft compose model: pick a workdir, a role and a base, write the
//! first message, then submit to create the agent.
//!
//! Entirely separate from [`crate::agent_view::AgentModel`] — there is no
//! transcript here. The multibuffer composes the draft fields and message body.
//! The excerpt boundary separates field from body, so there is no scaffold text
//! to parse: submission just reads the two writable buffers.
//!
//! The model is the buffer role; the draft surface builds an editor over the
//! shared multibuffer via [`DraftModel::build_editor`].
//! Cursor-dependent operations (field cycling, focusing the body) act on a
//! specific editor, passed by the caller — the cursor belongs to the
//! window, not the buffer.

use std::ops::Range;

use collections::HashSet;
use editor::display_map::CustomBlockId;
use editor::scroll::AutoscrollStrategy;
use editor::{Editor, EditorMode, HighlightKey, Inlay, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{Context, Entity, Focusable, Subscription, WeakEntity, Window, div, px};
use language::{Buffer, BufferEvent, Capability, InlayId, Point};
use multi_buffer::{MultiBuffer, PathKey, ToOffset as _};
use rho_agent_types::ContentPart;
use rho_window::style::{self, PROMPT_DRAFT_HIGHLIGHT_KEY, StyleClass};
use theme::ActiveTheme as _;

const BODY_PLACEHOLDER_INLAY_ID: usize = 0;
const WORKDIR_LABEL_INLAY_ID: usize = 1;
const ROLE_LABEL_INLAY_ID: usize = 2;
const START_LABEL_INLAY_ID: usize = 3;
const START_TARGET_HINT_INLAY_ID: usize = 4;

pub use rho_agents_client::create::{AUTO_BASE_REV, DEFAULT_ROLE, DEFAULT_START, StartFieldMode};

impl StartFieldModeLabel for StartFieldMode {
    fn label(self) -> &'static str {
        match self {
            Self::NewOn => "On top of: ",
            Self::Join => "Join: ",
        }
    }
}

/// How a start mode reads in the field label. The mode is the crate's;
/// the words in front of it are this screen's.
trait StartFieldModeLabel {
    fn label(self) -> &'static str;
}

/// What the host supplies for a draft editor. The crate builds the editor
/// and owns the fields; who can complete a workdir, a role or a start is
/// the host's question, so the host installs its own provider on each
/// editor as it is built.
///
/// A closure rather than a function pointer because the provider needs
/// whatever the host completes against, and that is state the crate must
/// not see.
#[derive(Clone)]
pub struct Hooks(std::rc::Rc<ConfigureEditor>);

/// What the host does to each draft editor as it is built.
type ConfigureEditor = dyn Fn(&mut Editor, Fields, &mut Window, &mut Context<Editor>);

impl Hooks {
    pub fn new(
        configure: impl Fn(&mut Editor, Fields, &mut Window, &mut Context<Editor>) + 'static,
    ) -> Self {
        Self(std::rc::Rc::new(configure))
    }
}

/// The three field buffers an editor is built over, named so the host can
/// tell them apart when it installs completion.
#[derive(Clone, Copy)]
pub struct Fields {
    pub workdir: gpui::EntityId,
    pub role: gpui::EntityId,
    pub start: gpui::EntityId,
}

/// What the draft says about itself. The host decides what an edit means
/// for the rest of the screen; the draft only says that one happened.
pub enum Event {
    /// The reader has typed in the draft, so it is what they are working
    /// on now.
    Edited,
}

impl gpui::EventEmitter<Event> for DraftModel {}

pub struct DraftGutter;

pub struct DraftModel {
    hooks: Hooks,
    multi_buffer: Entity<MultiBuffer>,
    workdir_buffer: Entity<Buffer>,
    role_buffer: Entity<Buffer>,
    start_buffer: Entity<Buffer>,
    start_mode: StartFieldMode,
    start_target_hints: Vec<(String, String)>,
    body_buffer: Entity<Buffer>,
    body_end: text::Anchor,
    attachments: Vec<ContentPart>,
    attachment_blocks: Vec<(WeakEntity<Editor>, CustomBlockId)>,
    /// Why the last submission was refused, kept under the body until the
    /// reader submits again.
    refusal: Option<String>,
    refusal_blocks: Vec<(WeakEntity<Editor>, CustomBlockId)>,
    suppress_draft_activation: bool,
    /// Editors currently displaying the draft, weakly held: surfaces own
    /// their editors; the model reconciles whoever is still alive.
    editors: Vec<WeakEntity<Editor>>,
    _subscriptions: Vec<Subscription>,
}

impl DraftModel {
    pub fn new(hooks: Hooks, cx: &mut Context<Self>) -> Self {
        let workdir_buffer = cx.new(|cx| Buffer::local("", cx));
        let role_buffer = cx.new(|cx| Buffer::local(DEFAULT_ROLE, cx));
        let start_buffer = cx.new(|cx| Buffer::local(DEFAULT_START, cx));
        let body_buffer = cx.new(|cx| Buffer::local("", cx));
        let body_end = body_buffer.read(cx).anchor_after(0);
        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            for (key, buffer) in [
                (0, &workdir_buffer),
                (1, &role_buffer),
                (2, &start_buffer),
                (3, &body_buffer),
            ] {
                multi_buffer.set_excerpts_for_path(
                    PathKey::sorted(key),
                    buffer.clone(),
                    [Point::zero()..buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            }
            multi_buffer
        });

        let subscriptions = vec![
            cx.subscribe(&body_buffer, |this, _, event, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.note_draft_edit(cx);
                    this.update_body_chrome(cx);
                }
            }),
            cx.subscribe(&workdir_buffer, |this, _, event, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.note_draft_edit(cx);
                }
            }),
            cx.subscribe(&role_buffer, |this, _, event, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.note_draft_edit(cx);
                }
            }),
            cx.subscribe(&start_buffer, |this, _, event, cx| {
                if matches!(event, BufferEvent::Edited { .. }) {
                    this.note_draft_edit(cx);
                    this.update_start_target_hint(cx);
                }
            }),
        ];

        Self {
            hooks,
            multi_buffer,
            workdir_buffer,
            role_buffer,
            start_buffer,
            start_mode: StartFieldMode::NewOn,
            start_target_hints: Vec::new(),
            body_buffer,
            body_end,
            attachments: Vec::new(),
            attachment_blocks: Vec::new(),
            refusal: None,
            refusal_blocks: Vec::new(),
            suppress_draft_activation: false,
            editors: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Builds an editor over the shared multibuffer — own cursor and
    /// scroll — fully caught up with the model, cursor at the body end.
    pub fn build_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        let hooks = self.hooks.clone();
        let multi_buffer = self.multi_buffer.clone();
        let buffers = [
            &self.workdir_buffer,
            &self.role_buffer,
            &self.start_buffer,
            &self.body_buffer,
        ];
        let buffer_ids = buffers.map(|buffer| buffer.read(cx).remote_id());
        let workdir_id = self.workdir_buffer.entity_id();
        let role_id = self.role_buffer.entity_id();
        let start_id = self.start_buffer.entity_id();
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer,
                None,
                window,
                cx,
            );
            rho_window::editor_config::configure(&mut editor, window, cx);
            for buffer_id in buffer_ids {
                editor.disable_header_for_buffer(buffer_id, cx);
            }
            (hooks.0)(
                &mut editor,
                Fields {
                    workdir: workdir_id,
                    role: role_id,
                    start: start_id,
                },
                window,
                cx,
            );
            editor
        });

        self.editors.push(editor.downgrade());
        self.insert_workdir_label_to(&editor, cx);
        self.insert_role_label_to(&editor, cx);
        self.insert_start_label_to(&editor, cx);
        self.apply_start_target_hint_to(&editor, cx);
        self.insert_body_gap_to(&editor, cx);
        self.pin_autoscroll_to(&editor, cx);
        self.apply_body_chrome_to(&editor, cx);
        self.refresh_attachment_blocks(cx);
        self.refresh_refusal_blocks(cx);
        self.focus_body(&editor, window, cx);
        editor
    }

    /// A direct-touch projection. Each editor contains exactly one of the
    /// canonical buffers, so tapping a field cannot land in the message.
    pub fn build_phone_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<PhoneDraft> {
        let fields = Fields {
            workdir: self.workdir_buffer.entity_id(),
            role: self.role_buffer.entity_id(),
            start: self.start_buffer.entity_id(),
        };
        let editors = [
            self.workdir_buffer.clone(),
            self.role_buffer.clone(),
            self.start_buffer.clone(),
            self.body_buffer.clone(),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, buffer)| {
            let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let hooks = self.hooks.clone();
            cx.new(|cx| {
                let mode = if index == 3 {
                    EditorMode::AutoHeight {
                        min_lines: 4,
                        max_lines: Some(6),
                    }
                } else {
                    EditorMode::SingleLine
                };
                let mut editor = Editor::new(mode, multi_buffer, None, window, cx);
                rho_window::editor_config::configure(&mut editor, window, cx);
                editor.set_show_compact_gutter(false, cx);
                editor.set_mouse_click_selection_enabled(true, cx);
                if index == 3 {
                    editor.set_placeholder_text("Write a message…", window, cx);
                }
                (hooks.0)(&mut editor, fields, window, cx);
                editor
            })
        })
        .collect::<Vec<_>>();
        let model = cx.entity();
        cx.new(|cx| PhoneDraft {
            _subscription: cx.observe(&model, |_, _, cx| cx.notify()),
            model,
            workdir: editors[0].clone(),
            role: editors[1].clone(),
            start: editors[2].clone(),
            body: editors[3].clone(),
            scroll: gpui::ScrollHandle::new(),
            last_viewport: gpui::Size::default(),
            last_focus: None,
        })
    }

    /// The editors still alive, pruning dropped ones.
    fn live_editors(&mut self) -> Vec<Entity<Editor>> {
        self.editors.retain(|editor| editor.upgrade().is_some());
        self.editors
            .iter()
            .filter_map(|editor| editor.upgrade())
            .collect()
    }

    pub fn workdir_text(&self, cx: &gpui::App) -> String {
        let buffer = self.workdir_buffer.read(cx);
        buffer.text_for_range(0..buffer.len()).collect()
    }

    pub fn set_workdir_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.suppress_draft_activation = true;
        self.workdir_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.suppress_draft_activation = false;
    }

    pub fn role_text(&self, cx: &gpui::App) -> String {
        let buffer = self.role_buffer.read(cx);
        buffer.text_for_range(0..buffer.len()).collect()
    }

    pub fn set_role_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.suppress_draft_activation = true;
        self.role_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.suppress_draft_activation = false;
    }

    pub fn start_mode(&self) -> StartFieldMode {
        self.start_mode
    }

    pub fn start_text(&self, cx: &gpui::App) -> String {
        let buffer = self.start_buffer.read(cx);
        buffer.text_for_range(0..buffer.len()).collect()
    }

    pub fn set_start_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.suppress_draft_activation = true;
        self.start_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.suppress_draft_activation = false;
        self.update_start_target_hint(cx);
    }

    pub fn set_start_target_hints(&mut self, hints: Vec<(String, String)>, cx: &mut Context<Self>) {
        self.start_target_hints = hints;
        self.update_start_target_hint(cx);
    }

    /// Shift-Tab while the cursor is in the start field: cycle how the
    /// target is interpreted (the field label shows the mode).
    pub fn cycle_start_mode(&mut self, cx: &mut Context<Self>) {
        self.set_start_mode(
            match self.start_mode {
                StartFieldMode::NewOn => StartFieldMode::Join,
                StartFieldMode::Join => StartFieldMode::NewOn,
            },
            cx,
        );
    }

    /// Choose a base interpretation without depending on keyboard cycling.
    pub fn set_start_mode(&mut self, mode: StartFieldMode, cx: &mut Context<Self>) {
        self.start_mode = mode;
        for editor in self.live_editors() {
            self.insert_start_label_to(&editor, cx);
        }
        self.update_start_target_hint(cx);
        cx.notify();
    }

    /// Whether the cursor sits in one of the header rows rather than in the
    /// message body.
    pub fn cursor_in_a_field(&self, editor: &Entity<Editor>, cx: &gpui::App) -> bool {
        self.cursor_in(&self.workdir_buffer, editor, cx)
            || self.cursor_in(&self.role_buffer, editor, cx)
            || self.cursor_in(&self.start_buffer, editor, cx)
    }

    pub fn cursor_in_start_field(&self, editor: &Entity<Editor>, cx: &gpui::App) -> bool {
        self.cursor_in(&self.start_buffer, editor, cx)
    }

    pub fn cursor_in_role_field(&self, editor: &Entity<Editor>, cx: &gpui::App) -> bool {
        self.cursor_in(&self.role_buffer, editor, cx)
    }

    fn cursor_in(&self, buffer: &Entity<Buffer>, editor: &Entity<Editor>, cx: &gpui::App) -> bool {
        let field = buffer.read(cx);
        let range = field.anchor_before(0)..field.anchor_after(field.len());
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let (Some(start), Some(end)) = (
            snapshot.anchor_in_excerpt(range.start),
            snapshot.anchor_in_excerpt(range.end),
        ) else {
            return false;
        };
        let cursor = editor
            .read(cx)
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot);
        cursor >= start.to_offset(&snapshot) && cursor <= end.to_offset(&snapshot)
    }

    /// The message body, without clearing it. Submissions read instead of
    /// taking: the buffers survive until the agent host confirms creation.
    pub fn body_text(&self, cx: &gpui::App) -> String {
        let buffer = self.body_buffer.read(cx);
        buffer.text_for_range(0..buffer.len()).collect()
    }

    pub fn content(&self, cx: &gpui::App) -> Option<Vec<ContentPart>> {
        let text = self.body_text(cx).trim().to_owned();
        if text.is_empty() && self.attachments.is_empty() {
            return None;
        }
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentPart::Text { text });
        }
        content.extend(self.attachments.iter().cloned());
        Some(content)
    }

    pub fn add_image(&mut self, media_type: String, data: Vec<u8>, cx: &mut Context<Self>) {
        self.attachments
            .push(ContentPart::Image { media_type, data });
        self.update_body_chrome(cx);
        self.refresh_attachment_blocks(cx);
    }

    pub fn clear_attachments(&mut self, cx: &mut Context<Self>) -> bool {
        let had = !self.attachments.is_empty();
        self.attachments.clear();
        self.update_body_chrome(cx);
        self.refresh_attachment_blocks(cx);
        had
    }

    pub fn set_body_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.suppress_draft_activation = true;
        self.body_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, text)], None, cx);
        });
        self.suppress_draft_activation = false;
    }

    /// (Re)writes the workdir field with the given label. With an empty body
    /// the cursor lands in the body — the default is usually right, so
    /// typing composes the message immediately (Tab jumps into the field to
    /// change it). A non-empty body keeps its field unless `force` (an
    /// explicit choice, e.g. `:agent new <path>`) asks for the rewrite.
    pub fn seed(
        &mut self,
        workdir: &str,
        force: bool,
        editor: Option<&Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.body_text(cx).trim().is_empty() {
            self.set_workdir_text(workdir, cx);
            if let Some(editor) = editor {
                self.focus_body(editor, window, cx);
            }
        } else if force {
            self.set_workdir_text(workdir, cx);
        }
    }

    /// Tab: cycles workdir field → role field → start field → message body.
    pub fn toggle_field(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = if self.cursor_in(&self.workdir_buffer, editor, cx) {
            Some(&self.role_buffer)
        } else if self.cursor_in(&self.role_buffer, editor, cx) {
            Some(&self.start_buffer)
        } else if self.cursor_in(&self.start_buffer, editor, cx) {
            None
        } else {
            Some(&self.workdir_buffer)
        };
        self.go_to_field(target.cloned(), editor, window, cx);
    }

    /// Shift-Tab: the same rows the other way round, body → start field
    /// → role field → workdir field → body.
    pub fn toggle_field_back(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = if self.cursor_in(&self.workdir_buffer, editor, cx) {
            None
        } else if self.cursor_in(&self.role_buffer, editor, cx) {
            Some(&self.workdir_buffer)
        } else if self.cursor_in(&self.start_buffer, editor, cx) {
            Some(&self.role_buffer)
        } else {
            Some(&self.start_buffer)
        };
        self.go_to_field(target.cloned(), editor, window, cx);
    }

    /// Empties the header row the cursor is on and leaves the cursor in it,
    /// ready to type. Answers whether there was a row to clear.
    pub fn clear_field(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let field = if self.cursor_in(&self.workdir_buffer, editor, cx) {
            self.workdir_buffer.clone()
        } else if self.cursor_in(&self.role_buffer, editor, cx) {
            self.role_buffer.clone()
        } else if self.cursor_in(&self.start_buffer, editor, cx) {
            self.start_buffer.clone()
        } else {
            return false;
        };
        self.suppress_draft_activation = true;
        field.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, "")], None, cx);
        });
        self.suppress_draft_activation = false;
        let start = field.read(cx).anchor_before(0);
        self.select_range(editor, start..start, window, cx);
        true
    }

    /// Puts the cursor at the end of a field, or in the body when there is
    /// no field left to walk to. The whole value used to arrive selected,
    /// which reads as visual mode: `cc` spent its first `c` on the
    /// selection and typed the second into the field, and `enter` was a
    /// key with no meaning there. An empty selection keeps the field an
    /// ordinary vim line, so the line editing keys all mean what they say.
    fn go_to_field(
        &mut self,
        target: Option<Entity<Buffer>>,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = target else {
            self.focus_body(editor, window, cx);
            return;
        };
        let field = target.read(cx);
        let end = field.anchor_after(field.len());
        self.select_range(editor, end..end, window, cx);
    }

    /// Puts the viewport's cursor at the end of the message body.
    pub fn focus_body(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_range(editor, self.body_end..self.body_end, window, cx);
    }

    fn select_range(
        &self,
        editor: &Entity<Editor>,
        range: Range<text::Anchor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let (Some(start), Some(end)) = (
            snapshot.anchor_in_excerpt(range.start),
            snapshot.anchor_in_excerpt(range.end),
        ) else {
            return;
        };
        editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_anchor_ranges([start..end]);
            });
        });
    }

    fn insert_workdir_label_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(field_start) =
            snapshot.anchor_in_excerpt(self.workdir_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        editor.update(cx, |editor, cx| {
            editor.splice_inlays(
                &[],
                vec![Inlay::custom(
                    WORKDIR_LABEL_INLAY_ID,
                    field_start,
                    "Workdir: ",
                )],
                cx,
            );
        });
    }

    fn insert_role_label_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(field_start) =
            snapshot.anchor_in_excerpt(self.role_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        editor.update(cx, |editor, cx| {
            editor.splice_inlays(
                &[],
                vec![Inlay::custom(ROLE_LABEL_INLAY_ID, field_start, "Role: ")],
                cx,
            );
        });
    }

    fn insert_start_label_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(field_start) =
            snapshot.anchor_in_excerpt(self.start_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        editor.update(cx, |editor, cx| {
            editor.splice_inlays(
                &[InlayId::Custom(START_LABEL_INLAY_ID)],
                vec![Inlay::custom(
                    START_LABEL_INLAY_ID,
                    field_start,
                    self.start_mode.label(),
                )],
                cx,
            );
        });
    }

    fn update_start_target_hint(&mut self, cx: &mut Context<Self>) {
        for editor in self.live_editors() {
            self.apply_start_target_hint_to(&editor, cx);
        }
    }

    fn apply_start_target_hint_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let target = self.start_text(cx).trim().to_owned();
        let hint = self
            .start_target_hints
            .iter()
            .find(|(label, _)| label == &target)
            .map(|(_, hint)| hint);
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let start_buffer = self.start_buffer.read(cx);
        let Some(field_end) =
            snapshot.anchor_in_excerpt(start_buffer.anchor_after(start_buffer.len()))
        else {
            return;
        };
        let inlays = hint
            .map(|hint| Inlay::custom(START_TARGET_HINT_INLAY_ID, field_end, format!("  {hint}")))
            .into_iter()
            .collect::<Vec<_>>();
        editor.update(cx, |editor, cx| {
            editor.splice_inlays(&[InlayId::Custom(START_TARGET_HINT_INLAY_ID)], inlays, cx);
        });
    }

    /// A blank line's worth of breathing room between the workdir field and
    /// the message body.
    fn insert_body_gap_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(body_start) =
            snapshot.anchor_in_excerpt(self.body_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        editor.update(cx, |editor, cx| {
            editor.insert_blocks(
                [editor::display_map::BlockProperties {
                    placement: editor::display_map::BlockPlacement::Above(body_start),
                    height: Some(1),
                    style: editor::display_map::BlockStyle::Fixed,
                    render: std::sync::Arc::new(|_| gpui::Empty.into_any_element()),
                    priority: 0,
                }],
                None,
                cx,
            );
        });
    }

    fn pin_autoscroll_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(body_end) = snapshot.anchor_in_excerpt(self.body_end) else {
            return;
        };
        editor.update(cx, |editor, cx| {
            editor.set_autoscroll_pin(body_end, AutoscrollStrategy::Bottom, cx);
        });
    }

    fn update_body_chrome(&mut self, cx: &mut Context<Self>) {
        for editor in self.live_editors() {
            self.apply_body_chrome_to(&editor, cx);
        }
        cx.notify();
    }

    /// What the agent host said when it refused this draft, or nothing once the
    /// reader submits again.
    pub fn set_refusal(&mut self, message: Option<String>, cx: &mut Context<Self>) {
        if self.refusal == message {
            return;
        }
        self.refusal = message;
        self.refresh_refusal_blocks(cx);
        cx.notify();
    }

    /// Why the last submission was refused, while it is still on the draft.
    pub fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    fn refresh_refusal_blocks(&mut self, cx: &mut Context<Self>) {
        for (editor, block_id) in self.refusal_blocks.drain(..) {
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |editor, cx| {
                    editor.remove_blocks(
                        std::iter::once(block_id).collect::<HashSet<_>>(),
                        None,
                        cx,
                    );
                });
            }
        }
        let Some(message) = self.refusal.clone() else {
            return;
        };
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(self.body_end)
        else {
            return;
        };
        for editor in self.live_editors() {
            let block = style::refusal_block(anchor, message.clone());
            let ids = editor.update(cx, |editor, cx| editor.insert_blocks([block], None, cx));
            if let Some(block_id) = ids.into_iter().next() {
                self.refusal_blocks.push((editor.downgrade(), block_id));
            }
        }
    }

    fn refresh_attachment_blocks(&mut self, cx: &mut Context<Self>) {
        for (editor, block_id) in self.attachment_blocks.drain(..) {
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |editor, cx| {
                    editor.remove_blocks(
                        std::iter::once(block_id).collect::<HashSet<_>>(),
                        None,
                        cx,
                    );
                });
            }
        }
        if self.attachments.is_empty() {
            return;
        }
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(self.body_end)
        else {
            return;
        };
        let block = style::attachment_block(anchor, crate::attachment_labels(&self.attachments));
        for editor in self.live_editors() {
            let block = block.clone();
            let ids = editor.update(cx, |editor, cx| editor.insert_blocks([block], None, cx));
            if let Some(block_id) = ids.into_iter().next() {
                self.attachment_blocks.push((editor.downgrade(), block_id));
            }
        }
    }

    fn apply_body_chrome_to(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let body_empty = self.body_buffer.read(cx).is_empty();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(body_start) =
            snapshot.anchor_in_excerpt(self.body_buffer.read(cx).anchor_before(0))
        else {
            return;
        };
        let Some(body_end) = snapshot.anchor_in_excerpt(self.body_end) else {
            return;
        };

        let mut inlays = Vec::new();
        if body_empty {
            inlays.push(Inlay::custom(
                BODY_PLACEHOLDER_INLAY_ID,
                body_end,
                "Write a message…",
            ));
        }
        let body_highlight = if body_empty {
            Vec::new()
        } else {
            vec![body_start..body_end]
        };
        let body_style = StyleClass::UserMessage.resolve(cx);
        editor.update(cx, |editor, cx| {
            editor.splice_inlays(&[InlayId::Custom(BODY_PLACEHOLDER_INLAY_ID)], inlays, cx);
            editor.highlight_text(
                HighlightKey::SyntaxTreeView(PROMPT_DRAFT_HIGHLIGHT_KEY),
                body_highlight,
                body_style,
                cx,
            );
            editor.highlight_gutter::<DraftGutter>(
                vec![body_start..body_end],
                style::user_prompt_gutter_color,
                cx,
            );
        });
    }

    fn note_draft_edit(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        if self.suppress_draft_activation {
            return;
        }
        cx.emit(Event::Edited);
    }
}

/// Host-owned candidate lists for the three draft fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftField {
    Workdir,
    Role,
    Start,
}

/// Touch controls request the same host operations as the desktop draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhoneDraftEvent {
    PickField(DraftField),
    Submit,
    AttachImages,
}

/// The phone's draft surface, with independent cursors over canonical buffers.
/// It owns no draft values and no submission logic.
pub struct PhoneDraft {
    model: Entity<DraftModel>,
    workdir: Entity<Editor>,
    role: Entity<Editor>,
    start: Entity<Editor>,
    body: Entity<Editor>,
    scroll: gpui::ScrollHandle,
    last_viewport: gpui::Size<gpui::Pixels>,
    last_focus: Option<usize>,
    _subscription: Subscription,
}

impl gpui::EventEmitter<PhoneDraftEvent> for PhoneDraft {}

impl PhoneDraft {
    pub fn body_editor(&self) -> Entity<Editor> {
        self.body.clone()
    }

    pub fn field_editor(&self, field: DraftField) -> Entity<Editor> {
        match field {
            DraftField::Workdir => self.workdir.clone(),
            DraftField::Role => self.role.clone(),
            DraftField::Start => self.start.clone(),
        }
    }

    pub fn focus_field(&self, field: DraftField, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.field_editor(field).read(cx).focus_handle(cx), cx);
    }

    /// Focus only the message editor; the desktop's multibuffer selection is
    /// intentionally independent.
    pub fn focus_body(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.body.read(cx).focus_handle(cx), cx);
    }

    fn field(
        &self,
        field: DraftField,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_color(colors.text_muted).child(label))
                    .child(
                        div()
                            .id(("draft-pick", field as usize))
                            .min_h(px(44.))
                            .px_3()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .text_color(colors.text_accent)
                            .child("Choose")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(PhoneDraftEvent::PickField(field));
                            })),
                    ),
            )
            .child(
                div()
                    .min_h(px(44.))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .child(self.field_editor(field)),
            )
    }
}

impl Render for PhoneDraft {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let mode = model.start_mode();
        let refusal = model.refusal().map(str::to_owned);
        let attachments = crate::attachment_labels(&model.attachments);
        let target = model.start_text(cx);
        let hint = model
            .start_target_hints
            .iter()
            .find(|(label, _)| label == target.trim())
            .map(|(_, hint)| hint.clone());
        let focused = [
            (1, &self.workdir),
            (2, &self.role),
            (4, &self.start),
            (6 + usize::from(hint.is_some()), &self.body),
        ]
        .into_iter()
        .find_map(|(index, editor)| {
            editor
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
                .then_some(index)
        });
        if self.last_viewport != window.viewport_size() || self.last_focus != focused {
            if let Some(index) = focused {
                // ScrollHandle reads the previous frame's viewport bounds.
                // Wait until this resize/focus frame has laid out the form.
                let form = cx.entity().downgrade();
                window.on_next_frame(move |_, cx| {
                    if let Some(form) = form.upgrade() {
                        form.update(cx, |form, cx| {
                            form.scroll.scroll_to_item(index);
                            cx.notify();
                        });
                    }
                });
            }
            self.last_viewport = window.viewport_size();
            self.last_focus = focused;
        }
        let colors = cx.theme().colors().clone();
        let mut modes = div().flex().gap_2();
        for (index, value, label) in [
            (0, StartFieldMode::NewOn, "New on"),
            (1, StartFieldMode::Join, "Join"),
        ] {
            modes = modes.child(
                div()
                    .id(("draft-start-mode", index as usize))
                    .flex_1()
                    .min_h(px(44.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .border_1()
                    .border_color(if mode == value {
                        colors.text_accent
                    } else {
                        colors.border
                    })
                    .text_color(if mode == value {
                        colors.text_accent
                    } else {
                        colors.text_muted
                    })
                    .cursor_pointer()
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.model
                            .update(cx, |model, cx| model.set_start_mode(value, cx));
                    })),
            );
        }
        div()
            .id("phone-draft")
            .track_scroll(&self.scroll)
            .size_full()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_3()
            .text_color(colors.text)
            .child(div().text_lg().child("New agent"))
            .child(self.field(DraftField::Workdir, "Workdir", cx))
            .child(self.field(DraftField::Role, "Role", cx))
            .child(modes)
            .child(self.field(
                DraftField::Start,
                if mode == StartFieldMode::NewOn {
                    "On top of"
                } else {
                    "Join"
                },
                cx,
            ))
            .children(hint.map(|hint| div().text_sm().text_color(colors.text_muted).child(hint)))
            .child(div().text_color(colors.text_muted).child("First message"))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .debug_selector(|| "phone-draft-body".into())
                    .child(self.body.clone()),
            )
            .children(attachments.iter().map(|label| {
                div()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child(label.clone())
            }))
            .when(!attachments.is_empty(), |el| {
                el.child(
                    div()
                        .id("draft-clear-images")
                        .min_h(px(44.))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .text_color(colors.text_accent)
                        .child("Remove images")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.update(cx, |model, cx| {
                                model.clear_attachments(cx);
                            });
                        })),
                )
            })
            .children(
                refusal.map(|message| div().text_color(colors.terminal_ansi_red).child(message)),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .id("draft-attach")
                            .flex_1()
                            .min_h(px(44.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .cursor_pointer()
                            .child("Paste image")
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(PhoneDraftEvent::AttachImages)),
                            ),
                    )
                    .child(
                        div()
                            .id("draft-submit")
                            .flex_1()
                            .min_h(px(44.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .bg(colors.element_selected)
                            .text_color(colors.text_accent)
                            .cursor_pointer()
                            .child("Create agent")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PhoneDraftEvent::Submit))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    fn init(cx: &mut gpui::App) {
        assets::Assets.load_test_fonts(cx);
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    }

    #[gpui::test]
    fn phone_editors_target_distinct_canonical_fields(cx: &mut TestAppContext) {
        cx.update(init);
        let model = cx.new(|cx| DraftModel::new(Hooks::new(|_, _, _, _| {}), cx));
        model.update(cx, |model, cx| model.set_body_text("Keep this message", cx));
        let window = cx.update(|cx| {
            cx.open_window(gpui::WindowOptions::default(), |window, cx| {
                model.update(cx, |model, cx| model.build_phone_form(window, cx))
            })
            .unwrap()
        });
        window
            .update(cx, |form, window, cx| {
                for (field, text) in [
                    (DraftField::Workdir, "/src/asymmetric-project"),
                    (DraftField::Role, "reviewer-special"),
                    (DraftField::Start, "feature/base-17"),
                ] {
                    form.focus_field(field, window, cx);
                    let field_editor = form.field_editor(field);
                    assert!(field_editor.read(cx).focus_handle(cx).is_focused(window));
                    field_editor.update(cx, |editor, cx| {
                        editor.set_text("", window, cx);
                        editor.insert(text, window, cx);
                    });
                }
                assert_eq!(model.read(cx).workdir_text(cx), "/src/asymmetric-project");
                assert_eq!(model.read(cx).role_text(cx), "reviewer-special");
                assert_eq!(model.read(cx).start_text(cx), "feature/base-17");
                assert_eq!(model.read(cx).body_text(cx), "Keep this message");

                form.body_editor().update(cx, |editor, cx| {
                    editor.set_text("  Submit only this body\n", window, cx);
                });
                assert_eq!(
                    model.read(cx).content(cx),
                    Some(vec![ContentPart::Text {
                        text: "Submit only this body".into()
                    }]),
                );
                assert_eq!(model.read(cx).workdir_text(cx), "/src/asymmetric-project");
                assert_eq!(model.read(cx).role_text(cx), "reviewer-special");
                assert_eq!(model.read(cx).start_text(cx), "feature/base-17");
            })
            .unwrap();
    }

    #[gpui::test]
    fn phone_and_desktop_share_values_attachments_and_base_mode(cx: &mut TestAppContext) {
        cx.update(init);
        let model = cx.new(|cx| DraftModel::new(Hooks::new(|_, _, _, _| {}), cx));
        let window = cx.update(|cx| {
            cx.open_window(gpui::WindowOptions::default(), |window, cx| {
                model.update(cx, |model, cx| model.build_phone_form(window, cx))
            })
            .unwrap()
        });
        window
            .update(cx, |form, window, cx| {
                model.update(cx, |model, cx| {
                    model.set_workdir_text("/shared/repository", cx);
                    model.set_role_text("architect", cx);
                    model.set_start_text("agent-29", cx);
                    model.set_start_mode(StartFieldMode::Join, cx);
                    model.set_body_text("Shared prompt", cx);
                    model.add_image("image/png".into(), vec![2, 7, 3], cx);
                    model.set_refusal(Some("Host is disconnected".into()), cx);
                });
                assert_eq!(
                    form.field_editor(DraftField::Workdir).read(cx).text(cx),
                    "/shared/repository"
                );
                assert_eq!(
                    form.field_editor(DraftField::Role).read(cx).text(cx),
                    "architect"
                );
                assert_eq!(
                    form.field_editor(DraftField::Start).read(cx).text(cx),
                    "agent-29"
                );
                assert_eq!(form.body_editor().read(cx).text(cx), "Shared prompt");
                assert_eq!(model.read(cx).start_mode(), StartFieldMode::Join);
                assert_eq!(model.read(cx).refusal(), Some("Host is disconnected"));
                assert_eq!(
                    model.read(cx).content(cx),
                    Some(vec![
                        ContentPart::Text {
                            text: "Shared prompt".into()
                        },
                        ContentPart::Image {
                            media_type: "image/png".into(),
                            data: vec![2, 7, 3]
                        },
                    ])
                );
                model.update(cx, |model, cx| {
                    model.cycle_start_mode(cx);
                    assert_eq!(model.start_mode(), StartFieldMode::NewOn);
                    assert_eq!(model.start_text(cx), "agent-29");
                    let desktop = model.build_editor(window, cx);
                    assert!(desktop.read(cx).text(cx).contains("Shared prompt"));
                    model.set_start_mode(StartFieldMode::Join, cx);
                    assert!(model.clear_attachments(cx));
                    assert_eq!(
                        model.content(cx),
                        Some(vec![ContentPart::Text {
                            text: "Shared prompt".into()
                        }])
                    );
                });
            })
            .unwrap();
    }
    #[gpui::test]
    fn phone_draft_keeps_the_focused_field_visible_when_keyboard_resizes(cx: &mut TestAppContext) {
        use gpui::{AppContext as _, VisualTestContext};
        cx.update(init);
        let model = cx.new(|cx| DraftModel::new(Hooks::new(|_, _, _, _| {}), cx));
        let window = cx.update(|cx| {
            cx.open_window(gpui::WindowOptions::default(), |window, cx| {
                model.update(cx, |model, cx| model.build_phone_form(window, cx))
            })
            .unwrap()
        });
        cx.simulate_window_resize(*window, gpui::size(px(360.), px(700.)));
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.draw_window(window.into());
        window
            .update(&mut visual, |form, window, cx| form.focus_body(window, cx))
            .unwrap();
        visual.draw_window(window.into());
        visual.simulate_window_resize(*window, gpui::size(px(360.), px(350.)));
        visual.draw_window(window.into());
        visual
            .update_window(window.into(), |_, window, cx| {
                window.simulate_next_frame(cx)
            })
            .unwrap();
        visual.draw_window(window.into());
        let body = visual.debug_bounds("phone-draft-body").unwrap();
        assert!(
            body.top() >= px(0.) && body.bottom() <= px(350.),
            "{body:?}"
        );
        window
            .update(&mut visual, |form, window, cx| {
                form.focus_field(DraftField::Workdir, window, cx)
            })
            .unwrap();
        visual.draw_window(window.into());
        visual
            .update_window(window.into(), |_, window, cx| {
                window.simulate_next_frame(cx)
            })
            .unwrap();
        visual.draw_window(window.into());
        let field = window
            .update(&mut visual, |form, _, cx| {
                *form.workdir.read(cx).last_bounds().unwrap()
            })
            .unwrap();
        assert!(
            field.top() >= px(0.) && field.bottom() <= px(350.),
            "{field:?}"
        );
    }
}
