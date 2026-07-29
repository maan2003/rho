//! Leptos views: connect/enroll screens, agent rail, chat pane, composer.

// Component functions follow leptos' PascalCase convention.
#![allow(non_snake_case)]

use std::collections::{BTreeSet, HashSet};

use leptos::html;
use leptos::prelude::*;
use rho_core::{AgentRole, ContentPart, EngineerIntelligence, MessageDelivery};
use rho_registry::{Workstream, agent_pinned};
use rho_ui_proto::remote::{UiAgentStatus, UiBlock, UiMessagePhase, UiTool, UiToolStatus};
use rho_ui_proto::{
    AgentId, ClientMessage, JoinTarget, StartMode, UiAgentSummary, UiAttention, WorkstreamId,
};
use wasm_bindgen::JsCast as _;

use crate::{App, Phase, conn, md};

const AUTO_BASE_REVSET: &str =
    r#"coalesce(bookmarks(exact:"main"), bookmarks(exact:"master"), trunk())"#;

pub fn Root(app: App) -> impl IntoView {
    view! {
        <div class="shell" class=("chat-open", move || app.chat_open.get())>
            {move || match app.phase.get() {
                Phase::NeedDaemon => ConnectScreen(app).into_any(),
                Phase::Unlock(daemon) => UnlockScreen(app, daemon).into_any(),
                Phase::Connecting => StatusScreen("Connecting to your daemon…", None).into_any(),
                Phase::Enroll(code) => EnrollScreen(code).into_any(),
                Phase::Failed(message) => StatusScreen("Connection failed", Some(message)).into_any(),
                Phase::Online => Main(app).into_any(),
            }}
            {move || app.toast.get().map(|message| view! { <div class="toast">{message}</div> })}
        </div>
    }
}

fn UnlockScreen(app: App, daemon: String) -> impl IntoView {
    let connect_daemon = daemon.clone();
    let reset_daemon = daemon.clone();
    let security_key_daemon = daemon.clone();
    let short = if daemon.len() > 20 {
        format!("{}…", &daemon[..20])
    } else {
        daemon
    };
    view! {
        <div class="screen">
            <div class="card">
                <div class="logo">"ρ"</div>
                <h1>"Unlock Rho"</h1>
                <p class="muted">"Use your passkey to connect to daemon " <code>{short}</code> "."</p>
                <button class="primary" on:click=move |_| conn::unlock(app, connect_daemon.clone())>
                    "Unlock and connect"
                </button>
                <button on:click=move |_| {
                    conn::reset_passkey();
                    conn::unlock(app, reset_daemon.clone());
                }>"Use a new passkey"</button>
                <button on:click=move |_| {
                    conn::reset_to_security_key();
                    conn::unlock(app, security_key_daemon.clone());
                }>"Use a security key"</button>
            </div>
        </div>
    }
}

fn ConnectScreen(app: App) -> impl IntoView {
    let input: NodeRef<html::Input> = NodeRef::new();
    let connect = move || {
        if let Some(element) = input.get_untracked() {
            let value = element.value();
            let value = value.trim();
            if !value.is_empty() {
                conn::set_daemon(app, value.to_owned());
            }
        }
    };
    view! {
        <div class="screen">
            <div class="card">
                <div class="logo">"ρ"</div>
                <h1>"Rho"</h1>
                <p class="muted">
                    "Enter your daemon's iroh endpoint id. The daemon prints it on "
                    "startup when run with " <code>"rho daemon --iroh"</code> "."
                </p>
                <input
                    type="text"
                    placeholder="daemon endpoint id"
                    node_ref=input
                    on:keydown=move |event| {
                        if event.key() == "Enter" {
                            connect();
                        }
                    }
                />
                <button class="primary" on:click=move |_| connect()>"Connect"</button>
            </div>
        </div>
    }
}

fn EnrollScreen(code: String) -> impl IntoView {
    view! {
        <div class="screen">
            <div class="card">
                <div class="logo">"ρ"</div>
                <h1>"Approve this browser"</h1>
                <p class="muted">
                    "This browser is not enrolled yet. On the machine running the "
                    "daemon, run:"
                </p>
                <pre class="code approve">{format!("rho iroh approve {code}")}</pre>
                <p class="muted">"After approval, reconnect with the same passkey before the code expires."</p>
                <button class="primary" on:click=|_| {
                    if let Some(window) = web_sys::window() {
                        let _ = window.location().reload();
                    }
                }>"Reconnect after approval"</button>
            </div>
        </div>
    }
}

