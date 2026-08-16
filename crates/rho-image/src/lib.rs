//! Bounded prompt-image preparation.
//!
//! Inputs are decoded to pixels and encoded as a fresh PNG. This deliberately
//! drops container metadata, color profiles, extra frames, and the source
//! encoding before bytes enter durable model context.

use std::io::Cursor;

use anyhow::{Context as _, ensure};
use image::imageops::FilterType;
use image::{ImageDecoder as _, ImageReader};
use rho_core::{ImageContent, ImageDetail};
use tokio::sync::Semaphore;

pub const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_SOURCE_DIMENSION: u32 = 16_384;
pub const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_OUTPUT_DIMENSION: u32 = 2_048;
pub const MAX_ORIGINAL_DIMENSION: u32 = 6_000;
pub const MAX_OUTPUT_BYTES: usize = (10 * 1024 * 1024 / 4) * 3;
pub const PATCH_SIZE: u32 = 32;
pub const MAX_PATCHES: u64 = 2_500;
pub const MAX_ORIGINAL_PATCHES: u64 = 10_000;

static PREPARE_PERMIT: Semaphore = Semaphore::const_new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedImage {
    pub content: ImageContent,
    pub width: u32,
    pub height: u32,
}

/// Decode one supported image to pixels, resize it to the model budget, and
/// encode a metadata-free, single-frame PNG.
pub async fn prepare(bytes: Vec<u8>) -> anyhow::Result<PreparedImage> {
    prepare_with_detail(bytes, ImageDetail::High).await
}

pub async fn prepare_with_detail(
    bytes: Vec<u8>,
    detail: ImageDetail,
) -> anyhow::Result<PreparedImage> {
    let permit = PREPARE_PERMIT
        .acquire()
        .await
        .expect("image preparation semaphore is never closed");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        prepare_pixels(&bytes, detail)
    })
    .await
    .map_err(|error| anyhow::anyhow!("image preparation task failed: {error}"))?
}

fn prepare_pixels(bytes: &[u8], detail: ImageDetail) -> anyhow::Result<PreparedImage> {
    ensure!(!bytes.is_empty(), "image is empty");
    ensure!(
        bytes.len() <= MAX_SOURCE_BYTES,
        "image exceeds the 10 MiB source limit"
    );

    let format = image::guess_format(bytes).context("unsupported or invalid image format")?;
    ensure!(
        matches!(
            format,
            image::ImageFormat::Png
                | image::ImageFormat::Jpeg
                | image::ImageFormat::WebP
                | image::ImageFormat::Gif
        ),
        "unsupported image format: {format:?}"
    );

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits.clone());
    let mut decoder = reader
        .into_decoder()
        .context("could not initialize image decoder")?;
    limits
        .reserve(decoder.total_bytes())
        .context("decoded image exceeds allocation limit")?;
    decoder
        .set_limits(limits)
        .context("image exceeds decoder limits")?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut decoded =
        image::DynamicImage::from_decoder(decoder).context("could not decode image pixels")?;
    decoded.apply_orientation(orientation);
    let (max_dimension, max_patches) = match detail {
        ImageDetail::High => (MAX_OUTPUT_DIMENSION, MAX_PATCHES),
        ImageDetail::Original => (MAX_ORIGINAL_DIMENSION, MAX_ORIGINAL_PATCHES),
    };
    let (width, height) = output_dimensions(
        decoded.width(),
        decoded.height(),
        max_dimension,
        max_patches,
    );
    let pixels = if (width, height) == (decoded.width(), decoded.height()) {
        decoded
    } else {
        decoded.resize_exact(width, height, FilterType::Triangle)
    };

    let mut png = Cursor::new(Vec::new());
    pixels
        .write_to(&mut png, image::ImageFormat::Png)
        .context("could not encode image pixels")?;
    ensure!(
        png.get_ref().len() <= MAX_OUTPUT_BYTES,
        "prepared image exceeds the 10 MiB encoded limit"
    );
    Ok(PreparedImage {
        content: ImageContent {
            media_type: "image/png".to_owned(),
            data: png.into_inner(),
            detail,
        },
        width,
        height,
    })
}

fn output_dimensions(width: u32, height: u32, max_dimension: u32, max_patches: u64) -> (u32, u32) {
    // Patch-grid sizing follows OpenAI Codex's prompt image preparation
    // (Apache-2.0), including its rounding order.
    let width = width.max(1);
    let height = height.max(1);
    if dimensions_fit(width, height, max_dimension, max_patches) {
        return (width, height);
    }

    let max_dimension_scale = (f64::from(max_dimension) / f64::from(width.max(height))).min(1.0);
    let width = ((f64::from(width) * max_dimension_scale).round() as u32).max(1);
    let height = ((f64::from(height) * max_dimension_scale).round() as u32).max(1);
    if dimensions_fit(width, height, max_dimension, max_patches) {
        return (width, height);
    }

    let width_f64 = f64::from(width);
    let height_f64 = f64::from(height);
    let patch_size = f64::from(PATCH_SIZE);
    let mut scale = (patch_size * patch_size * max_patches as f64 / width_f64 / height_f64).sqrt();
    let scaled_patches_wide = width_f64 * scale / patch_size;
    let scaled_patches_high = height_f64 * scale / patch_size;
    scale *= (scaled_patches_wide.floor() / scaled_patches_wide)
        .min(scaled_patches_high.floor() / scaled_patches_high);
    (
        ((width_f64 * scale).floor() as u32).max(1),
        ((height_f64 * scale).floor() as u32).max(1),
    )
}

fn dimensions_fit(width: u32, height: u32, max_dimension: u32, max_patches: u64) -> bool {
    let patches_wide = width.div_ceil(PATCH_SIZE);
    let patches_high = height.div_ceil(PATCH_SIZE);
    let patch_count = u64::from(patches_wide) * u64::from(patches_high);
    width <= max_dimension && height <= max_dimension && patch_count <= max_patches
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, ImageBuffer, Rgba};

    use super::*;

    #[test]
    fn reencodes_pixels_as_bounded_png() {
        let source =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(4096, 1024, Rgba([10, 20, 30, 255])));
        let mut bytes = Cursor::new(Vec::new());
        source
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();

        let prepared = prepare_pixels(&bytes.into_inner(), ImageDetail::High).unwrap();
        assert_eq!(prepared.content.media_type, "image/png");
        assert_eq!((prepared.width, prepared.height), (2048, 512));
        let decoded = image::load_from_memory(&prepared.content.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2048, 512));
    }

    #[test]
    fn rejects_declared_image_garbage() {
        assert!(prepare_pixels(b"not an image", ImageDetail::High).is_err());
    }

    #[test]
    fn square_image_uses_codex_patch_grid_budget() {
        assert_eq!(
            output_dimensions(2048, 2048, MAX_OUTPUT_DIMENSION, MAX_PATCHES),
            (1600, 1600)
        );
    }

    #[test]
    fn original_detail_uses_codex_original_budget() {
        assert_eq!(
            output_dimensions(4096, 1024, MAX_ORIGINAL_DIMENSION, MAX_ORIGINAL_PATCHES),
            (4096, 1024)
        );
        assert_eq!(
            output_dimensions(6401, 100, MAX_ORIGINAL_DIMENSION, MAX_ORIGINAL_PATCHES),
            (6000, 94)
        );
    }
}
