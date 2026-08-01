use std::sync::Arc;

use gpui::prelude::*;
use gpui::{Context, Render, RenderImage, Task, Window, div, img};
use theme::ActiveTheme as _;

use crate::connection::VisualizationClient;

enum State {
    Pending,
    Loading,
    Ready(Arc<RenderImage>),
    Error(String),
}

/// A lazily fetched and rasterized immutable visualization.
pub struct Visualization {
    id: String,
    client: VisualizationClient,
    state: State,
    task: Option<Task<()>>,
}

impl Visualization {
    pub fn new(id: String, client: VisualizationClient) -> Self {
        Self {
            id,
            client,
            state: State::Pending,
            task: None,
        }
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.state, State::Pending) {
            return;
        }
        self.state = State::Loading;
        let request = self.client.get(self.id.clone(), cx);
        let renderer = cx.svg_renderer();
        let executor = cx.background_executor().clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let artifact = request
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                anyhow::ensure!(
                    artifact.mime_type == "image/svg+xml",
                    "unsupported visualization type {}",
                    artifact.mime_type
                );
                executor
                    .spawn(async move {
                        renderer
                            .render_single_frame(&artifact.content, 1.0)
                            .map_err(|error| anyhow::anyhow!(error.to_string()))
                    })
                    .await
            }
            .await;
            this.update_in(cx, |this, _, cx| {
                this.state = match result {
                    Ok(image) => State::Ready(image),
                    Err(error) => State::Error(format!("{error:#}")),
                };
                this.task = None;
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Render for Visualization {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.start(window, cx);
        div()
            .size_full()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .child(match &self.state {
                State::Ready(image) => img(image.clone()).size_full().into_any_element(),
                State::Pending | State::Loading => div()
                    .text_color(cx.theme().colors().text_muted)
                    .child("loading visualization…")
                    .into_any_element(),
                State::Error(error) => div()
                    .text_color(cx.theme().colors().text_muted)
                    .child(format!("visualization unavailable: {error}"))
                    .into_any_element(),
            })
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    #[gpui::test]
    fn gpui_renderer_rasterizes_svg_for_the_transcript(cx: &mut TestAppContext) {
        let renderer = cx.update(|cx| cx.svg_renderer());
        let image = renderer
            .render_single_frame(
                b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 800 450\"><rect width=\"800\" height=\"450\" fill=\"red\"/></svg>",
                1.0,
            )
            .unwrap();
        let size = image.size(0);
        assert!(size.width.0 > 0);
        assert!(size.height.0 > 0);
    }
}