fn StatusScreen(title: &'static str, detail: Option<String>) -> impl IntoView {
    view! {
        <div class="screen">
            <div class="card">
                <div class="logo">"ρ"</div>
                <h1>{title}</h1>
                {detail.map(|detail| view! { <p class="muted">{detail}</p> })}
                {(title == "Connection failed").then(|| view! {
                    <button class="primary" on:click=|_| {
                        if let Some(window) = web_sys::window() {
                            let _ = window.location().reload();
                        }
                    }>"Reload"</button>
                })}
            </div>
        </div>
    }
}

#[derive(Clone)]
enum WebRailRow {
    Group(String),
    Workstream(Workstream, bool),
}

fn structured_rows(registry: &rho_registry::AgentRegistry, folded: bool) -> Vec<WebRailRow> {
    let (listed, tail) = registry.split_rows();
    let display = if folded {
        listed.into_iter().chain(tail).collect::<Vec<_>>()
    } else {
        listed
    };
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for workstream in &display {
        match &workstream.group {
            None => rows.push(WebRailRow::Workstream((*workstream).clone(), false)),
            Some(group) if seen.insert(group.clone()) => {
                rows.push(WebRailRow::Group(group.clone()));
                rows.extend(
                    display
                        .iter()
                        .filter(|candidate| candidate.group.as_ref() == Some(group))
                        .map(|candidate| WebRailRow::Workstream((*candidate).clone(), true)),
                );
            }
            Some(_) => {}
        }
    }
    rows
}

fn Main(app: App) -> impl IntoView {
    Effect::new(move |_| {
        app.registry_epoch.track();
        let count = app.registry.with_value(|registry| {
            registry
                .workstreams()
                .iter()
                .flat_map(|workstream| &workstream.agents)
                .filter(|agent| {
                    !agent.hidden && registry.attention(agent.agent_id) == UiAttention::NeedsInput
                })
                .count()
        });
        if let Some(document) = web_sys::window().and_then(|window| window.document()) {
            let title = if count > 0 {
                format!("({count}) Rho")
            } else {
                "Rho".to_owned()
            };
            document.set_title(&title);
        }
    });
    let open_agents: RwSignal<HashSet<AgentId>> = RwSignal::new(HashSet::new());
    let folded_open = RwSignal::new(false);
    view! {
        <div class="rail">
            <div class="rail-head">
                <div class="brand"><span class="logo small">"ρ"</span>"rho"</div>
                <button class="new-agent" title="New agent" on:click=move |_| {
                    app.show_new_agent.set(true);
                    app.chat_open.set(true);
                }>
                    <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4">
                        <path d="M11.5 2.5l2 2L6 12l-2.7.7.7-2.7 7.5-7.5z"/>
                    </svg>
                    <span>"New"</span>
                </button>
            </div>
            <div class="topics">
                {move || {
                    app.registry_epoch.track();
                    open_agents.track();
                    let rows = app.registry.with_value(|registry| structured_rows(registry, folded_open.get()));
                    let folded_count = app.registry.with_value(|registry| registry.split_rows().1.len());
                    view! {
                        {rows.into_iter().map(|row| match row {
                            WebRailRow::Group(name) => view! { <div class="fold-row">{name}</div> }.into_any(),
                            WebRailRow::Workstream(workstream, grouped) =>
                                WorkstreamRow(app, workstream, grouped, open_agents).into_any(),
                        }).collect_view()}
                        {(folded_count > 0).then(|| view! {
                            <button class="fold-row" on:click=move |_| folded_open.update(|value| *value = !*value)>
                                <span>{move || if folded_open.get() { "⌃" } else { "⌄" }}</span>
                                <span>{move || if folded_open.get() { "Show less".to_owned() } else { format!("{folded_count} more") }}</span>
                            </button>
                        })}
                    }
                }}
            </div>
            <div class="rail-foot" title="connected">
                <span class="dot ok"></span><span class="foot-label">{daemon_short()}</span>
            </div>
        </div>
        <div class="chat">
            {move || if app.show_new_agent.get() {
                NewAgentPage(app).into_any()
            } else {
                match app.selected.get() {
                    Some(agent_id) => ChatPane(app, agent_id).into_any(),
                    None => view! {
                        <div class="placeholder"><div class="logo big">"ρ"</div>
                        <p class="muted">"Pick an agent, or start a new one."</p></div>
                    }.into_any(),
                }
            }}
        </div>
    }
}

