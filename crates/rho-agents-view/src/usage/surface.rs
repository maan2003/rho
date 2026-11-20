//! The surface over one usage chart.

use std::sync::Arc;

use editor::display_map::CustomBlockId;
use editor::{Editor, HighlightKey};
use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Focusable, HighlightStyle, Pixels, Window, div};
use language::Point;
use multi_buffer::MultiBufferOffset;
use rho_agents_client::{now_ms, usage};
use rho_frame::Surface;
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

/// Between two legend entries: wide enough that `opus $1.20   gpt $0.40`
/// reads as two entries rather than four words.
const LEGEND_GAP: &str = "   ";

/// The legend's colours: one highlight per entry, at keys no other surface
/// uses.
const LEGEND_KEY_BASE: usize = usize::MAX / 6;

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
        cx.subscribe(
            &system,
            |this, system, moved: &UsageMoved, cx| match moved {
                UsageMoved::Connected => {
                    let chart = this.chart;
                    system.update(cx, |system, cx| system.ask(chart, cx));
                }
                moved if moved.concerns(this.chart) => this.rebuild(cx),
                _ => {}
            },
        )
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
    /// width, or the words that stand in the picture's place when there is
    /// nothing to draw.
    fn summary(&self, system: &AgentsSystem) -> Result<Summary, String> {
        let now = now_ms();
        let days = self.chart.days.count();
        let columns = self.columns;
        let waiting = || match system.attached_hosts() {
            0 => "no agent host is attached".to_owned(),
            _ => "waiting for the agent hosts' answer".to_owned(),
        };
        match self.chart.chart {
            Chart::RateLimit => {
                let (series, active) = system.quota_history().ok_or_else(waiting)?;
                // An empty grid cannot tell an account at its limit from no
                // account at all; the words can.
                if series.iter().all(|series| series.points.is_empty()) {
                    return Err(match system.hosts_without_accounts() {
                        Some(hosts) => format!("no accounts on {}", hosts.join(" or ")),
                        None => "no usage yet".to_owned(),
                    });
                }
                Ok(Summary::Quota(usage::quota_summary(
                    &series, &active, days, now, columns,
                )))
            }
            Chart::ModelCost => Ok(Summary::Cost(usage::cost_summary(
                &system.usage().spend(self.chart).ok_or_else(waiting)?,
                days,
                now,
                columns,
            ))),
            Chart::UsageShare => Ok(Summary::Share(usage::share_summary(
                &system.usage().spend(self.chart).ok_or_else(waiting)?,
                days,
                now,
                columns,
            ))),
            Chart::AgentCost => Ok(Summary::AgentCost(usage::agent_cost_summary(
                &system
                    .usage()
                    .agent_cost(self.chart.days)
                    .ok_or_else(waiting)?,
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
        let summary = self.summary(self.system.read(cx)).map(Arc::new);
        self.write_header(summary.as_deref().map_err(String::as_str), cx);
        self.reinsert_block(summary.ok(), cx);
        cx.notify();
    }

    /// The buffer's text: what the chart is, the totals, and what each
    /// colour stands for — everything that is words rather than picture.
    /// The point goes to the legend, the first line that says something
    /// about the picture under it, or to the words standing in for one.
    fn write_header(&mut self, summary: Result<&Summary, &str>, cx: &mut Context<Self>) {
        let mut lines = vec![format!(
            "{} · last {} days",
            self.chart.chart.title(),
            self.chart.days.count()
        )];
        match summary {
            Ok(Summary::Cost(cost)) => lines.push(format!(
                "${:.2} estimated API cost · {} requests{}",
                cost.total,
                cost.requests,
                paint::backfill_note(cost.approximate)
            )),
            Ok(Summary::Share(share)) => lines.push(format!(
                "colors = share · height = smoothed weighted usage (p95-scaled) · {} requests{}",
                share.requests,
                paint::backfill_note(share.approximate)
            )),
            Ok(Summary::Quota(_) | Summary::AgentCost(_)) => {}
            Err(words) => lines.push(words.to_owned()),
        }
        let mut text = lines.join("\n");
        let legend = summary.map_or(&[][..], Summary::legend);
        let mut entries = Vec::new();
        if !legend.is_empty() {
            text.push('\n');
        }
        for (index, entry) in legend.iter().enumerate() {
            if index > 0 {
                text.push_str(LEGEND_GAP);
            }
            entries.push((entry.color, text.len()..text.len() + entry.label.len()));
            text.push_str(&entry.label);
        }
        self.buffer.update(cx, |buffer, cx| {
            let old = buffer.len();
            buffer.edit([(0..old, text.as_str())], None, cx);
        });
        let home = Point::new(text.matches('\n').count() as u32, 0);
        self.editor.update(cx, |editor, cx| {
            // Every highlight in this editor is a legend entry's.
            editor.clear_highlights_with(
                &mut |key| {
                    matches!(key, HighlightKey::SyntaxTreeView(key) if *key >= LEGEND_KEY_BASE)
                },
                cx,
            );
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            for (index, (color, range)) in entries.into_iter().enumerate() {
                let range = snapshot.anchor_after(MultiBufferOffset(range.start))
                    ..snapshot.anchor_before(MultiBufferOffset(range.end));
                let style = HighlightStyle {
                    color: Some(paint::color_of(color, cx)),
                    ..HighlightStyle::default()
                };
                editor.highlight_text(
                    HighlightKey::SyntaxTreeView(LEGEND_KEY_BASE + index),
                    vec![range],
                    style,
                    cx,
                );
            }
            let display = editor.display_snapshot(cx);
            editor.selections.change_with(&display, |selections| {
                selections.select_ranges([home..home]);
            });
            cx.notify();
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
