//! Leptos views: connect/enroll screens, agent rail, chat pane, composer.

// Component functions follow leptos' PascalCase convention.
#![allow(non_snake_case)]

use std::collections::{HashMap, HashSet};

use leptos::html;
use leptos::prelude::*;
use rho_webui_messages::{AgentSummary, Block, FromBrowser, Topic};
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

/// Quiet rows riding along with the active cohort before the tail folds,
/// mirroring the GUI registry's rail policy.
const EXTRA_ROWS: usize = 5;

/// Attention as an ordered level; higher needs the user more.
fn attention_level(attention: &str) -> u8 {
    match attention {
        "needs_input" => 3,
        "pending" => 2,
        "working" => 1,
        _ => 0,
    }
}

/// The GUI registry's active bucket: every colored row, plus enough of the
/// most recently active quiet rows to fill five slots.
fn active_bucket(mut candidates: Vec<(String, u8, u64)>) -> HashSet<String> {
    let colored = candidates
        .iter()
        .filter(|(_, level, _)| *level > 0)
        .count();
    let quiet_slots = 5usize.saturating_sub(colored);
    let mut top: HashSet<String> = candidates
        .iter()
        .filter(|(_, level, _)| *level > 0)
        .map(|(key, _, _)| key.clone())
        .collect();
    candidates.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    top.extend(
        candidates
            .into_iter()
            .filter(|(_, level, _)| *level == 0)
            .take(quiet_slots)
            .map(|(key, _, _)| key),
    );
    top
}

/// Retained rail order, mirroring the GUI registry: first-seen agents enter
/// above the existing order seeded by recency; already-placed agents keep
/// their relative position across refreshes.
fn update_retained(order: &mut Vec<String>, topics: &[Topic]) -> HashMap<String, usize> {
    let mut unseen: Vec<(u64, String)> = topics
        .iter()
        .flat_map(|topic| &topic.agents)
        .filter(|agent| !order.iter().any(|id| id == &agent.id))
        .map(|agent| (agent.updated_at, agent.id.clone()))
        .collect();
    unseen.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    order.splice(0..0, unseen.into_iter().map(|(_, id)| id));
    order
        .iter()
        .cloned()
        .enumerate()
        .map(|(rank, id)| (id, rank))
        .collect()
}

/// One rail row: a workstream with its visible agents in rail order.
/// Activating the row opens the root (best-ordered) agent.
#[derive(Clone, PartialEq)]
struct StreamRowData {
    topic_id: String,
    name: String,
    pinned: bool,
    has_pinned_agent: bool,
    /// Highest attention level over visible agents; the row's lamp.
    lamp: u8,
    agents: Vec<AgentSummary>,
}