fn attention_for_tree(
    registry: &rho_registry::AgentRegistry,
    tree: &[(&UiAgentSummary, usize)],
) -> UiAttention {
    tree.iter()
        .map(|(agent, _)| registry.attention(agent.agent_id))
        .max()
        .unwrap_or_default()
}

fn lamp_view(attention: UiAttention) -> Option<AnyView> {
    let (glyph, class) = match attention {
        UiAttention::Working => ("…", "working"),
        UiAttention::Pending => ("●", "pending"),
        UiAttention::NeedsInput => ("◆", "needs_input"),
        UiAttention::Quiet => return None,
    };
    Some(view! { <span class=format!("lamp {class}")>{glyph}</span> }.into_any())
}

fn WorkstreamRow(
    app: App,
    workstream: Workstream,
    grouped: bool,
    open_agents: RwSignal<HashSet<AgentId>>,
) -> impl IntoView {
    let (tree, roots, names, aggregate) = app.registry.with_value(|registry| {
        let tree = registry.ordered_workstream_tree(&workstream);
        let roots = registry.ordered_workstream_roots(&workstream);
        let names = tree
            .iter()
            .map(|(agent, _)| (agent.agent_id, registry.agent_human_name(agent.agent_id)))
            .collect::<Vec<_>>();
        let aggregate = attention_for_tree(registry, &tree);
        (
            tree.into_iter()
                .map(|(agent, depth)| (agent.clone(), depth))
                .collect::<Vec<_>>(),
            roots
                .into_iter()
                .map(|agent| agent.agent_id)
                .collect::<Vec<_>>(),
            names,
            aggregate,
        )
    });
    let names = names
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
    let attentions = app.registry.with_value(|registry| {
        tree.iter()
            .enumerate()
            .map(|(index, (agent, depth))| {
                let attention = if *depth == 0 {
                    let end = tree[index + 1..]
                        .iter()
                        .position(|(_, candidate_depth)| *candidate_depth == 0)
                        .map_or(tree.len(), |offset| index + 1 + offset);
                    tree[index..end]
                        .iter()
                        .map(|(agent, _)| registry.attention(agent.agent_id))
                        .max()
                        .unwrap_or_default()
                } else {
                    registry.attention(agent.agent_id)
                };
                (agent.agent_id, attention)
            })
            .collect::<std::collections::HashMap<_, _>>()
    });
    let singleton = roots.len() == 1;
    let root = singleton.then(|| roots[0]);
    let title = if workstream.name.trim().is_empty() {
        "Untitled workstream".to_owned()
    } else {
        workstream.name.clone()
    };
    let active_agents = tree
        .iter()
        .map(|(agent, _)| agent.agent_id)
        .collect::<Vec<_>>();
    let header_fold = root.and_then(|root| {
        tree.iter()
            .position(|(agent, _)| agent.agent_id == root)
            .and_then(|index| (tree.len() > index + 1).then_some(root))
    });
    view! {
        <div class="stream" class:grouped=grouped>
            <div class="stream-row" role="button"
                class:active=move || app.selected.get().is_some_and(|selected| active_agents.iter().any(|id| *id == selected))
                on:click=move |_| { if let Some(root) = root { app.select(root); } }>
                {workstream.pinned.then(|| view! { <span class="pin-mark">"◆ "</span> })}
                <span class="stream-title">{title}</span>
                {header_fold.map(|fold| Disclosure(fold, open_agents))}
                <span class="rail-spacer"></span>
                {lamp_view(aggregate)}
            </div>
            {tree.into_iter().enumerate().filter_map(|(index, (agent, depth))| {
                if singleton && index == 0 { return None; }
                let always_visible = depth == 0;
                let visible = always_visible || ancestors_open(&agent, &workstream, open_agents);
                visible.then(|| {
                    let has_children = tree_has_children(&workstream, agent.agent_id);
                    let name = names.get(&agent.agent_id).cloned().unwrap_or_else(|| agent.agent_id.encoded());
                    let attention = attentions.get(&agent.agent_id).copied().unwrap_or_default();
                    AgentLine(app, agent, if singleton { depth } else { depth + 1 }, name, attention, has_children, open_agents)
                })
            }).collect_view()}
        </div>
    }
}

