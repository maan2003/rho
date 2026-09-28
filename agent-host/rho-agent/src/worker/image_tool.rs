use std::path::PathBuf;

use rho_agent_types::transcript::{ImageContent, ImageDetail};

#[derive(Clone)]
pub(crate) struct ImageTools {
    cwd: camino::Utf8PathBuf,
}

/// The arguments of the notebook's `view_image()`.
pub(crate) struct ViewImageArgs {
    pub(crate) path: PathBuf,
    pub(crate) detail: ImageDetail,
}

impl ImageTools {
    pub(crate) fn new(cwd: camino::Utf8PathBuf) -> Self {
        Self { cwd }
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

    async fn load(
        &self,
        args: ViewImageArgs,
    ) -> anyhow::Result<(PathBuf, rho_image::PreparedImage)> {
        let visible = if args.path.is_absolute() {
            args.path
        } else {
            self.cwd.as_std_path().join(args.path)
        };
        let bytes = rho_fs_view::read_file_bounded(
            camino::Utf8Path::new(rho_fs_view::MOUNT_ROOT),
            &visible,
            rho_image::MAX_SOURCE_BYTES,
        )
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
        let temp = tempfile::tempdir_in("/src").unwrap();
        let source =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(4096, 1, Rgba([10, 20, 30, 255])));
        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        source.save(work.join("image.png")).unwrap();
        let args = ViewImageArgs {
            path: "image.png".into(),
            detail: ImageDetail::Original,
        };
        let (_, image) = ImageTools::new(camino::Utf8PathBuf::from_path_buf(work).unwrap())
            .view(args)
            .await
            .unwrap();

        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.detail, ImageDetail::Original);
        let decoded = image::load_from_memory(&image.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4096, 1));
    }
}
