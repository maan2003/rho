//! VP9 Profile 1, full-resolution color, with no encoder lookahead.
use anyhow::{Result, ensure};
use shiguredo_libvpx as vpx;

pub use crate::FrameKind;

pub const MAX_PIXELS: usize = 4096 * 4096;

#[cfg(feature = "encoder")]
pub struct Encoder {
    inner: vpx::Encoder,
    size: (usize, usize),
    planes: [Vec<u8>; 3],
}

#[cfg(feature = "encoder")]
pub struct Packet {
    pub kind: FrameKind,
    pub data: Vec<u8>,
}

#[cfg(feature = "encoder")]
impl Encoder {
    pub fn new(width: usize, height: usize, bitrate: usize) -> Result<Self> {
        ensure!(
            width > 0 && height > 0 && width.checked_mul(height).is_some_and(|n| n <= MAX_PIXELS),
            "invalid video dimensions"
        );
        let mut config = vpx::EncoderConfig::new(
            width,
            height,
            vpx::ImageFormat::I444,
            vpx::CodecConfig::Vp9(vpx::Vp9Config {
                profile: vpx::Vp9Profile::Profile1,
                tune_content: Some(vpx::ContentType::Screen),
                row_mt: true,
                ..Default::default()
            }),
        );
        config.target_bitrate = bitrate;
        config.deadline = vpx::EncodingDeadline::Realtime;
        config.rate_control = vpx::RateControlMode::Cbr;
        config.cpu_used = Some(7);
        config.threads = std::num::NonZeroUsize::new(2);
        config.error_resilient = true;
        // Startup and explicit recovery own reference-chain resets. Neither a
        // wall-clock timer nor libvpx's frame-count default should force them.
        config.automatic_keyframes = false;
        config.max_quantizer = 40;
        Ok(Self {
            inner: vpx::Encoder::new(config)?,
            size: (width, height),
            planes: std::array::from_fn(|_| vec![0; width * height]),
        })
    }

    pub fn encode(&mut self, bgra: &[u8], kind: FrameKind) -> Result<Vec<Packet>> {
        ensure!(
            bgra.len() == self.size.0 * self.size.1 * 4,
            "invalid BGRA frame length"
        );
        let [y, u, v] = &mut self.planes;
        let mut planar = yuv::YuvPlanarImageMut {
            y_plane: yuv::BufferStoreMut::Borrowed(y),
            y_stride: self.size.0 as u32,
            u_plane: yuv::BufferStoreMut::Borrowed(u),
            u_stride: self.size.0 as u32,
            v_plane: yuv::BufferStoreMut::Borrowed(v),
            v_stride: self.size.0 as u32,
            width: self.size.0 as u32,
            height: self.size.1 as u32,
        };
        yuv::bgra_to_yuv444(
            &mut planar,
            bgra,
            self.size.0 as u32 * 4,
            yuv::YuvRange::Full,
            yuv::YuvStandardMatrix::Bt601,
            yuv::YuvConversionMode::Balanced,
        )?;
        self.inner.encode(
            &vpx::ImageData::I444 {
                y: &self.planes[0],
                u: &self.planes[1],
                v: &self.planes[2],
            },
            &vpx::EncodeOptions {
                force_keyframe: kind == FrameKind::Key,
                // Error resilience resets entropy, segmentation and loop-filter
                // history. Only checkpoints change the durable GOLDEN pixels.
                references: vpx::ReferenceMode::Golden {
                    refresh: kind == FrameKind::Checkpoint,
                },
            },
        )?;
        let mut packets = Vec::new();
        while let Some(frame) = self.inner.next_frame() {
            packets.push(Packet {
                kind: if frame.is_keyframe() {
                    FrameKind::Key
                } else {
                    kind
                },
                data: frame.data().to_vec(),
            });
        }
        Ok(packets)
    }

    pub fn quality(&mut self, bitrate: usize, settled: bool) -> Result<()> {
        let mut params = vpx::ReconfigureParams::default();
        params.target_bitrate = Some(bitrate);
        // Text must converge to a clean image after motion stops. A merely
        // lower lossy quantizer leaves ringing in the final static frame.
        params.max_quantizer = Some(if settled { 0 } else { 40 });
        self.inner.reconfigure(&params)?;
        Ok(())
    }
}

#[cfg(feature = "decoder")]
pub struct Decoder(vpx::Decoder);

pub struct Image {
    pub width: usize,
    pub height: usize,
    pub bgra: Vec<u8>,
}

#[cfg(feature = "decoder")]
impl Decoder {
    pub fn new() -> Result<Self> {
        let mut config = vpx::DecoderConfig::new(vpx::DecoderCodec::Vp9);
        config.threads = 2;
        config.retain_frames = true;
        Ok(Self(vpx::Decoder::new(config)?))
    }