fn ancestors_open(
    agent: &UiAgentSummary,
    workstream: &Workstream,
    open: RwSignal<HashSet<AgentId>>,
) -> bool {
    let by_id = workstream
        .agents
        .iter()
        .map(|agent| (agent.agent_id, agent))
        .collect::<std::collections::HashMap<_, _>>();
    let mut parent = agent.parent_agent;
    while let Some(id) = parent {
        if !open.with(|set| set.contains(&id)) {
            return false;
        }
        parent = by_id.get(&id).and_then(|agent| agent.parent_agent);
    }
    true
}

fn tree_has_children(workstream: &Workstream, id: AgentId) -> bool {
    workstream
        .agents
        .iter()
        .any(|agent| agent.parent_agent == Some(id) && !agent.hidden)
}

fn Disclosure(agent_id: AgentId, open: RwSignal<HashSet<AgentId>>) -> AnyView {
    view! {
        <button class="disclose" on:click=move |event| {
            event.stop_propagation();
            open.update(|set| { if !set.remove(&agent_id) { set.insert(agent_id); } });
        }>{move || if open.with(|set| set.contains(&agent_id)) { "⌄" } else { "›" }}</button>
    }
    .into_any()
}

fn AgentLine(
    app: App,
    agent: UiAgentSummary,
    depth: usize,
    name: String,
    attention: UiAttention,
    has_children: bool,
    open: RwSignal<HashSet<AgentId>>,
) -> impl IntoView {
    let id = agent.agent_id;
    let role = role_label(agent.role);
    view! {
        <button class="agent-line" style=format!("padding-left: {}px", 10 + depth * 16)
            class:active=move || app.selected.get() == Some(id)
            on:click=move |_| app.select(id)>
            {agent_pinned(&agent).then(|| view! { <span class="pin-mark">"◆ "</span> })}
            <span class="agent-line-name">{name}</span>
            {has_children.then(|| Disclosure(id, open))}
            <span class="agent-line-role">{role}</span>
            <span class="rail-spacer"></span>
            {lamp_view(attention)}
        </button>
    }
}

fn daemon_short() -> String {
    match conn::daemon_id() {
        Some(id) if id.len() > 12 => format!("{}…", &id[..12]),
        Some(id) => id,
        None => "connected".to_owned(),
    }
}

fn ChatPane(app: App, agent_id: AgentId) -> impl IntoView {
    let summary = Memo::new(move |_| {
        app.registry_epoch.track();
        app.registry.with_value(|registry| {
            registry
                .workstreams()
                .iter()
                .flat_map(|workstream| &workstream.agents)
                .find(|agent| agent.agent_id == agent_id)
                .cloned()
        })
    });
    view! {
        <div class="chat-head">
            <button class="back" on:click=move |_| app.chat_open.set(false)>"‹"</button>
            <span class="chat-name">{move || app.registry.with_value(|registry| registry.agent_human_name(agent_id))}</span>
            {move || summary.get().map(|agent| view! { <span class="chip mode">{role_label(agent.role)}</span> })}
        </div>
        <Transcript app=app />
        <Composer app=app />
    }
}

