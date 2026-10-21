use std::path::PathBuf;

use anyhow::Context as _;
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
        // Any file the notebook could open itself: its Python reads
        // anywhere this process can, so a narrower view would protect nothing.
        let path = visible.clone();
        let bytes = tokio::task::spawn_blocking(move || read_bounded(&path))
            .await
            .context("image read task panicked")??;
        let prepared = rho_image::prepare_with_detail(bytes, args.detail).await?;
        Ok((visible, prepared))
    }
}

/// A regular file of at most [`rho_image::MAX_SOURCE_BYTES`].
fn read_bounded(path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let max_len = rho_image::MAX_SOURCE_BYTES;
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file(), "not a regular file: {}", path.display());
    anyhow::ensure!(
        metadata.len() <= max_len as u64,
        "{} is {} bytes, over the {max_len} byte limit",
        path.display(),
        metadata.len()
    );
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(max_len as u64 + 1).read_to_end(&mut contents)?;
    anyhow::ensure!(
        contents.len() <= max_len,
        "{} grew past the {max_len} byte limit while reading",
        path.display()
    );
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, ImageBuffer, Rgba};

    use super::*;

    #[tokio::test]
    async fn loads_an_image_outside_the_workset() {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = temp.path().join("capture.png");
        DynamicImage::ImageRgba8(ImageBuffer::from_pixel(3, 2, Rgba([1, 2, 3, 255])))
            .save(&path)
            .unwrap();
        let (text, image) = ImageTools::new("/src".into())
            .view(ViewImageArgs {
                path: path.clone(),
                detail: ImageDetail::High,
            })
            .await
            .unwrap();
        assert!(text.contains("(3x2)"), "{text}");
        assert_eq!(image.media_type, "image/png");
    }

    #[tokio::test]
    async fn loads_relative_path_with_original_detail() {
        let temp = tempfile::tempdir().unwrap();
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
