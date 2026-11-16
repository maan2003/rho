//! The surface over one usage chart.

use std::sync::Arc;

use editor::Editor;
use editor::display_map::CustomBlockId;
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Pixels, Window, div};
use rho_agents_client::{now_ms, usage};
use rho_frame::{Row, Surface};
use theme::ActiveTheme as _;

use super::paint::{self, Summary};
use super::{Chart, UsageChart, UsageMoved};
use crate::system::AgentsSystem;

/// One chart, as a buffer: the title and the totals are its text, and the
/// picture is a block under them, so search, yank and the point work on it
/// like on any other buffer.
pub struct UsageSurface {
    system: Entity<AgentsSystem>,
    chart: UsageChart,
    buffer: Entity<language::Buffer>,
    editor: Entity<Editor>,
    /// The width the summary was reduced to, in points; zero before the
    /// first frame. Compared against the window on each frame (one number)
    /// so a wider window rebuilds the summary once instead of drawing a
    /// coarser chart forever.
    columns: usize,
    /// How tall the chart was drawn, compared with the width.
    height: Pixels,
    block: Option<CustomBlockId>,
}

impl UsageSurface {
    pub fn new(
        system: Entity<AgentsSystem>,
        chart: UsageChart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local("", cx);
            buffer.set_capability(language::Capability::Read, cx);
            buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer.clone(), None, window, cx);
            rho_window::editor_config::configure(&mut editor, window, cx);
            editor.set_read_only(true);
            editor
        });
        cx.subscribe(&system, |this, _, moved: &UsageMoved, cx| {
            if moved.concerns(this.chart) {
                this.rebuild(cx);
            }
        })
        .detach();
        Self {
            system,
            chart,
            buffer,
            editor,
            columns: 0,
            height: Pixels::ZERO,
            block: None,
        }
    }

    /// What the system holds for this chart, reduced to the picture's
    /// width; `None` until a host has answered.
    fn summary(&self, system: &AgentsSystem) -> Option<Summary> {
        let now = now_ms();
        let days = self.chart.days.count();
        let columns = self.columns;
        match self.chart.chart {
            Chart::RateLimit => {
                let (series, active) = system.quota_history()?;
                Some(Summary::Quota(usage::quota_summary(
                    &series, &active, days, now, columns,
                )))
            }
            Chart::ModelCost => Some(Summary::Cost(usage::cost_summary(
                &system.usage().spend(self.chart)?,
                days,
                now,
                columns,
            ))),
            Chart::UsageShare => Some(Summary::Share(usage::share_summary(
                &system.usage().spend(self.chart)?,
                days,
                now,
                columns,
            ))),
            Chart::AgentCost => Some(Summary::AgentCost(usage::agent_cost_summary(
                &system.usage().agent_cost(self.chart.days)?,
                days,
                now,
                columns,
            ))),
        }
    }

    /// Reduce what the system holds to the chart's width, write the header
    /// the totals live in, and put the block back. Once per arrival and
    /// once per resize; never per frame.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let system = self.system.read(cx);
        let summary = self.summary(system).map(Arc::new);
        let waiting = match system.attached_hosts() {
            0 => "no agent host is attached",
            _ => "waiting for the agent hosts' answer",
        };
        self.write_header(summary.as_deref(), waiting, cx);
        self.reinsert_block(summary, cx);
        cx.notify();
    }

    /// The buffer's text: what the chart is, and the totals that are words
    /// rather than picture.
    fn write_header(&mut self, summary: Option<&Summary>, waiting: &str, cx: &mut Context<Self>) {
        let mut text = format!(
            "{} · last {} days\n",
            self.chart.chart.title(),
            self.chart.days.count()
        );
        match summary {
            Some(Summary::Cost(cost)) => text.push_str(&format!(
                "${:.2} estimated API cost · {} requests{}\n",
                cost.total,
                cost.requests,
                paint::backfill_note(cost.approximate)
            )),
            Some(Summary::Share(share)) => text.push_str(&format!(
                "colors = share · height = smoothed weighted usage (p95-scaled) · {} requests{}\n",
                share.requests,
                paint::backfill_note(share.approximate)
            )),
            Some(Summary::Quota(_) | Summary::AgentCost(_)) => {}
            None => {
                text.push_str(waiting);
                text.push('\n');
            }
        }
        self.buffer.update(cx, |buffer, cx| {
            let old = buffer.len();
            buffer.edit([(0..old, text.as_str())], None, cx);
        });
    }

    fn reinsert_block(&mut self, summary: Option<Arc<Summary>>, cx: &mut Context<Self>) {
        let old = self.block.take();
        let height = self.height;
        // Below the last line of the header, so the words come first and the
        // picture under them, the way a menu block sits under its row.
        self.block = self.editor.update(cx, |editor, cx| {
            if let Some(old) = old {
                editor.remove_blocks([old].into_iter().collect(), None, cx);
            }
            let summary = summary?;
            let anchor = {
                let snapshot = editor.buffer().read(cx).read(cx);
                snapshot.anchor_before(snapshot.len())
            };
            editor
                .insert_blocks([paint::chart_block(anchor, summary, height)], None, cx)
                .into_iter()
                .next()
        });
    }
}

impl Surface for UsageSurface {
    fn title(&self, _cx: &App) -> String {
        self.chart.title()
    }

    fn editor(&self, _cx: &App) -> Option<Entity<Editor>> {
        Some(self.editor.clone())
    }

    /// The other pictures over the same window, and this picture over the
    /// other one. Model cost is `k` here rather than the usage menu's `c`,
    /// which the chat system's rows give to a new channel.
    fn rows(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> Vec<Row> {
        let here = self.chart;
        let open = |key: &'static str, chart| super::open_row(self.system.clone(), key, chart);
        let mut rows: Vec<Row> = Chart::ALL
            .into_iter()
            .filter(|chart| *chart != here.chart)
            .map(|chart| {
                let key = match chart {
                    Chart::RateLimit => "r",
                    Chart::ModelCost => "k",
                    Chart::UsageShare => "s",
                    Chart::AgentCost => "a",
                };
                let to = UsageChart { chart, ..here };
                open(key, to)
            })
            .collect();
        let other = UsageChart {
            days: here.days.other(),
            ..here
        };
        rows.push(open("d", other));
        rows
    }
}

impl Focusable for UsageSurface {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for UsageSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One comparison per frame, and a rebuild only when the answer
        // changes: the first frame, and a resize.
        let columns = paint::columns_for(window);
        let height = paint::height_for(window);
        if columns != self.columns || height != self.height {
            self.columns = columns;
            self.height = height;
            self.rebuild(cx);
        }
        div()
            .key_context("RhoUsage")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.editor.clone())
    }
}