    /// Retain the decoder allocation; no pixel conversion or copy.
    pub fn decode_planes(&mut self, data: &[u8]) -> Result<Option<vpx::RetainedFrame>> {
        ensure!(data.len() <= 16 * 1024 * 1024, "video packet too large");
        self.0.decode(data)?;
        let mut image = None;
        while let Some(frame) = self.0.next_frame()? {
            let (width, height) = (frame.width(), frame.height());
            ensure!(
                width > 0
                    && height > 0
                    && width.checked_mul(height).is_some_and(|n| n <= MAX_PIXELS),
                "invalid decoded dimensions"
            );
            ensure!(
                !frame.is_high_depth() && frame.chroma_shift() == (0, 0),
                "expected VP9 8-bit 4:4:4"
            );
            image = Some(frame.retain()?);
        }
        Ok(image)
    }
    /// Explicit CPU export, not used for live presentation.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Image>> {
        self.decode_planes(data)?
            .map(|frame| export_bgra(&frame))
            .transpose()
    }
}

#[cfg(feature = "decoder")]
pub use vpx::RetainedFrame;

#[cfg(feature = "decoder")]
pub fn export_bgra(frame: &RetainedFrame) -> Result<Image> {
    let (width, height) = (frame.width(), frame.height());
    let mut bgra = vec![0; width * height * 4];
    let planar = yuv::YuvPlanarImage {
        y_plane: frame.plane(0),
        y_stride: frame.stride(0) as u32,
        u_plane: frame.plane(1),
        u_stride: frame.stride(1) as u32,
        v_plane: frame.plane(2),
        v_stride: frame.stride(2) as u32,
        width: width as u32,
        height: height as u32,
    };
    yuv::yuv444_to_bgra(
        &planar,
        &mut bgra,
        width as u32 * 4,
        yuv::YuvRange::Full,
        yuv::YuvStandardMatrix::Bt601,
    )?;
    Ok(Image {
        width,
        height,
        bgra,
    })
}

#[cfg(all(test, feature = "encoder", feature = "decoder"))]
mod tests {
    use super::*;