#[component]
fn Transcript(app: App) -> impl IntoView {
    let scroller: NodeRef<html::Div> = NodeRef::new();
    let pinned = RwSignal::new(true);
    Effect::new(move |_| {
        app.state_epoch.track();
        if !pinned.get_untracked() {
            return;
        }
        if let Some(element) = scroller.get_untracked() {
            request_animation_frame(move || element.set_scroll_top(element.scroll_height()));
        }
    });
    let jump_to_latest = move |_| {
        if let Some(element) = scroller.get_untracked() {
            element.set_scroll_top(element.scroll_height());
        }
        pinned.set(true);
    };
    view! {
        <div class="transcript" node_ref=scroller on:scroll=move |_| {
            if let Some(element) = scroller.get_untracked() {
                pinned.set(element.scroll_height() - element.scroll_top() - element.client_height() < 60);
            }
        }>
            <div class="column">{move || {
                app.state_epoch.track();
                let state = app.selected.get().and_then(|id| app.store.with_value(|store| store.get(&id).cloned()));
                match state {
                    None => view! { <p class="muted loading">"Loading transcript…"</p> }.into_any(),
                    Some(state) => {
                        let busy = matches!(state.status, UiAgentStatus::Streaming | UiAgentStatus::ToolCalling { .. });
                        Blocks(&state.blocks, busy).into_any()
                    }
                }
            }}</div>
            {move || (!pinned.get()).then(|| view! { <button class="jump-latest" on:click=jump_to_latest>"↓ latest"</button> })}
        </div>
    }
}

fn Blocks(blocks: &[UiBlock], busy: bool) -> impl IntoView {
    let mut views = Vec::new();
    let mut index = 0;
    while index < blocks.len() {
        if matches!(blocks[index], UiBlock::Reasoning { .. }) {
            index += 1;
            continue;
        }
        let run_end = blocks[index..]
            .iter()
            .position(|block| !matches!(block, UiBlock::Tool(_)))
            .map(|offset| index + offset)
            .unwrap_or(blocks.len());
        if run_end == index {
            views.push(BlockView(&blocks[index]));
            index += 1;
        } else {
            let run = &blocks[index..run_end];
            let tail_open = busy && run_end == blocks.len();
            if run.len() > 1 && !tail_open {
                views.push(ToolFold(run.to_vec()));
            } else {
                views.extend(run.iter().map(BlockView));
            }
            index = run_end;
        }
    }
    views.collect_view()
}

fn tool_duration_ms(tool: &UiTool) -> Option<u64> {
    Some(tool.finished_at?.0.saturating_sub(tool.started_at?.0))
}

fn ToolFold(run: Vec<UiBlock>) -> AnyView {
    let open = RwSignal::new(false);
    let total_ms = run
        .iter()
        .filter_map(|block| match block {
            UiBlock::Tool(tool) => tool_duration_ms(tool),
            _ => None,
        })
        .sum();
    let label = if total_ms >= 1000 {
        format!("Worked for {}", format_duration(total_ms))
    } else {
        format!("{} tools", run.len())
    };
    view! {
        <div class="tool-fold">
            <button class="fold-head" on:click=move |_| open.update(|value| *value = !*value)>
                <span class="fold-label">{label}</span><span class="chev">{move || if open.get() { "⌄" } else { "›" }}</span>
            </button>
            {move || open.get().then(|| run.iter().map(BlockView).collect_view())}
        </div>
    }.into_any()
}

fn format_duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

fn BlockView(block: &UiBlock) -> AnyView {
    match block {
        UiBlock::UserMessage { text } => view! { <div class="row user"><div class="bubble user">{text.clone()}</div></div> }.into_any(),
        UiBlock::AssistantMessage { text, phase } => {
            let class = if matches!(phase, Some(UiMessagePhase::FinalAnswer)) { "block assistant final" } else { "block assistant" };
            view! { <div class=class inner_html=md::render(text)></div> }.into_any()
        }
        UiBlock::Reasoning { .. } => ().into_any(),
        UiBlock::Tool(tool) => ToolLine(tool),
        UiBlock::Notice { text } => view! { <div class="block notice">{text.clone()}</div> }.into_any(),
        UiBlock::QueuedMessage { text, .. } => view! { <div class="row user"><div class="bubble user queued">{text.clone()}</div></div> }.into_any(),
        UiBlock::AgentMessage { sender, text } => view! {
            <div class="block agent-msg"><div class="sender">{format!("from {}", sender.encoded())}</div>
            <div inner_html=md::render(text)></div></div>
        }.into_any(),
    }
}

