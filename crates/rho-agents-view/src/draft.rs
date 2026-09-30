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
use gpui::{
    AnyElement, Context, Entity, Focusable as _, Subscription, WeakEntity, Window, div, px,
};
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
    /// Phone controls share the desktop buffers, not its excerpts or chrome.
    phone_editors: Option<[Entity<Editor>; 4]>,
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
                    cx.notify();
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
            phone_editors: None,
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

    fn ensure_phone_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phone_editors.is_none() {
            let buffers = [
                self.workdir_buffer.clone(),
                self.role_buffer.clone(),
                self.start_buffer.clone(),
                self.body_buffer.clone(),
            ];
            let fields = Fields {
                workdir: self.workdir_buffer.entity_id(),
                role: self.role_buffer.entity_id(),
                start: self.start_buffer.entity_id(),
            };
            let hooks = self.hooks.clone();
            self.phone_editors = Some(std::array::from_fn(|index| {
                let buffer = cx.new(|cx| MultiBuffer::singleton(buffers[index].clone(), cx));
                cx.new(|cx| {
                    let mode = if index == 3 {
                        EditorMode::AutoHeight {
                            min_lines: 6,
                            max_lines: None,
                        }
                    } else {
                        EditorMode::SingleLine
                    };
                    let mut editor = Editor::new(mode, buffer, None, window, cx);
                    rho_window::editor_config::configure(&mut editor, window, cx);
                    editor.set_mouse_click_selection_enabled(true, cx);
                    editor.set_placeholder_text(
                        [
                            "Working directory",
                            "Role",
                            "Agent or revision",
                            "Write a message…",
                        ][index],
                        window,
                        cx,
                    );
                    (hooks.0)(&mut editor, fields, window, cx);
                    editor
                })
            }));
        }
    }

    /// A touch form over the same buffers as the desktop multibuffer. Building
    /// it never focuses an input: the keyboard opens only after a tap.
    pub fn render_phone(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_phone_editors(window, cx);
        let compact = window.viewport_size().height <= px(300.);
        let editors = self.phone_editors.as_ref().unwrap();
        let colors = cx.theme().colors();
        let control = |id: &'static str, label: &'static str, editor: &Entity<Editor>| {
            let focus = editor.focus_handle(cx);
            div()
                .flex()
                .flex_col()
                .gap_1()
                .when(compact, |control| control.gap_0())
                .child(
                    div()
                        .text_color(colors.text_muted)
                        .when(compact, |label| label.text_size(px(13.)))
                        .child(label),
                )
                .child(
                    div()
                        .id(id)
                        .w_full()
                        .min_h(px(48.))
                        .px_2()
                        .py_3()
                        .when(compact, |control| control.py_1())
                        .border_1()
                        .border_color(colors.border)
                        .rounded_md()
                        .cursor_text()
                        .on_click(move |_, window, cx| {
                            window.focus(&focus, cx);
                        })
                        .child(editor.clone()),
                )
        };
        let mut form = div()
            .w_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_4()
            .when(compact, |form| form.px_2().py_0().gap_2())
            .child(control("phone-draft-prompt", "Message", &editors[3]));
        for (index, label) in crate::attachment_labels(&self.attachments)
            .into_iter()
            .enumerate()
        {
            form = form.child(
                div()
                    .id(("phone-draft-attachment", index))
                    .min_h(px(48.))
                    .flex()
                    .items_center()
                    .px_2()
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .child(label),
            );
        }
        if let Some(message) = &self.refusal {
            form = form.child(
                div()
                    .id("phone-draft-refusal")
                    .text_color(colors.text_accent)
                    .child(message.clone()),
            );
        }
        form = form
            .child(control(
                "phone-draft-workdir",
                "Working directory",
                &editors[0],
            ))
            .child(control("phone-draft-role", "Role", &editors[1]));
        // Use the desktop role cycle as the source of available choices.
        let selected_role = self.role_text(cx).trim().to_ascii_lowercase();
        let mut roles = div().flex().flex_wrap().gap_2().w_full();
        let mut role = DEFAULT_ROLE;
        loop {
            let selected =
                selected_role == role || (selected_role.is_empty() && role == DEFAULT_ROLE);
            roles = roles.child(
                div()
                    .id(gpui::SharedString::from(format!("phone-draft-role-{role}")))
                    .flex_1()
                    .min_w(px(100.))
                    .min_h(px(48.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .border_color(if selected {
                        colors.text_accent
                    } else {
                        colors.border
                    })
                    .text_color(if selected {
                        colors.text_accent
                    } else {
                        colors.text
                    })
                    .rounded_md()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_role_text(role, cx);
                        this.note_draft_edit(cx);
                        cx.notify();
                    }))
                    .child(role),
            );
            role = rho_agents_client::create::cycle_agent_role_text(role);
            if role == DEFAULT_ROLE {
                break;
            }
        }
        form = form
            .child(
                div()
                    .text_color(colors.text_muted)
                    .child("Choose an agent role"),
            )
            .child(roles);
        let mut modes = div().flex().gap_2().w_full();
        for (mode, label) in [
            (StartFieldMode::NewOn, "New on"),
            (StartFieldMode::Join, "Join"),
        ] {
            let selected = self.start_mode == mode;
            modes = modes.child(
                div()
                    .id(if mode == StartFieldMode::NewOn {
                        "phone-draft-new-on"
                    } else {
                        "phone-draft-join"
                    })
                    .flex_1()
                    .min_h(px(48.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .border_color(if selected {
                        colors.text_accent
                    } else {
                        colors.border
                    })
                    .text_color(if selected {
                        colors.text_accent
                    } else {
                        colors.text
                    })
                    .rounded_md()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.start_mode != mode {
                            this.cycle_start_mode(cx);
                        }
                    }))
                    .child(label),
            );
        }
        form = form
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_color(colors.text_muted).child("Start mode"))
                    .child(modes),
            )
            .child(control(
                "phone-draft-start",
                if self.start_mode == StartFieldMode::NewOn {
                    "On top of (agent or revision)"
                } else {
                    "Join agent"
                },
                &editors[2],
            ));
        let target = self.start_text(cx);
        if let Some((_, hint)) = self
            .start_target_hints
            .iter()
            .find(|(label, _)| label == target.trim())
        {
            form = form.child(div().text_color(colors.text_muted).child(hint.clone()));
        }
        div()
            .id("phone-draft-form")
            .size_full()
            .overflow_y_scroll()
            .child(form)
            .into_any_element()
    }

    /// The phone prompt, for explicit focus or submission routing. None until
    /// the phone form is first rendered.
    pub fn phone_body_editor(&self) -> Option<Entity<Editor>> {
        self.phone_editors
            .as_ref()
            .map(|editors| editors[3].clone())
    }

    /// The phone control receiving keys, falling back to the prompt when
    /// none is focused (e.g. a touch toolbar or menu has taken focus).
    pub fn phone_active_editor(&self, window: &Window, cx: &gpui::App) -> Option<Entity<Editor>> {
        let editors = self.phone_editors.as_ref()?;
        Some(
            editors
                .iter()
                .find(|editor| editor.focus_handle(cx).contains_focused(window, cx))
                .unwrap_or(&editors[3])
                .clone(),
        )
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
        self.start_mode = match self.start_mode {
            StartFieldMode::NewOn => StartFieldMode::Join,
            StartFieldMode::Join => StartFieldMode::NewOn,
        };
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
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
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
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
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
        cx.notify();
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
        if self.suppress_draft_activation {
            return;
        }
        cx.emit(Event::Edited);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::TestAppContext;

    use super::*;

    struct PhoneForm(Entity<DraftModel>);

    impl gpui::Render for PhoneForm {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let form = self
                .0
                .update(cx, |draft, cx| draft.render_phone(window, cx));
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(form))
                // The persistent phone toolbar still owns its full touch height.
                .child(div().h(px(56.)).flex_shrink_0())
        }
    }

    #[gpui::test]
    fn compact_phone_form_keeps_first_prompt_line_above_toolbar(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            cx.text_system()
                .add_fonts(vec![std::borrow::Cow::Borrowed(
                    include_bytes!("../../../vendor/zed/assets/fonts/lilex/Lilex-Regular.ttf")
                        .as_slice(),
                )])
                .unwrap();
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let draft = cx.new(|cx| {
                let mut draft = DraftModel::new(Hooks::new(|_, _, _, _| {}), cx);
                draft.set_body_text("Visible prompt", cx);
                draft
            });
            PhoneForm(draft)
        });
        cx.simulate_window_resize(*window, gpui::size(px(720.), px(120.)));
        window
            .update(cx, |_, window, cx| {
                window.simulate_next_frame(cx);
            })
            .unwrap();
        window
            .update(cx, |form, window, cx| {
                let draft = form.0.read(cx);
                let prompt = draft.phone_body_editor().unwrap();
                prompt.update(cx, |editor, cx| {
                let bounds = *editor
                    .last_bounds()
                    .expect("prompt must have been rendered");
                let line_height = editor
                    .style(cx)
                    .text
                    .line_height_in_pixels(window.rem_size());
                assert!(
                    bounds.top() + line_height <= px(64.),
                    "first editable line must fit in 120px viewport minus 56px toolbar: {bounds:?}"
                );
                assert!(editor.text(cx).contains("Visible prompt"));
                });
                assert!(
                    !prompt.focus_handle(cx).is_focused(window),
                    "rendering must not open keyboard"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn phone_edits_share_desktop_buffers_without_mixing_fields_and_prompt(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let hook_count = Rc::new(Cell::new(0));
        let configured = hook_count.clone();
        let draft = cx.new(|cx| {
            DraftModel::new(
                Hooks::new(move |_, _, _, _| {
                    configured.set(configured.get() + 1);
                }),
                cx,
            )
        });
        let window = cx.add_window(|_, _| gpui::Empty);
        window.update(cx, |_, window, cx| {
            draft.update(cx, |draft, cx| {
                draft.set_workdir_text("/src/desktop-seed", cx);
                draft.set_role_text("high1-eng", cx);
                draft.set_start_text("base-agent", cx);
                draft.set_body_text("Original prompt\nsecond line", cx);
                draft.ensure_phone_editors(window, cx);
                let phone = draft.phone_editors.as_ref().unwrap().clone();
                assert!(phone.iter().all(|editor| !editor.focus_handle(cx).is_focused(window)));
                assert_eq!(hook_count.get(), 4);
                assert_eq!(draft.phone_active_editor(window, cx).unwrap(), phone[3]);
                window.focus(&phone[1].focus_handle(cx), cx);
                assert_eq!(draft.phone_active_editor(window, cx).unwrap(), phone[1]);
                window.blur();

                // Editing a field must not appear in submission content.
                phone[0].update(cx, |editor, cx| editor.set_text("/src/phone", window, cx));
                phone[1].update(cx, |editor, cx| editor.set_text("mini-eng", window, cx));
                phone[2].update(cx, |editor, cx| editor.set_text("topic-branch", window, cx));
                phone[3].update(cx, |editor, cx| editor.set_text("Phone prompt\nwith details", window, cx));
                draft.cycle_start_mode(cx);
                draft.add_image("image/png".into(), vec![1, 2, 3], cx);
                draft.set_refusal(Some("Choose another target".into()), cx);

                assert_eq!(draft.workdir_text(cx), "/src/phone");
                assert_eq!(draft.role_text(cx), "mini-eng");
                assert_eq!(draft.start_text(cx), "topic-branch");
                assert_eq!(draft.start_mode(), StartFieldMode::Join);
                assert_eq!(draft.body_text(cx), "Phone prompt\nwith details");
                let content = draft.content(cx).unwrap();
                assert!(matches!(&content[0], ContentPart::Text { text } if text == "Phone prompt\nwith details"));
                assert!(matches!(&content[1], ContentPart::Image { media_type, data } if media_type == "image/png" && data == &[1, 2, 3]));
                assert_eq!(content.len(), 2);
                assert_eq!(draft.refusal(), Some("Choose another target"));

                assert!(draft.cursor_in_a_field(&phone[0], cx));
                assert!(draft.cursor_in_role_field(&phone[1], cx));
                assert!(draft.cursor_in_start_field(&phone[2], cx));
                assert!(!draft.cursor_in_a_field(&phone[3], cx));
                draft.focus_body(&phone[3], window, cx);
                assert_eq!(phone[3].read(cx).selections.newest_anchor().head()
                    .to_offset(&phone[3].read(cx).buffer().read(cx).snapshot(cx)),
                    editor::MultiBufferOffset("Phone prompt\nwith details".len()));

                // Switching presentation preserves all values and both surfaces
                // keep observing the same buffers, including subsequent edits.
                let desktop = draft.build_editor(window, cx);
                assert!(desktop.read(cx).text(cx).contains("/src/phone"));
                assert!(desktop.read(cx).text(cx).contains("mini-eng"));
                assert!(desktop.read(cx).text(cx).contains("topic-branch"));
                draft.focus_body(&desktop, window, cx);
                assert!(!draft.cursor_in_a_field(&desktop, cx));
                draft.set_body_text("Desktop replacement", cx);
                draft.set_role_text("med1-eng", cx);
                draft.ensure_phone_editors(window, cx);
                assert_eq!(hook_count.get(), 5, "building again must reuse phone controls");
                assert_eq!(draft.phone_body_editor().unwrap(), phone[3]);
                assert_eq!(phone[3].read(cx).text(cx), "Desktop replacement");
                assert_eq!(phone[1].read(cx).text(cx), "med1-eng");
                assert_eq!(draft.start_mode(), StartFieldMode::Join);
                assert_eq!(draft.refusal(), Some("Choose another target"));
                assert_eq!(draft.content(cx).unwrap().len(), 2);
            });
        }).unwrap();
    }
}
