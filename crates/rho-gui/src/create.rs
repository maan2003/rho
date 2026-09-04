//! Creating things: one verb, one place to extend.
//!
//! `n` opens the `new` transient anywhere: `a` agent, `p` page, `n` note.
//! Every flow starts with the area picker, so a new thing always has a
//! parent the user chose and there is no unfiled pile to guard with a
//! dealer curve. The picker's first row is the node in context — the row
//! under the cursor, or the node behind the surface in view — so Enter
//! alone files the new thing where the reader already is; `root` is the
//! second row, and typing narrows to any node in the tree.

use std::rc::Rc;

use gpui::{App, Context, Window};

use crate::find::rank;
use crate::minibuffer::Candidate;
use crate::registry::HostId;
use crate::style::StyleClass;
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NewKind {
    Agent,
    Page,
    Note,
}

impl NewKind {
    fn prompt(self) -> &'static str {
        match self {
            Self::Agent => "new agent in:",
            Self::Page => "new page in:",
            Self::Note => "new note in:",
        }
    }
}

/// The label of the row that files at the root. A path, so it reads like
/// every other row and cannot collide with a node's own path.
const ROOT_ROW: &str = "root";
/// Ranking keys that put the context row first and `root` second among
/// equal matches, which is what an empty query makes everything.
const CONTEXT_RECENCY: i64 = i64::MAX;
const ROOT_RECENCY: i64 = i64::MAX - 1;

/// One place a new node can be filed.
struct Area {
    path: String,
    kind: &'static str,
    /// `None` files at the root.
    target: Option<(HostId, rho_desk::cells::Id)>,
    recency: i64,
}