const TOOL_TEXT_LIMIT: usize = 16 * 1024;
const TOOL_LABEL_LIMIT: usize = 256;

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} …[truncated]", &text[..end])
}

fn streaming_json_text_field(arguments: &str, key: &str) -> Option<String> {
    let mut parser = json_stream::JsonStreamParser::new();
    for character in arguments.chars() {
        parser.add_char(character).ok()?;
    }
    parser
        .get_result()
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn tool_label(name: &str, arguments: &str) -> String {
    let label = match name {
        "shell" | "shell_command" | "exec_command" | "write_stdin" | "Bash" => {
            let command = streaming_json_text_field(arguments, "command")
                .or_else(|| {
                    (!arguments.trim_start().starts_with('{')).then(|| arguments.to_owned())
                })
                .unwrap_or_default();
            if command.is_empty() {
                "$".to_owned()
            } else {
                format!("$ {command}")
            }
        }
        "Read" | "Write" | "Edit" => {
            let verb = name.to_ascii_lowercase();
            streaming_json_text_field(arguments, "file_path")
                .filter(|path| !path.is_empty())
                .map_or_else(|| verb.clone(), |path| format!("{verb} {path}"))
        }
        _ if arguments.is_empty() => name.to_owned(),
        _ => format!("{name} {arguments}"),
    };
    truncate(&label, TOOL_LABEL_LIMIT)
}

fn ToolLine(tool: &UiTool) -> AnyView {
    let open = RwSignal::new(false);
    let output = tool
        .output
        .as_deref()
        .map(|text| truncate(text, TOOL_TEXT_LIMIT));
    let error = tool
        .error
        .as_deref()
        .map(|text| truncate(text, TOOL_TEXT_LIMIT));
    let expandable = output.is_some() || error.is_some();
    let status = match tool.status {
        UiToolStatus::Running => "running",
        UiToolStatus::Success => "success",
        UiToolStatus::Error => "error",
        UiToolStatus::Cancelled => "cancelled",
    };
    let status_text = match tool.status {
        UiToolStatus::Running => "…",
        UiToolStatus::Success => "ok",
        UiToolStatus::Error => "error",
        UiToolStatus::Cancelled => "cancelled",
    };
    let duration = tool_duration_ms(tool)
        .filter(|ms| *ms >= 1000)
        .map(format_duration);
    let label = tool_label(&tool.name, &tool.arguments);
    view! {
        <div class="tool" class:open=move || open.get()>
            <button class="tool-line" class:expandable=expandable on:click=move |_| {
                if expandable { open.update(|value| *value = !*value); }
            }>
                <span class="tool-label">{label}</span>
                <span class=format!("tool-status {status}")>{status_text}</span>
                {duration.map(|duration| view! { <span class="tool-dur">{duration}</span> })}
            </button>
            {move || (open.get() && expandable).then(|| view! {
                <div class="tool-body">
                    {output.clone().map(|text| view! { <pre>{text}</pre> })}
                    {error.clone().map(|text| view! { <pre class="err">{text}</pre> })}
                </div>
            })}
        </div>
    }
    .into_any()
}

#[component]
fn Composer(app: App) -> impl IntoView {
    let area: NodeRef<html::Textarea> = NodeRef::new();
    let status = Memo::new(move |_| {
        app.state_epoch.track();
        app.selected.get().and_then(|id| {
            app.store
                .with_value(|store| store.get(&id).map(|state| state.status))
        })
    });
    let busy = Memo::new(move |_| {
        matches!(
            status.get(),
            Some(UiAgentStatus::Streaming | UiAgentStatus::ToolCalling { .. })
        )
    });
    let status_label = move || {
        status.get().and_then(|status| {
            let (class, label) = match status {
                UiAgentStatus::Streaming => ("streaming", "thinking…"),
                UiAgentStatus::ToolCalling { .. } => ("tool_calling", "running tools…"),
                UiAgentStatus::UnfinishedTurn { .. } => ("unfinished", "stopped mid-turn"),
                UiAgentStatus::Error => ("error", "error"),
                _ => return None,
            };
            Some(view! { <span class=format!("chip status-{class}")>{label}</span> })
        })
    };
    let send = move || {
        let Some(element) = area.get_untracked() else {
            return;
        };
        let text = element.value();
        let Some(agent_id) = app.selected.get_untracked() else {
            return;
        };
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        app.send(ClientMessage::SendUserMessage {
            agent_id,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            delivery: MessageDelivery::NextRequest,
        });
        app.mutate_registry(|registry| registry.touch_agent(agent_id));
        element.set_value("");
        autosize(&element);
    };
    view! {
        <div class="composer"><div class="composer-card">
            <textarea rows="1" placeholder="Message the agent…" node_ref=area
                on:input=move |event| if let Some(element) = event.target().and_then(|target| target.dyn_into::<web_sys::HtmlTextAreaElement>().ok()) { autosize(&element); }
                on:keydown=move |event| if event.key() == "Enter" && !event.shift_key() && !coarse_pointer() { event.prevent_default(); send(); }>
            </textarea>
            <div class="composer-bar">
                <span class="composer-status">{status_label}{move || {
                    app.state_epoch.track();
                    app.selected.get().and_then(|id| app.store.with_value(|store| store.get(&id).and_then(|state| state.context_used)))
                        .map(|used| view! { <span class="chip">{format!("{used}%")}</span> })
                }}</span>
                {move || busy.get().then(|| view! {
                    <button class="stop" title="Stop" on:click=move |_| if let Some(agent_id) = app.selected.get_untracked() {
                        app.send(ClientMessage::CancelTurn { agent_id });
                    }>"■"</button>
                })}
                <button class="send" on:click=move |_| send() title="Send">"↑"</button>
            </div>
        </div></div>
    }
}

fn coarse_pointer() -> bool {
    web_sys::window()
        .and_then(|window| window.match_media("(pointer: coarse)").ok().flatten())
        .is_some_and(|query| query.matches())
}

fn autosize(element: &web_sys::HtmlTextAreaElement) {
    let style = web_sys::HtmlElement::style(element);
    let _ = style.set_property("height", "auto");
    let height = element.scroll_height().min(200);
    let _ = style.set_property("height", &format!("{height}px"));
}

fn role_label(role: AgentRole) -> &'static str {
    match role {
        AgentRole::PM | AgentRole::WorkflowPM { .. } => "pm",
        AgentRole::Iris => "iris",
        AgentRole::Advisor { .. } => "advisor",
        AgentRole::Engineer { intelligence, .. }
        | AgentRole::WorkflowEngineer { intelligence, .. } => match intelligence {
            EngineerIntelligence::Mini => "eng-mini",
            EngineerIntelligence::Low => "eng-low",
            EngineerIntelligence::Medium => "eng",
            EngineerIntelligence::High => "eng-high",
            EngineerIntelligence::Ultra => "eng-ultra",
            EngineerIntelligence::Alt => "eng-alt",
        },
    }
}

