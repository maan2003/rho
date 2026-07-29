use gpui::prelude::*;
use gpui::{App, Bounds, Context, Window, WindowBounds, WindowOptions, div, px, rgb, size};
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
}

impl Rail {
    fn canned() -> Self {
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
        Self { registry }
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
            .justify_center()
            .items_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .w(px(480.))
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
    }
}

fn main() {
    gpui_platform::web_init();
    let app = gpui_platform::application().run_embedded(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(720.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Rail::canned()),
        )
        .expect("open spike window");
        cx.activate(true);
    });
    std::mem::forget(app);
}