impl Workspace {
    /// The node the reader is looking at: the row under the cursor on
    /// Home or the desk, else the node behind the surface in view.
    pub(crate) fn context_area(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(HostId, rho_desk::cells::Id)> {
        // Home is a window onto the same nodes, so its cursor names an
        // area exactly as the desk's does.
        if self.active_pane().surface.key == crate::pane::SurfaceKey::Home
            && let Some(view) = self.home_view()
        {
            match view.update(cx, |view, cx| view.cursor_target(cx)) {
                crate::home::HomeTarget::Card(card) => return Some((card.host, card.node_id)),
                crate::home::HomeTarget::Agent(agent_id) => {
                    if let Some(card) = self.dashboard.agent_card_id(agent_id) {
                        return Some((card.host, card.node_id));
                    }
                }
                crate::home::HomeTarget::None => {}
            }
        }
        if let Some(node) = self.dashboard.tree_node_at_cursor(cx) {
            return Some(node);
        }
        let card = match &self.active_pane().surface.key {
            crate::pane::SurfaceKey::DeskNode { host, node_id } => {
                Some(crate::dashboard::DealCardId {
                    host: *host,
                    node_id: node_id.clone(),
                })
            }
            crate::pane::SurfaceKey::Transcript(agent_id)
            | crate::pane::SurfaceKey::Shell(agent_id)
            | crate::pane::SurfaceKey::Diff { agent_id }
            | crate::pane::SurfaceKey::File { agent_id, .. }
            | crate::pane::SurfaceKey::Terminal { agent_id, .. } => {
                self.dashboard.agent_card_id(*agent_id)
            }
            crate::pane::SurfaceKey::Browser(page) => self.dashboard.page_card_id(*page),
            crate::pane::SurfaceKey::SlackConversation(rho_slack::session::Source::Thread(key)) => {
                self.dashboard
                    .thread_card_id(&crate::slack::store_unit_of(key))
            }
            _ => None,
        }?;
        Some((card.host, card.node_id))
    }

    fn areas(&self, context: Option<(HostId, rho_desk::cells::Id)>, cx: &App) -> Vec<Area> {
        let mut areas = vec![Area {
            path: ROOT_ROW.to_owned(),
            kind: "root",
            target: None,
            recency: ROOT_RECENCY,
        }];
        let threads = self.slack_thread_facts(cx);
        for (path, kind, host, node_id) in
            self.dashboard.area_candidates(&self.registry, &threads, cx)
        {
            let recency = if context == Some((host, node_id.clone())) {
                CONTEXT_RECENCY
            } else {
                0
            };
            areas.push(Area {
                path,
                kind,
                target: Some((host, node_id)),
                recency,
            });
        }
        areas
    }

    /// `n a`, `n p`, `n n`: ask for the area, then make the thing.
    pub(crate) fn begin_new(&mut self, kind: NewKind, window: &mut Window, cx: &mut Context<Self>) {
        let context = self.context_area(cx);
        let submit_context = context.clone();
        let complete = Rc::new(move |workspace: &Workspace, input: &str, cx: &App| {
            let areas = workspace.areas(context.clone(), cx);
            let paths = areas
                .iter()
                .map(|area| (area.path.clone(), area.recency))
                .collect::<Vec<_>>();
            rank(&paths, input)
                .into_iter()
                .filter_map(|index| areas.get(index))
                .take(AREA_LIMIT)
                .map(|area| Candidate {
                    value: area.path.clone(),
                    description: area.kind.to_owned(),
                })
                .collect()
        });
        let on_submit = Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                workspace.new_in_area(kind, submit_context.clone(), &input, window, cx);
            },
        );
        self.open_prompt(kind.prompt(), complete, on_submit, window, cx);
        if let Some(minibuffer) = &mut self.minibuffer {
            // A path has spaces in it, so completion replaces the whole
            // input rather than the last word.
            minibuffer.set_complete_whole_input();
        }
    }

    /// Resolves what the reader typed to an area, then creates in it. An
    /// empty submission is Enter on the first row: the context.
    fn new_in_area(
        &mut self,
        kind: NewKind,
        context: Option<(HostId, rho_desk::cells::Id)>,
        input: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = input.trim();
        let areas = self.areas(context, cx);
        let paths = areas
            .iter()
            .map(|area| (area.path.clone(), area.recency))
            .collect::<Vec<_>>();
        let chosen = areas
            .iter()
            .position(|area| area.path == input)
            .or_else(|| rank(&paths, input).first().copied());
        let Some(index) = chosen else {
            self.notice_on(
                None,
                &format!("nothing matching `{input}`"),
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let area = areas[index].target.clone();
        match kind {
            NewKind::Agent => self.new_agent_in_area(area, window, cx),
            NewKind::Page => self.prompt_new_page(area, window, cx),
            NewKind::Note => self.new_note_in_area(area, window, cx),
        }
    }

    fn prompt_new_page(
        &mut self,
        area: Option<(HostId, rho_desk::cells::Id)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on_submit = Rc::new(
            move |workspace: &mut Workspace,
                  input: String,
                  window: &mut Window,
                  cx: &mut Context<Workspace>| {
                let input = input.trim();
                if input.is_empty() {
                    return;
                }
                let url = if input.contains("://") {
                    input.to_owned()
                } else {
                    format!("https://{input}")
                };
                workspace.create_browser_page(url, area.clone(), window, cx);
            },
        );
        self.open_prompt(
            "new page:",
            Rc::new(|_, _, _| Vec::new()),
            on_submit,
            window,
            cx,
        );
    }

    fn new_note_in_area(
        &mut self,
        area: Option<(HostId, rho_desk::cells::Id)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = area
            .as_ref()
            .map(|(host, _)| *host)
            .or_else(|| self.hosts.primary())
        else {
            self.notice_on(
                None,
                "new note: no daemon is connected",
                StyleClass::SystemInfo,
                cx,
            );
            return;
        };
        let Some((created, writes)) = self
            .desk_cells
            .create_note_writes(host, area.as_ref().map(|(_, node_id)| node_id.clone()))
        else {
            return;
        };
        let undo = self.desk_cells.delete_writes(created.clone());
        let Some(stamp) = self.apply_desk_writes(host, writes, None, window, cx) else {
            return;
        };
        crate::journal::record(crate::journal::Event::Created {
            node_id: created.clone().into(),
            kind: crate::journal::CreatedKind::Note,
            method: crate::journal::CreateMethod::New,
            at_root: area.is_none(),
        });
        self.dashboard.move_to_tree_node_when_ready(host, created);
        self.sync_tree_dashboard(host, window, cx);
        let transaction_id = self.record_desk_semantic_undo(host, stamp, undo, cx);
        self.pending_semantic_group = Some(transaction_id);
        // A new note is a row on the map, so the map is where the reader
        // has to be to type it. From Home — the front door, and where `n`
        // is usually pressed — the row and the insert cursor were both
        // behind a surface that never came into view, so the title went
        // into nothing and the note read as "nothing happened".
        self.open_overview(window, cx);
        // The note is ready for its first line immediately, rather than
        // reading the title's characters as normal-mode commands.
        self.enter_insert_when_shown(window, cx);
    }
}

/// The prompt shows a window of areas; ranking past that is work the
/// reader never sees.
const AREA_LIMIT: usize = 50;