fn parse_role(role: &str) -> AgentRole {
    match role {
        "eng-mini" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        },
        "eng-low" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
        },
        "eng-high" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        },
        "eng-ultra" => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Ultra,
        },
        "pm" => AgentRole::PM,
        _ => AgentRole::default(),
    }
}

fn NewAgentPage(app: App) -> impl IntoView {
    let repo = RwSignal::new(
        app.projects
            .get_untracked()
            .first()
            .map(|project| project.path.to_string())
            .unwrap_or_default(),
    );
    let workstreams = app
        .registry
        .with_value(|registry| registry.workstreams().to_vec());
    let selected = app.selected.get_untracked();
    let workstream = RwSignal::new(
        selected
            .and_then(|selected| {
                app.registry
                    .with_value(|registry| registry.workstream_of(selected))
            })
            .or_else(|| {
                workstreams
                    .first()
                    .map(|workstream| workstream.workstream_id)
            }),
    );
    let role = RwSignal::new("eng".to_owned());
    let join = RwSignal::new(false);
    let sandbox = RwSignal::new(false);
    let revset = RwSignal::new("auto".to_owned());
    let area: NodeRef<html::Textarea> = NodeRef::new();
    let create = move || {
        let Some(element) = area.get_untracked() else {
            return;
        };
        let text = element.value();
        let repo = camino::Utf8PathBuf::from(repo.get_untracked());
        if text.trim().is_empty() || repo.as_str().is_empty() {
            app.show_toast("Pick a repository and write a first message.".to_owned());
            return;
        }
        let base = if revset.get_untracked().eq_ignore_ascii_case("auto") {
            AUTO_BASE_REVSET.to_owned()
        } else {
            revset.get_untracked()
        };
        let start = if join.get_untracked() {
            StartMode::Join(JoinTarget::User { repo })
        } else if sandbox.get_untracked() {
            StartMode::Sandbox { repo, revset: base }
        } else {
            StartMode::NewOn { repo, revset: base }
        };
        app.send(ClientMessage::NewAgent {
            workstream: workstream.get_untracked(),
            role: parse_role(&role.get_untracked()),
            start,
            content: Some(vec![ContentPart::Text {
                text: text.trim().to_owned(),
            }]),
        });
    };
    view! {
        <div class="draft-page">
            <div class="draft-head"><button class="back" on:click=move |_| app.show_new_agent.set(false)>"‹"</button>
                <div><h1>"New agent"</h1><p class="muted">"Choose how this agent should work, then give it a first task."</p></div>
            </div>
            <div class="draft-form">
                <section><h2>"Task"</h2><label>"First message"</label>
                    <textarea class="draft-task" rows="8" placeholder="What should it work on?" node_ref=area></textarea>
                </section>
                <div class="draft-grid">
                    <section><h2>"Location"</h2><label>"Repository"</label>
                        <select on:change=move |event| repo.set(event_target_value(&event))>
                            {move || app.projects.get().into_iter().map(|project| view! { <option value=project.path.to_string()>{project.name}</option> }).collect_view()}
                        </select>
                        <label>"Topic"</label>
                        <select on:change=move |event| workstream.set(event_target_value(&event).strip_prefix("tp-").and_then(|id| id.parse().ok()).map(WorkstreamId))>
                            {workstreams.clone().into_iter().map(|item| view! {
                                <option value=format!("tp-{}", item.workstream_id.0) selected=workstream.get_untracked() == Some(item.workstream_id)>{item.name}</option>
                            }).collect_view()}
                        </select>
                    </section>
                    <section><h2>"Role"</h2><label>"Responsibility and intelligence"</label>
                        <select on:change=move |event| role.set(event_target_value(&event))>
                            <option value="eng-mini">"Engineer · Mini"</option><option value="eng-low">"Engineer · Low"</option>
                            <option value="eng" selected>"Engineer · Standard"</option><option value="eng-high">"Engineer · High"</option>
                            <option value="eng-ultra">"Engineer · Ultra"</option><option value="pm">"Project manager"</option>
                        </select>
                        <p class="field-help">"Engineers implement changes. Project managers coordinate work across agents."</p>
                    </section>
                    <section><h2>"Workspace"</h2>
                        <label class="choice"><input type="radio" name="workspace" value="new" checked on:change=move |_| { join.set(false); sandbox.set(false); }/>
                            <span><strong>"New isolated workspace"</strong><small>"Recommended · keeps changes separate"</small></span></label>
                        <label class="choice"><input type="radio" name="workspace" value="join" on:change=move |_| { join.set(true); sandbox.set(false); }/>
                            <span><strong>"Work in my checkout"</strong><small>"Shares files and uncommitted changes"</small></span></label>
                        <label class="choice"><input type="radio" name="workspace" value="sandbox" on:change=move |_| { join.set(false); sandbox.set(true); }/>
                            <span><strong>"Restricted sandbox"</strong><small>"Fresh Git history · no network · restricted filesystem"</small></span></label>
                        <div class="revset" class:hidden=move || join.get()><label>"Base revision"</label>
                            <input value="auto" on:input=move |event| revset.set(event_target_value(&event))/>
                            <small>"Local main, then local master, then trunk"</small></div>
                    </section>
                </div>
                <div class="draft-actions"><button on:click=move |_| app.show_new_agent.set(false)>"Cancel"</button>
                    <button class="primary" on:click=move |_| create()>"Start agent"</button></div>
            </div>
        </div>
    }
}