/// The native dashboard's rail: workstream rows in retained order behind
/// pinned rows and the active cohort, with the quiet tail folded.
fn rail_rows(
    topics: &[Topic],
    selected: Option<&str>,
    ranks: &HashMap<String, usize>,
) -> (Vec<StreamRowData>, Vec<StreamRowData>) {
    let mut rows = Vec::new();
    let mut best_ranks = HashMap::new();
    let mut max_updated = HashMap::new();
    for topic in topics {
        let visible: Vec<&AgentSummary> = topic
            .agents
            .iter()
            .filter(|agent| !agent.hidden)
            .collect();
        if visible.is_empty() {
            continue;
        }
        let topic_bucket = active_bucket(
            visible
                .iter()
                .filter(|agent| !agent.pinned)
                .map(|agent| {
                    (
                        agent.id.clone(),
                        attention_level(&agent.attention),
                        agent.updated_at,
                    )
                })
                .collect(),
        );
        let mut agents: Vec<AgentSummary> = visible.into_iter().cloned().collect();
        agents.sort_by_key(|agent| {
            (
                !agent.pinned,
                !topic_bucket.contains(&agent.id),
                ranks.get(&agent.id).copied().unwrap_or(usize::MAX),
            )
        });
        best_ranks.insert(
            topic.id.clone(),
            agents
                .first()
                .and_then(|agent| ranks.get(&agent.id))
                .copied()
                .unwrap_or(usize::MAX),
        );
        max_updated.insert(
            topic.id.clone(),
            agents.iter().map(|agent| agent.updated_at).max().unwrap_or(0),
        );
        rows.push(StreamRowData {
            topic_id: topic.id.clone(),
            name: topic.name.clone(),
            pinned: topic.pinned,
            has_pinned_agent: agents.iter().any(|agent| agent.pinned),
            lamp: agents
                .iter()
                .map(|agent| attention_level(&agent.attention))
                .max()
                .unwrap_or(0),
            agents,
        });
    }
    let bucket = active_bucket(
        rows.iter()
            .filter(|row| !row.pinned && !row.has_pinned_agent)
            .map(|row| {
                (
                    row.topic_id.clone(),
                    row.lamp,
                    max_updated.get(&row.topic_id).copied().unwrap_or(0),
                )
            })
            .collect(),
    );
    rows.sort_by_key(|row| {
        (
            !row.pinned,
            !row.has_pinned_agent,
            !bucket.contains(&row.topic_id),
            best_ranks.get(&row.topic_id).copied().unwrap_or(usize::MAX),
        )
    });
    let mut listed = Vec::new();
    let mut folded = Vec::new();
    let mut extra = 0;
    for row in rows {
        let keep = row.pinned
            || row.has_pinned_agent
            || bucket.contains(&row.topic_id)
            || selected
                .is_some_and(|selected| row.agents.iter().any(|agent| agent.id == selected));
        if keep {
            listed.push(row);
        } else if extra < EXTRA_ROWS {
            extra += 1;
            listed.push(row);
        } else {
            folded.push(row);
        }
    }
    (listed, folded)
}

