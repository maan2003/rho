use std::path::PathBuf;
use std::sync::Arc;

use rho_core::{ImageContent, ImageDetail};
use serde::Deserialize;

use crate::View;

#[derive(Clone)]
pub(crate) struct ImageTools {
    view: Arc<View>,
}

/// The arguments of the notebook's `view_image()`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewImageArgs {
    path: PathBuf,
    #[serde(default)]
    detail: ImageDetail,
}

impl ImageTools {
    pub(crate) fn new(view: Arc<View>) -> Self {
        Self { view }
    }

    /// Load an image for the model: a one-line description and the image.
    pub(crate) async fn view(&self, args: ViewImageArgs) -> anyhow::Result<(String, ImageContent)> {
        let (path, prepared) = self.load(args).await?;
        Ok((
            format!(
                "Loaded image {} ({}x{}).",
                path.display(),
                prepared.width,
                prepared.height
            ),
            prepared.content,
        ))
    }

    async fn load(&self, args: ViewImageArgs) -> anyhow::Result<(PathBuf, rho_image::PreparedImage)> {
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
        let args = serde_json::from_str(r#"{"path":"image.png","detail":"original"}"#).unwrap();
        let (_, image) = ImageTools::new(view).view(args).await.unwrap();

        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.detail, ImageDetail::Original);
        let decoded = image::load_from_memory(&image.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4096, 1));
    }
}