    #[test]
    fn states_can_be_skipped_reordered_and_repeated_across_checkpoint_promotions() -> Result<()> {
        let (w, h) = (257, 129);
        let mut encoder = Encoder::new(w, h, 2_000_000)?;
        let mut full = Decoder::new()?;
        let mut checkpoints_only = Decoder::new()?;
        let mut reordered = Decoder::new()?;
        // Thin colored strokes, asymmetric rows and odd dimensions expose
        // chroma, stride and stale segmentation/loop-filter history mistakes.
        let mut pixels = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let color = if (x / 3 + y / 7) % 5 == 0 {
                    [17, 210, 63, 255]
                } else {
                    [231, 29, 181, 255]
                };
                pixels.extend_from_slice(&color);
            }
        }
        // Even a State request at startup must be reported as an actual Key.
        let root = encoder.encode(&pixels, FrameKind::State)?.remove(0);
        assert_eq!(root.kind, FrameKind::Key);
        let expected = visible_planes(full.decode_planes(&root.data)?.unwrap());
        assert_eq!(
            visible_planes(checkpoints_only.decode_planes(&root.data)?.unwrap()),
            expected
        );
        assert_eq!(
            visible_planes(reordered.decode_planes(&root.data)?.unwrap()),
            expected
        );

        for promotion in 0..8 {
            let mut states = Vec::new();
            for state in 0..9 {
                let mut changed = pixels.clone();
                // Motion touches an area different from the accumulated
                // checkpoint edits. Each ordinary frame differs from its base.
                for y in 51..113 {
                    for x in 77..219 {
                        if (x + state * 7 + y * 3) % 11 < 4 {
                            let i = (y * w + x) * 4;
                            changed[i..i + 4].copy_from_slice(&[
                                ((x * 13 + promotion * 19) % 251) as u8,
                                ((y * 29 + state * 23) % 253) as u8,
                                ((x + y * 5 + state * 31) % 249) as u8,
                                255,
                            ]);
                        }
                    }
                }
                // Both directions of the lossy/lossless transition occur
                // inside an epoch, not merely at keyframes.
                encoder.quality(2_000_000, state % 3 == 1)?;
                let packet = encoder.encode(&changed, FrameKind::State)?.remove(0);
                assert_eq!(packet.kind, FrameKind::State);
                // Profile 1, shown inter frame, error-resilient: the first
                // byte is fixed and the next byte is refresh_frame_flags.
                // Zero refresh proves no reference slot was changed.
                assert_eq!(&packet.data[..2], &[0xa7, 0]);
                let expected = visible_planes(full.decode_planes(&packet.data)?.unwrap());
                states.push((packet, expected));
            }
            // No ordinary frame is needed by later ordinary frames. Decode
            // newest first, then older frames, repetitions, and newest again.
            for index in [8, 0, 5, 8, 2, 2, 7, 1, 8] {
                let (packet, expected) = &states[index];
                assert_eq!(
                    visible_planes(reordered.decode_planes(&packet.data)?.unwrap()),
                    *expected,
                    "promotion {promotion}, reordered state {index}"
                );
            }

            // Promotions retain previous checkpoint edits while changing a
            // separate small region, encouraging a genuine predictive chain.
            for y in 9..43 {
                for x in (promotion * 23 + 3)..(promotion * 23 + 21) {
                    let i = (y * w + x) * 4;
                    pixels[i..i + 4].copy_from_slice(&[
                        (31 + promotion * 23) as u8,
                        (197 - promotion * 17) as u8,
                        (55 + y * 3) as u8,
                        255,
                    ]);
                }
            }
            encoder.quality(2_000_000, promotion % 2 == 0)?;
            let checkpoint = encoder.encode(&pixels, FrameKind::Checkpoint)?.remove(0);
            assert_eq!(
                checkpoint.kind,
                FrameKind::Checkpoint,
                "promotion must not force a key"
            );
            let expected = visible_planes(full.decode_planes(&checkpoint.data)?.unwrap());
            assert_eq!(
                visible_planes(checkpoints_only.decode_planes(&checkpoint.data)?.unwrap()),
                expected,
                "promotion {promotion}, skipping every intervening state"
            );
            assert_eq!(
                visible_planes(reordered.decode_planes(&checkpoint.data)?.unwrap()),
                expected,
                "promotion {promotion}, after reordered/repeated states"
            );
            // The newest state based on this new checkpoint must also match.
            encoder.quality(2_000_000, promotion % 2 != 0)?;
            let newest = encoder.encode(&pixels, FrameKind::State)?.remove(0);
            assert_eq!(newest.kind, FrameKind::State);
            let expected = visible_planes(full.decode_planes(&newest.data)?.unwrap());
            assert_eq!(
                visible_planes(reordered.decode_planes(&newest.data)?.unwrap()),
                expected
            );
            // After the final promotion, a receiver that skipped *every*
            // ordinary picture can immediately decode the newest B-state.
            if promotion == 7 {
                assert_eq!(
                    visible_planes(checkpoints_only.decode_planes(&newest.data)?.unwrap()),
                    expected
                );
            }
        }
        Ok(())
    }

    fn visible_planes(frame: RetainedFrame) -> [Vec<u8>; 3] {
        std::array::from_fn(|p| {
            frame
                .plane(p)
                .chunks(frame.stride(p))
                .take(frame.height())
                .flat_map(|row| row[..frame.width()].iter().copied())
                .collect()
        })
    }

    #[test]
    fn only_startup_and_requested_recovery_insert_keyframes() -> Result<()> {
        let (w, h) = (47, 33);
        let mut encoder = Encoder::new(w, h, 2_000_000)?;
        let mut decoder = Decoder::new()?;
        let mut pixels = vec![255; w * h * 4];
        // Cross libvpx's usual frame-count keyframe boundary before requesting
        // recovery. Scene changes must not silently recreate periodic groups.
        for frame in 0..270 {
            for y in 0..h {
                for x in 0..w {
                    let color = ((x * 17 + y * 31 + frame * 13) % 251) as u8;
                    let i = (y * w + x) * 4;
                    pixels[i..i + 3].copy_from_slice(&[color, color, color]);
                }
            }
            let packets = encoder.encode(
                &pixels,
                if frame == 200 {
                    FrameKind::Key
                } else {
                    FrameKind::State
                },
            )?;
            assert_eq!(packets.len(), 1);
            assert_eq!(
                packets[0].kind == FrameKind::Key,
                frame == 0 || frame == 200,
                "frame {frame}"
            );
            assert!(decoder.decode(&packets[0].data)?.is_some());
        }
        Ok(())
    }

    #[test]
    fn profile_one_preserves_color_in_bottom_half_and_predicts_frames() -> Result<()> {
        // Odd dimensions and alternating colored columns expose stride and
        // chroma-subsampling mistakes; different top/bottom halves expose a
        // decoder that returns only half of the chroma plane.
        let (w, h) = (65, 47);
        let mut pixels = Vec::new();
        for y in 0..h {
            for x in 0..w {
                pixels.extend_from_slice(if y < 23 {
                    if x % 2 == 0 {
                        &[20, 30, 220, 255]
                    } else {
                        &[200, 180, 10, 255]
                    }
                } else if x % 2 == 0 {
                    &[210, 25, 40, 255]
                } else {
                    &[15, 220, 160, 255]
                });
            }
        }
        let mut encoder = Encoder::new(w, h, 8_000_000)?;
        encoder.quality(8_000_000, true)?;
        let mut decoder = Decoder::new()?;
        for kind in [FrameKind::Key, FrameKind::State, FrameKind::Key] {
            let packets = encoder.encode(&pixels, kind)?;
            assert_eq!(packets.len(), 1);
            assert_eq!(packets[0].kind, kind);
            let image = decoder.decode(&packets[0].data)?.unwrap();
            assert_eq!((image.width, image.height), (w, h));
            for (got, want) in image.bgra.iter().zip(&pixels) {
                assert!((*got as i16 - *want as i16).abs() <= 18, "{got} != {want}");
            }
        }
        Ok(())
    }
    #[test]
    fn settled_frame_repairs_lossy_text_edges_without_a_keyframe() -> Result<()> {
        let (w, h) = (257, 129);
        let mut pixels = Vec::new();
        // Narrow, asymmetric strokes with antialiased gray edges. Grayscale
        // has no chroma detail to lose. Allow only the integer color
        // conversion's rounding error, not lossy compression residue.
        for y in 0..h {
            for x in 0..w {
                let gray = [19, 57, 113, 201, 239][(x / 3 + y / 5) % 5];
                pixels.extend_from_slice(&[gray, gray, gray, 255]);
            }
        }
        let mut encoder = Encoder::new(w, h, 128_000)?;
        let mut decoder = Decoder::new()?;
        for round in 0..2 {
            encoder.quality(128_000, false)?;
            let motion = encoder
                .encode(
                    &pixels,
                    if round == 0 {
                        FrameKind::Key
                    } else {
                        FrameKind::State
                    },
                )?
                .remove(0);
            decoder.decode(&motion.data)?.unwrap();
            encoder.quality(128_000, true)?;
            let refined = encoder.encode(&pixels, FrameKind::State)?.remove(0);
            assert!(refined.kind == FrameKind::State);
            let image = decoder.decode(&refined.data)?.unwrap();
            let max_error = image
                .bgra
                .iter()
                .zip(&pixels)
                .map(|(got, want)| (*got as i16 - *want as i16).abs())
                .max()
                .unwrap();
            assert!(
                max_error <= 2,
                "round {round}: maximum channel error {max_error}"
            );
            // Exercise the transition back out of zero-quantizer refinement.
            pixels[..4].copy_from_slice(&[87, 87, 87, 255]);
        }
        Ok(())
    }

    #[test]
    fn retained_planes_survive_reuse_keyframes_and_decoder_drop() -> Result<()> {
        let (width, height) = (65, 47);
        let mut encoder = Encoder::new(width, height, 8_000_000)?;
        let mut decoder = Decoder::new()?;
        let pixels = vec![73u8; width * height * 4];
        let first = encoder.encode(&pixels, FrameKind::Key)?.remove(0);
        let frozen = decoder.decode_planes(&first.data)?.unwrap();
        let saved: Vec<_> = (0..3).map(|p| frozen.plane(p).to_vec()).collect();
        let mut allocations = std::collections::HashSet::new();
        for i in 0..40 {
            let mut pixels = pixels.clone();
            for (n, pixel) in pixels.chunks_exact_mut(4).enumerate() {
                pixel.copy_from_slice(&[
                    (n % 251) as u8,
                    (i * 5) as u8,
                    (n / width * 3) as u8,
                    255,
                ]);
            }
            let packet = encoder
                .encode(
                    &pixels,
                    if i % 7 == 0 {
                        FrameKind::Key
                    } else {
                        FrameKind::State
                    },
                )?
                .remove(0);
            let frame = decoder.decode_planes(&packet.data)?.unwrap();
            allocations.insert(frame.plane(0).as_ptr() as usize);
            for p in 0..3 {
                assert_eq!(frozen.plane(p), saved[p]);
            }
        }
        assert!(allocations.len() < 40, "decoder buffers must be reused");
        for (width, height) in [(129, 71), (31, 19)] {
            let mut encoder = Encoder::new(width, height, 8_000_000)?;
            let packet = encoder
                .encode(&vec![150; width * height * 4], FrameKind::Key)?
                .remove(0);
            let resized = decoder.decode_planes(&packet.data)?.unwrap();
            assert_eq!((resized.width(), resized.height()), (width, height));
            for p in 0..3 {
                assert_eq!(frozen.plane(p), saved[p]);
            }
        }
        drop(decoder);
        for p in 0..3 {
            assert_eq!(frozen.plane(p), saved[p]);
        }
        Ok(())
    }
}
