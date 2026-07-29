use editor::{
    Backspace, Editor, MoveLeft, MoveRight, MoveToBeginningOfLine, MoveToEndOfLine, Newline, Redo,
    Undo,
};
use gpui::prelude::*;
use gpui::{
    App, AssetSource, Bounds, Context, Focusable, KeyBinding, Window, WindowBounds, WindowOptions,
    div, px, rgb, size,
};
use rho_core::UnixMs;
use rho_registry::AgentRegistry;
use rho_ui_proto::{
    AgentId, AgentIdDomain, AgentRole, UiAgentSummary, UiAttention, UiWorkstream, WorkspaceInfo,
    WorkstreamId,
};

const BACKGROUND: u32 = 0x111318;
const RAIL: u32 = 0x191c23;
const ROW: u32 = 0x222630;
const TEXT: u32 = 0xe7eaf0;
const MUTED: u32 = 0x89909f;
const WORKING: u32 = 0x79c0ff;
const PENDING: u32 = 0xf2cc60;
const NEEDS_INPUT: u32 = 0xff7b72;

struct Rail {
    registry: AgentRegistry,
    editor: gpui::Entity<Editor>,
}

impl Rail {
    fn canned(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut registry = AgentRegistry::default();
        registry.set_machine_seed(7);
        registry.set_agent_counter(4);
        registry.set_data(
            vec![
                UiWorkstream {
                    workstream_id: WorkstreamId(1),
                    name: "GPUI web feasibility".into(),
                    labels: vec!["pin".into()],
                },
                UiWorkstream {
                    workstream_id: WorkstreamId(2),
                    name: "Daemon reliability".into(),
                    labels: Vec::new(),
                },
            ],
            vec![
                agent(
                    1,
                    1,
                    None,
                    "Port the Rho rail",
                    AgentRole::pm(),
                    UiAttention::Pending,
                ),
                agent(
                    2,
                    1,
                    Some(1),
                    "Render plain GPUI rows",
                    AgentRole::default(),
                    UiAttention::Working,
                ),
                agent(
                    3,
                    1,
                    Some(1),
                    "Investigate browser input",
                    AgentRole::default(),
                    UiAttention::NeedsInput,
                ),
                agent(
                    4,
                    2,
                    None,
                    "Bound reconnect backoff",
                    AgentRole::default(),
                    UiAttention::Quiet,
                ),
            ],
        );
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(
                "// The real Zed editor, running in WebAssembly\n\
                 fn browser_editor() {\n\
                 \tlet editing = \"typing, selection, movement\";\n\
                 \tprintln!(\"{editing}\");\n\
                 }\n\n\
                 Try typing here. Backspace, Enter, arrows, Home/End,\n\
                 scrolling, Ctrl-Z and Ctrl-Shift-Z are wired to Editor.",
                window,
                cx,
            );
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        Self { registry, editor }
    }
}

fn agent(
    counter: u64,
    workstream: u64,
    parent: Option<u64>,
    name: &str,
    role: AgentRole,
    attention: UiAttention,
) -> UiAgentSummary {
    let domain = AgentIdDomain(7);
    let id = |counter| AgentId::from_counter(counter, &domain).expect("small canned agent id");
    UiAgentSummary {
        agent_id: id(counter),
        parent_agent: parent.map(id),
        display_name: Some(name.into()),
        created_at: UnixMs(counter),
        updated_at: UnixMs(counter),
        role,
        workspace: WorkspaceInfo::UserCheckout {
            repo: "/rho".into(),
        },
        attention,
        last_active: UnixMs(100 - counter),
        hidden: false,
        last_user_message_text: String::new(),
        workstream: WorkstreamId(workstream),
        labels: Vec::new(),
    }
}

fn attention_color(attention: UiAttention) -> u32 {
    match attention {
        UiAttention::Quiet => MUTED,
        UiAttention::Working => WORKING,
        UiAttention::Pending => PENDING,
        UiAttention::NeedsInput => NEEDS_INPUT,
    }
}

fn role_label(role: AgentRole) -> &'static str {
    if role.is_pm() {
        "PM"
    } else if role.is_engineer() {
        "Engineer"
    } else {
        "Advisor"
    }
}

impl Render for Rail {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut workstream_rows = div().flex().flex_col().gap_3();
        for workstream in self.registry.ordered_workstreams() {
            let mut agent_rows = div().flex().flex_col().gap_1();
            for (agent, depth) in self.registry.ordered_workstream_tree(workstream) {
                let color = attention_color(self.registry.attention(agent.agent_id));
                agent_rows = agent_rows.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .pl(px(12. + depth as f32 * 18.))
                        .pr_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(ROW))
                        .text_color(rgb(TEXT))
                        .child(div().text_color(rgb(color)).child("●"))
                        .child(
                            div()
                                .flex_1()
                                .child(self.registry.agent_human_name(agent.agent_id)),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child(role_label(agent.role)),
                        ),
                );
            }
            workstream_rows = workstream_rows.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .text_color(rgb(TEXT))
                            .child(if workstream.pinned { "◆  " } else { "" })
                            .child(workstream.name.clone()),
                    )
                    .child(agent_rows),
            );
        }

        div()
            .flex()
            .size_full()
            .bg(rgb(BACKGROUND))
            .gap_4()
            .p_5()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .w(px(420.))
                    .p_5()
                    .rounded_lg()
                    .bg(rgb(RAIL))
                    .child(div().text_xl().text_color(rgb(TEXT)).child("Rho agents"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child("canned Ready snapshot · rho-registry ordering"),
                    )
                    .child(workstream_rows),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .h_full()
                    .overflow_hidden()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(ROW))
                    .child(self.editor.clone()),
            )
    }
}

fn main() {
    gpui_platform::web_init();
    let app = gpui_platform::application().run_embedded(|cx: &mut App| {
        let assets = assets::Assets;
        let fonts = assets
            .list("fonts")
            .expect("list embedded fonts")
            .into_iter()
            .filter(|path| path.ends_with(".ttf"))
            .map(|path| assets.load(&path).expect("load embedded font").unwrap())
            .collect();
        cx.text_system().add_fonts(fonts).expect("add editor fonts");
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        cx.bind_keys([
            KeyBinding::new("left", MoveLeft, Some("Editor")),
            KeyBinding::new("right", MoveRight, Some("Editor")),
            KeyBinding::new("up", zed_actions::editor::MoveUp, Some("Editor")),
            KeyBinding::new("down", zed_actions::editor::MoveDown, Some("Editor")),
            KeyBinding::new("home", MoveToBeginningOfLine::default(), Some("Editor")),
            KeyBinding::new("end", MoveToEndOfLine::default(), Some("Editor")),
            KeyBinding::new("backspace", Backspace, Some("Editor")),
            KeyBinding::new("enter", Newline, Some("Editor")),
            KeyBinding::new("ctrl-z", Undo, Some("Editor")),
            KeyBinding::new("ctrl-shift-z", Redo, Some("Editor")),
        ]);

        let bounds = Bounds::centered(None, size(px(1180.), px(720.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Rail::canned(window, cx)),
        )
        .expect("open spike window");
        cx.activate(true);
    });
    std::mem::forget(app);
}
