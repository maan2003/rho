//! Leptos views: connect/enroll screens, agent rail, chat pane, composer.

// Component functions follow leptos' PascalCase convention.
#![allow(non_snake_case)]

use leptos::html;
use leptos::prelude::*;
use rho_webui_messages::{AgentSummary, Block, FromBrowser};
use wasm_bindgen::JsCast as _;

use crate::{App, Phase, conn, md};

pub fn Root(app: App) -> impl IntoView {
    view! {
        <div class="shell" class=("chat-open", move || app.chat_open.get())>
            {move || match app.phase.get() {
                Phase::NeedDaemon => ConnectScreen(app).into_any(),
                Phase::Connecting => StatusScreen("Connecting to your daemon…", None).into_any(),
                Phase::Enroll(code) => EnrollScreen(code).into_any(),
                Phase::Failed(message) => StatusScreen("Connection failed", Some(message)).into_any(),
                Phase::Online => Main(app).into_any(),
            }}
            {move || app.toast.get().map(|message| view! { <div class="toast">{message}</div> })}
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
                <p class="muted spin-row"><span class="spinner"></span>"Waiting for approval… the code expires after a minute; reload to get a new one."</p>
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

fn Main(app: App) -> impl IntoView {
    view! {
        <div class="rail">
            <div class="rail-head">
                <div class="brand"><span class="logo small">"ρ"</span>"rho"</div>
                <button
                    class="new-agent"
                    title="New agent"
                    on:click=move |_| app.show_new_agent.set(true)
                >
                    <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4">
                        <path d="M11.5 2.5l2 2L6 12l-2.7.7.7-2.7 7.5-7.5z"/>
                    </svg>
                </button>
            </div>
            <div class="topics">
                {move || app.topics.get().into_iter().map(|topic| {
                    let agents: Vec<AgentSummary> = topic
                        .agents
                        .into_iter()
                        .filter(|agent| !agent.hidden)
                        .collect();
                    if agents.is_empty() {
                        return ().into_any();
                    }
                    view! {
                        <div class="topic">
                            <div class="topic-name">{topic.name}</div>
                            {agents.into_iter().map(|agent| AgentRow(app, agent)).collect_view()}
                        </div>
                    }
                    .into_any()
                }).collect_view()}
            </div>
            <div class="rail-foot" title="connected">
                <span class="dot ok"></span>
                <span class="foot-label">{daemon_short()}</span>
            </div>
        </div>
        <div class="chat">
            {move || match app.selected.get() {
                Some(agent_id) => ChatPane(app, agent_id).into_any(),
                None => view! {
                    <div class="placeholder">
                        <div class="logo big">"ρ"</div>
                        <p class="muted">"Pick an agent, or start a new one."</p>
                    </div>
                }
                .into_any(),
            }}
        </div>
        {move || app.show_new_agent.get().then(|| NewAgentDialog(app))}
    }
}

/// Shortened daemon endpoint id for the rail footer.
fn daemon_short() -> String {
    match conn::daemon_id() {
        Some(id) if id.len() > 12 => format!("{}…", &id[..12]),
        Some(id) => id,
        None => "connected".to_owned(),
    }
}

fn AgentRow(app: App, agent: AgentSummary) -> impl IntoView {
    let id = agent.id.clone();
    let selected_id = agent.id.clone();
    let attention = agent.attention.clone();
    view! {
        <button
            class="agent-row"
            class:active=move || app.selected.get().as_deref() == Some(selected_id.as_str())
            on:click=move |_| app.select(id.clone())
        >
            <svg class="row-icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3">
                <path d="M2.5 3.5h11v8h-6l-3 2.5v-2.5h-2v-8z"/>
            </svg>
            <span class="agent-meta">
                <span class="agent-name">{agent.name}</span>
                <span class="agent-mode">{agent.mode}</span>
            </span>
            <span class=format!("attn {attention}")></span>
        </button>
    }
}

fn ChatPane(app: App, agent_id: String) -> impl IntoView {
    let summary = Memo::new({
        let agent_id = agent_id.clone();
        move |_| {
            app.topics.with(|topics| {
                topics
                    .iter()
                    .flat_map(|topic| &topic.agents)
                    .find(|agent| agent.id == agent_id)
                    .cloned()
            })
        }
    });
    let status = Memo::new(move |_| {
        app.state
            .with(|state| state.as_ref().map(|state| state.status.clone()))
    });
    let busy =
        Memo::new(move |_| matches!(status.get().as_deref(), Some("streaming" | "tool_calling")));
    let cancel_id = agent_id.clone();
    let header_status = move || {
        status.get().map(|status| {
            let label = match status.as_str() {
                "idle" => "idle",
                "streaming" => "thinking",
                "tool_calling" => "running tools",
                "unfinished" => "stopped mid-turn",
                "error" => "error",
                _ => "…",
            };
            view! { <span class=format!("chip status-{status}")>{label}</span> }
        })
    };
    view! {
        <div class="chat-head">
            <button class="back" on:click=move |_| app.chat_open.set(false)>"‹"</button>
            <div class="chat-title">
                <span class="chat-name">
                    {move || summary.get().map(|agent| agent.name).unwrap_or_else(|| agent_id.clone())}
                </span>
                <span class="chat-chips">
                    {header_status}
                    {move || app.state.with(|state| {
                        state.as_ref().and_then(|state| state.context_used).map(|used| {
                            view! { <span class="chip">{format!("{used}% context")}</span> }
                        })
                    })}
                </span>
            </div>
            {move || summary.get().map(|agent| view! {
                <span class="chip mode">{agent.mode}</span>
            })}
            {move || busy.get().then(|| {
                let cancel_id = cancel_id.clone();
                view! {
                    <button class="stop" on:click=move |_| {
                        app.send(FromBrowser::Cancel { agent_id: cancel_id.clone() });
                    }>"Stop"</button>
                }
            })}
        </div>
        <Transcript app=app />
        <Composer app=app />
    }
}

#[component]
fn Transcript(app: App) -> impl IntoView {
    let scroller: NodeRef<html::Div> = NodeRef::new();
    // Follow the newest message whenever the transcript grows.
    Effect::new(move |_| {
        app.state.track();
        if let Some(element) = scroller.get_untracked() {
            request_animation_frame(move || {
                element.set_scroll_top(element.scroll_height());
            });
        }
    });
    view! {
        <div class="transcript" node_ref=scroller>
            <div class="column">
                {move || match app.state.get() {
                    None => view! { <p class="muted loading">"Loading transcript…"</p> }.into_any(),
                    Some(state) => {
                        let busy = matches!(state.status.as_str(), "streaming" | "tool_calling");
                        Blocks(&state.blocks, busy).into_any()
                    }
                }}
            </div>
        </div>
    }
}

fn Blocks(blocks: &[Block], busy: bool) -> impl IntoView {
    let mut views = Vec::new();
    let mut index = 0;
    while index < blocks.len() {
        let run_end = blocks[index..]
            .iter()
            .position(|block| !matches!(block, Block::Tool { .. }))
            .map(|offset| index + offset)
            .unwrap_or(blocks.len());
        if run_end == index {
            views.push(BlockView(&blocks[index]));
            index += 1;
            continue;
        }
        // Finished runs of tool lines collapse behind a "Worked for …" fold;
        // the trailing run stays open while the agent is busy so live
        // activity is visible.
        let run = &blocks[index..run_end];
        let tail_open = busy && run_end == blocks.len();
        if run.len() > 1 && !tail_open {
            views.push(ToolFold(run.to_vec()));
        } else {
            views.extend(run.iter().map(BlockView));
        }
        index = run_end;
    }
    views.collect_view()
}

fn ToolFold(run: Vec<Block>) -> AnyView {
    let open = RwSignal::new(false);
    let total_ms: u64 = run
        .iter()
        .filter_map(|block| match block {
            Block::Tool { duration_ms, .. } => *duration_ms,
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
            <button class="fold-head" on:click=move |_| open.update(|open| *open = !*open)>
                <span class="fold-label">{label}</span>
                <span class="chev">{move || if open.get() { "⌄" } else { "›" }}</span>
            </button>
            {move || open.get().then(|| run.iter().map(BlockView).collect_view())}
        </div>
    }
    .into_any()
}

/// `3s` / `1m20s`, matching the GUI transcript.
fn format_duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

fn BlockView(block: &Block) -> AnyView {
    match block {
        Block::User { text } => view! {
            <div class="row user"><div class="bubble user">{text.clone()}</div></div>
        }
        .into_any(),
        Block::Assistant { text, final_answer } => {
            let class = if *final_answer {
                "block assistant final"
            } else {
                "block assistant"
            };
            view! { <div class=class inner_html=md::render(text)></div> }.into_any()
        }
        Block::Tool {
            label,
            status,
            duration_ms,
            output,
            error,
        } => ToolLine(
            label,
            status,
            *duration_ms,
            output.as_deref(),
            error.as_deref(),
        ),
        Block::Notice { text } => {
            view! { <div class="block notice">{text.clone()}</div> }.into_any()
        }
        Block::Queued { text } => view! {
            <div class="row user"><div class="bubble user queued">{text.clone()}</div></div>
        }
        .into_any(),
        Block::AgentMessage { sender, text } => view! {
            <div class="block agent-msg">
                <div class="sender">{format!("from {sender}")}</div>
                <div inner_html=md::render(text)></div>
            </div>
        }
        .into_any(),
    }
}

/// One quiet line per tool, GUI-style: `label status [duration]`. Clicking
/// the line reveals output/error when the tool produced any.
fn ToolLine(
    label: &str,
    status: &str,
    duration_ms: Option<u64>,
    output: Option<&str>,
    error: Option<&str>,
) -> AnyView {
    let open = RwSignal::new(false);
    let expandable = output.is_some() || error.is_some();
    let status_text = match status {
        "running" => "…",
        "success" => "ok",
        other => other,
    }
    .to_owned();
    let label = label.to_owned();
    let status = status.to_owned();
    let duration = duration_ms.filter(|&ms| ms >= 1000).map(format_duration);
    let output = output.map(str::to_owned);
    let error = error.map(str::to_owned);
    view! {
        <div class="tool" class:open=move || open.get()>
            <button
                class="tool-line"
                class:expandable=expandable
                on:click=move |_| {
                    if expandable {
                        open.update(|open| *open = !*open);
                    }
                }
            >
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
    let send = move || {
        let Some(element) = area.get_untracked() else {
            return;
        };
        let text = element.value();
        let text = text.trim();
        let Some(agent_id) = app.selected.get_untracked() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        app.send(FromBrowser::Send {
            agent_id,
            text: text.to_owned(),
        });
        element.set_value("");
        autosize(&element);
    };
    view! {
        <div class="composer">
            <div class="composer-card">
                <textarea
                    rows="1"
                    placeholder="Message the agent…"
                    node_ref=area
                    on:input=move |event| {
                        if let Some(element) = event
                            .target()
                            .and_then(|target| target.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
                        {
                            autosize(&element);
                        }
                    }
                    on:keydown=move |event| {
                        if event.key() == "Enter" && !event.shift_key() {
                            event.prevent_default();
                            send();
                        }
                    }
                ></textarea>
                <div class="composer-bar">
                    <button class="send" on:click=move |_| send() title="Send">"↑"</button>
                </div>
            </div>
        </div>
    }
}

fn autosize(element: &web_sys::HtmlTextAreaElement) {
    // Fully qualified: leptos' ElementExt also has a `style` method.
    let style = web_sys::HtmlElement::style(element);
    let _ = style.set_property("height", "auto");
    let height = element.scroll_height().min(200);
    let _ = style.set_property("height", &format!("{height}px"));
}

fn NewAgentDialog(app: App) -> impl IntoView {
    let repo = RwSignal::new(
        app.workdirs
            .get_untracked()
            .first()
            .map(|workdir| workdir.path.clone())
            .unwrap_or_default(),
    );
    let area: NodeRef<html::Textarea> = NodeRef::new();
    let create = move || {
        let Some(element) = area.get_untracked() else {
            return;
        };
        let text = element.value();
        let text = text.trim();
        let repo = repo.get_untracked();
        if text.is_empty() || repo.is_empty() {
            app.show_toast("Pick a repository and write a first message.".to_owned());
            return;
        }
        app.send(FromBrowser::NewAgent {
            repo,
            text: text.to_owned(),
        });
        app.show_new_agent.set(false);
    };
    view! {
        <div class="overlay" on:click=move |_| app.show_new_agent.set(false)>
            <div class="dialog" on:click=|event| event.stop_propagation()>
                <h2>"New agent"</h2>
                <label>"Repository"</label>
                <select on:change=move |event| repo.set(event_target_value(&event))>
                    {move || app.workdirs.get().into_iter().map(|workdir| {
                        view! { <option value=workdir.path.clone()>{workdir.name}</option> }
                    }).collect_view()}
                </select>
                <label>"First message"</label>
                <textarea rows="4" placeholder="What should it work on?" node_ref=area></textarea>
                <div class="dialog-actions">
                    <button on:click=move |_| app.show_new_agent.set(false)>"Cancel"</button>
                    <button class="primary" on:click=move |_| create()>"Start"</button>
                </div>
            </div>
        </div>
    }
}
