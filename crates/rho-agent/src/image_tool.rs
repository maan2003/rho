use std::path::PathBuf;
use std::sync::Arc;

use rho_core::{ImageDetail, ToolCall, ToolName, ToolOutput, ToolOutputStatus, ToolSpec, ToolType};
use serde::Deserialize;
use serde_json::json;

use crate::View;

pub(crate) const VIEW_IMAGE_TOOL_NAME: &str = "view_image";

#[derive(Clone)]
pub(crate) struct ImageTools {
    view: Arc<View>,
}

#[derive(Deserialize)]
struct ViewImageArgs {
    path: PathBuf,
    #[serde(default)]
    detail: ImageDetail,
}

impl ImageTools {
    pub(crate) fn new(view: Arc<View>) -> Self {
        Self { view }
    }

    pub(crate) fn spec() -> ToolSpec {
        ToolSpec {
            name: ToolName::try_from(VIEW_IMAGE_TOOL_NAME).expect("static tool name"),
            tool_type: ToolType::Function,
            description: "Loads an image from the agent's filesystem view and returns its pixels to the model. Metadata and animation are discarded.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Image path, relative to the working directory or absolute."
                    },
                    "detail": {
                        "type": "string",
                        "enum": ["high", "original"],
                        "description": "Image detail level. Defaults to `high`; use `original` to preserve exact resolution within the original-detail safety limits."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            format: None,
        }
    }

    pub(crate) async fn call(&self, call: ToolCall) -> ToolOutput {
        match self.load(&call.arguments).await {
            Ok((path, prepared)) => ToolOutput {
                output: Arc::new(format!(
                    "Loaded image {} ({}x{}).",
                    path.display(),
                    prepared.width,
                    prepared.height
                )),
                full_output: None,
                images: Arc::new(vec![prepared.content]),
                status: ToolOutputStatus::Success,
            },
            Err(error) => ToolOutput {
                output: Arc::new(error.to_string()),
                full_output: None,
                images: Arc::new(Vec::new()),
                status: ToolOutputStatus::Error,
            },
        }
    }

    async fn load(&self, arguments: &str) -> anyhow::Result<(PathBuf, rho_image::PreparedImage)> {
        let args: ViewImageArgs = serde_json::from_str(arguments)?;
        let visible = if args.path.is_absolute() {
            args.path
        } else {
            self.view.cwd().as_std_path().join(args.path)
        };
        let bytes = self
            .view
            .read_file_bounded(&visible, rho_image::MAX_SOURCE_BYTES)
            .await?;
        let prepared = rho_image::prepare_with_detail(bytes, args.detail).await?;
        Ok((visible, prepared))
    }
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, ImageBuffer, Rgba};
    use rho_core::ToolCallId;

    use super::*;

    #[tokio::test]
    async fn loads_relative_path_with_original_detail() {
        let temp = tempfile::tempdir().unwrap();
        let source =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(4096, 1, Rgba([10, 20, 30, 255])));
        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        source.save(work.join("image.png")).unwrap();
        let worksets = rho_fs_view::Worksets::open(
            temp.path().join("state"),
            Default::default(),
            Default::default(),
            rho_fs_view::StoreService::None,
        )
        .await
        .unwrap();
        // Reads never build the namespace, so no user namespace is needed.
        let view = worksets
            .adopt(&work)
            .unwrap()
            .enter(
                rho_fs_view::Mode::View {
                    home_skeleton: None,
                },
                camino::Utf8Path::new(rho_fs_view::MOUNT_ROOT),
            )
            .unwrap();
        let output = ImageTools::new(view)
            .call(ToolCall {
                id: ToolCallId::try_from("image-call".to_owned()).unwrap(),
                name: ToolName::try_from(VIEW_IMAGE_TOOL_NAME).unwrap(),
                tool_type: ToolType::Function,
                arguments: r#"{"path":"image.png","detail":"original"}"#.to_owned(),
            })
            .await;

        assert_eq!(output.status, ToolOutputStatus::Success);
        assert_eq!(output.images.len(), 1);
        assert_eq!(output.images[0].media_type, "image/png");
        assert_eq!(output.images[0].detail, ImageDetail::Original);
        let decoded = image::load_from_memory(&output.images[0].data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4096, 1));
    }
}