fn Main(app: App) -> impl IntoView {
    // Surface the attention count where a phone glance lands: the tab title.
    Effect::new(move |_| {
        let count = app.topics.with(|topics| {
            topics
                .iter()
                .flat_map(|topic| &topic.agents)
                .filter(|agent| !agent.hidden && agent.attention == "needs_input")
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
    // Session-retained rail order and per-row disclosure, surviving refreshes.
    let retained: StoredValue<Vec<String>> = StoredValue::new(Vec::new());
    let open_streams: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());
    let folded_open = RwSignal::new(false);
    let rows = move || {
        let topics = app.topics.get();
        let selected = app.selected.get();
        let ranks = retained
            .try_update_value(|order| update_retained(order, &topics))
            .unwrap_or_default();
        rail_rows(&topics, selected.as_deref(), &ranks)
    };
    view! {
        <div class="rail">
            <div class="rail-head">
                <div class="brand"><span class="logo small">"ρ"</span>"rho"</div>
                <button
                    class="new-agent"
                    title="New agent"
                    on:click=move |_| {
                        app.show_new_agent.set(true);
                        app.chat_open.set(true);
                    }
                >
                    <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4">
                        <path d="M11.5 2.5l2 2L6 12l-2.7.7.7-2.7 7.5-7.5z"/>
                    </svg>
                    <span>"New"</span>
                </button>
            </div>
            <div class="topics">
                {move || {
                    let (listed, folded) = rows();
                    let folded_count = folded.len();
                    view! {
                        {listed.into_iter().map(|row| StreamRow(app, row, open_streams)).collect_view()}
                        {(folded_count > 0).then(|| view! {
                            {move || folded_open.get().then({
                                let folded = folded.clone();
                                move || folded.into_iter().map(|row| StreamRow(app, row, open_streams)).collect_view()
                            })}
                            <button class="fold-row" on:click=move |_| folded_open.update(|value| *value = !*value)>
                                <span>{move || if folded_open.get() { "⌃" } else { "⌄" }}</span>
                                <span>{move || if folded_open.get() { "Show less".to_owned() } else { format!("{folded_count} more") }}</span>
                            </button>
                        })}
                    }
                }}
            </div>
            <div class="rail-foot" title="connected">
                <span class="dot ok"></span>
                <span class="foot-label">{daemon_short()}</span>
            </div>
        </div>
        <div class="chat">
            {move || if app.show_new_agent.get() {
                NewAgentPage(app).into_any()
            } else {
                match app.selected.get() {
                    Some(agent_id) => ChatPane(app, agent_id).into_any(),
                    None => view! {
                    <div class="placeholder">
                        <div class="logo big">"ρ"</div>
                        <p class="muted">"Pick an agent, or start a new one."</p>
                    </div>
                    }.into_any(),
                }
            }}
        </div>
    }
}

/// The attention lamp hanging off a row's right end, GUI glyphs and all.
fn lamp_view(level: u8) -> Option<AnyView> {
    let (glyph, class) = match level {
        3 => ("◆", "needs_input"),
        2 => ("●", "pending"),
        1 => ("…", "working"),
        _ => return None,
    };
    Some(view! { <span class=format!("lamp {class}")>{glyph}</span> }.into_any())
}

/// One workstream row: activation opens the root agent; a disclosure
/// chevron reveals the member agents of multi-agent workstreams.
fn StreamRow(app: App, row: StreamRowData, open_streams: RwSignal<HashSet<String>>) -> impl IntoView {
    let root = row.agents.first().map(|agent| agent.id.clone());
    let agent_ids: Vec<String> = row.agents.iter().map(|agent| agent.id.clone()).collect();
    let active_ids = agent_ids.clone();
    let multi = row.agents.len() > 1;
    let topic_id = row.topic_id.clone();
    let toggle_id = row.topic_id.clone();
    let title = if row.name.trim().is_empty() {
        "Untitled workstream".to_owned()
    } else {
        row.name.clone()
    };
    let agents = row.agents.clone();
    view! {
        <div class="stream">
            <div
                class="stream-row"
                role="button"
                class:active=move || {
                    app.selected.get().is_some_and(|selected| active_ids.contains(&selected))
                }
                on:click=move |_| {
                    if let Some(root) = root.clone() {
                        app.select(root);
                    }
                }
            >
                {row.pinned.then(|| view! { <span class="pin-mark">"◆"</span> })}
                <span class="stream-title">{title}</span>
                {lamp_view(row.lamp)}
                {multi.then(|| view! {
                    <button
                        class="disclose"
                        on:click=move |event| {
                            event.stop_propagation();
                            open_streams.update(|open| {
                                if !open.remove(&toggle_id) {
                                    open.insert(toggle_id.clone());
                                }
                            });
                        }
                    >
                        {
                            let chevron_id = topic_id.clone();
                            move || if open_streams.with(|open| open.contains(&chevron_id)) { "⌄" } else { "›" }
                        }
                    </button>
                })}
            </div>
            {
                let members_id = row.topic_id.clone();
                move || {
                    (multi && open_streams.with(|open| open.contains(&members_id))).then(|| {
                        agents
                            .clone()
                            .into_iter()
                            .map(|agent| AgentLine(app, agent))
                            .collect_view()
                    })
                }
            }
        </div>
    }
}

/// A member agent line under a disclosed workstream row.
fn AgentLine(app: App, agent: AgentSummary) -> impl IntoView {
    let id = agent.id.clone();
    let selected_id = agent.id.clone();
    let level = attention_level(&agent.attention);
    view! {
        <button
            class="agent-line"
            class:active=move || app.selected.get().as_deref() == Some(selected_id.as_str())
            on:click=move |_| app.select(id.clone())
        >
            {agent.pinned.then(|| view! { <span class="pin-mark">"◆"</span> })}
            <span class="agent-line-name">{agent.name}</span>
            <span class="agent-line-role">{agent.role}</span>
            {lamp_view(level)}
        </button>
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
    view! {
        <div class="chat-head">
            <button class="back" on:click=move |_| app.chat_open.set(false)>"‹"</button>
            <span class="chat-name">
                {move || summary.get().map(|agent| agent.name).unwrap_or_else(|| agent_id.clone())}
            </span>
            {move || summary.get().map(|agent| view! {
                <span class="chip mode">{agent.role}</span>
            })}
        </div>
        <Transcript app=app />
        <Composer app=app />
    }
}

#[component]
fn Transcript(app: App) -> impl IntoView {
    let scroller: NodeRef<html::Div> = NodeRef::new();
    // Follow the newest message only while the reader is at the bottom;
    // scrolling up to reread must never be yanked back down by streaming.
    let pinned = RwSignal::new(true);
    Effect::new(move |_| {
        app.state.track();
        if !pinned.get_untracked() {
            return;
        }
        if let Some(element) = scroller.get_untracked() {
            request_animation_frame(move || {
                element.set_scroll_top(element.scroll_height());
            });
        }
    });
    let jump_to_latest = move |_| {
        if let Some(element) = scroller.get_untracked() {
            element.set_scroll_top(element.scroll_height());
        }
        pinned.set(true);
    };
    view! {
        <div
            class="transcript"
            node_ref=scroller
            on:scroll=move |_| {
                if let Some(element) = scroller.get_untracked() {
                    let bottom_gap = element.scroll_height()
                        - element.scroll_top()
                        - element.client_height();
                    pinned.set(bottom_gap < 60);
                }
            }
        >
            <div class="column">
                {move || match app.state.get() {
                    None => view! { <p class="muted loading">"Loading transcript…"</p> }.into_any(),
                    Some(state) => {
                        let busy = matches!(state.status.as_str(), "streaming" | "tool_calling");
                        Blocks(&state.blocks, busy).into_any()
                    }
                }}
            </div>
            {move || (!pinned.get()).then(|| view! {
                <button class="jump-latest" on:click=jump_to_latest>"↓ latest"</button>
            })}
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
    let status = Memo::new(move |_| {
        app.state
            .with(|state| state.as_ref().map(|state| state.status.clone()))
    });
    let busy =
        Memo::new(move |_| matches!(status.get().as_deref(), Some("streaming" | "tool_calling")));
    let status_label = move || {
        status.get().and_then(|status| {
            let label = match status.as_str() {
                "streaming" => "thinking…",
                "tool_calling" => "running tools…",
                "unfinished" => "stopped mid-turn",
                "error" => "error",
                _ => return None,
            };
            Some(view! { <span class=format!("chip status-{status}")>{label}</span> })
        })
    };
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
                        // Touch keyboards have no shift-enter; there the
                        // enter key inserts a newline and the send button
                        // sends.
                        if event.key() == "Enter" && !event.shift_key() && !coarse_pointer() {
                            event.prevent_default();
                            send();
                        }
                    }
                ></textarea>
                <div class="composer-bar">
                    <span class="composer-status">
                        {status_label}
                        {move || app.state.with(|state| {
                            state.as_ref().and_then(|state| state.context_used).map(|used| {
                                view! { <span class="chip">{format!("{used}%")}</span> }
                            })
                        })}
                    </span>
                    {move || busy.get().then(|| {
                        view! {
                            <button class="stop" title="Stop" on:click=move |_| {
                                if let Some(agent_id) = app.selected.get_untracked() {
                                    app.send(FromBrowser::Cancel { agent_id });
                                }
                            }>"■"</button>
                        }
                    })}
                    <button class="send" on:click=move |_| send() title="Send">"↑"</button>
                </div>
            </div>
        </div>
    }
}

/// Primary input is a touch screen (phone or tablet).
fn coarse_pointer() -> bool {
    web_sys::window()
        .and_then(|window| window.match_media("(pointer: coarse)").ok().flatten())
        .is_some_and(|query| query.matches())
}

fn autosize(element: &web_sys::HtmlTextAreaElement) {
    // Fully qualified: leptos' ElementExt also has a `style` method.
    let style = web_sys::HtmlElement::style(element);
    let _ = style.set_property("height", "auto");
    let height = element.scroll_height().min(200);
    let _ = style.set_property("height", &format!("{height}px"));
}

fn NewAgentPage(app: App) -> impl IntoView {
    let repo = RwSignal::new(
        app.projects
            .get_untracked()
            .first()
            .map(|project| project.path.clone())
            .unwrap_or_default(),
    );
    let topics = app.topics.get_untracked();
    let selected = app.selected.get_untracked();
    let topic_id = RwSignal::new(
        selected
            .as_ref()
            .and_then(|selected| {
                topics
                    .iter()
                    .find(|topic| topic.agents.iter().any(|agent| &agent.id == selected))
            })
            .or_else(|| topics.first())
            .map(|topic| topic.id.clone())
            .unwrap_or_default(),
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
        let text = text.trim();
        let repo = repo.get_untracked();
        if text.is_empty() || repo.is_empty() {
            app.show_toast("Pick a repository and write a first message.".to_owned());
            return;
        }
        let revset = revset.get_untracked();
        app.send(FromBrowser::NewAgent {
            topic_id: topic_id.get_untracked(),
            repo,
            role: role.get_untracked(),
            join: join.get_untracked(),
            sandbox: sandbox.get_untracked(),
            revset: if revset.eq_ignore_ascii_case("auto") {
                AUTO_BASE_REVSET.to_owned()
            } else {
                revset
            },
            text: text.to_owned(),
        });
    };
    view! {
        <div class="draft-page">
            <div class="draft-head">
                <button class="back" on:click=move |_| app.show_new_agent.set(false)>"‹"</button>
                <div>
                    <h1>"New agent"</h1>
                    <p class="muted">"Choose how this agent should work, then give it a first task."</p>
                </div>
            </div>
            <div class="draft-form">
                <section>
                    <h2>"Task"</h2>
                    <label>"First message"</label>
                    <textarea class="draft-task" rows="8" placeholder="What should it work on?" node_ref=area></textarea>
                </section>
                <div class="draft-grid">
                    <section>
                        <h2>"Location"</h2>
                        <label>"Repository"</label>
                <select on:change=move |event| repo.set(event_target_value(&event))>
                    {move || app.projects.get().into_iter().map(|project| {
                        view! { <option value=project.path.clone()>{project.name}</option> }
                    }).collect_view()}
                </select>
                        <label>"Topic"</label>
                        <select prop:value=move || topic_id.get()
                            on:change=move |event| topic_id.set(event_target_value(&event))>
                            {move || app.topics.get().into_iter().map(|topic| {
                                view! { <option value=topic.id>{topic.name}</option> }
                            }).collect_view()}
                        </select>
                    </section>
                    <section>
                        <h2>"Role"</h2>
                        <label>"Responsibility and intelligence"</label>
                        <select on:change=move |event| role.set(event_target_value(&event))>
                            <option value="eng-mini">"Engineer · Mini"</option>
                            <option value="eng-low">"Engineer · Low"</option>
                            <option value="eng" selected>"Engineer · Standard"</option>
                            <option value="eng-high">"Engineer · High"</option>
                            <option value="eng-ultra">"Engineer · Ultra"</option>
                            <option value="pm">"Project manager"</option>
                        </select>
                        <p class="field-help">"Engineers implement changes. Project managers coordinate work across agents."</p>
                    </section>
                    <section>
                        <h2>"Workspace"</h2>
                        <label class="choice">
                            <input type="radio" name="workspace" value="new" checked
                                on:change=move |_| { join.set(false); sandbox.set(false); } />
                            <span><strong>"New isolated workspace"</strong><small>"Recommended · keeps changes separate"</small></span>
                        </label>
                        <label class="choice">
                            <input type="radio" name="workspace" value="join"
                                on:change=move |_| { join.set(true); sandbox.set(false); } />
                            <span><strong>"Work in my checkout"</strong><small>"Shares files and uncommitted changes"</small></span>
                        </label>
                        <label class="choice">
                            <input type="radio" name="workspace" value="sandbox"
                                on:change=move |_| { join.set(false); sandbox.set(true); } />
                            <span><strong>"Restricted sandbox"</strong><small>"Fresh Git history · no network · restricted filesystem"</small></span>
                        </label>
                        <div class="revset" class:hidden=move || join.get()>
                            <label>"Base revision"</label>
                            <input value="auto"
                                on:input=move |event| revset.set(event_target_value(&event)) />
                            <small>"Local main, then local master, then trunk"</small>
                        </div>
                    </section>
                </div>
                <div class="draft-actions">
                    <button on:click=move |_| app.show_new_agent.set(false)>"Cancel"</button>
                    <button class="primary" on:click=move |_| create()>"Start agent"</button>
                </div>
            </div>
        </div>
    }
}
