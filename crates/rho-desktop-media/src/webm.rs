//! Pure-Rust WebM (EBML/Matroska) muxer and VP8/VP9 video recorder.
//!
//! Writes browser-playable WebM containers (`<video>` compatible) with
//! `SeekHead`, `Info` (patched `Duration`), `Tracks` (`V_VP9` or `V_VP8`),
//! `Cluster`/`SimpleBlock` frames, and `Cues` keyframe index.

use std::io::{Seek, SeekFrom, Write};

use anyhow::{Context as _, Result, ensure};

const UNKNOWN_SIZE_VINT: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

// EBML / Matroska / WebM element IDs.
const ID_EBML: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
const ID_EBML_VERSION: &[u8] = &[0x42, 0x86];
const ID_EBML_READ_VERSION: &[u8] = &[0x42, 0xF7];
const ID_EBML_MAX_ID_LENGTH: &[u8] = &[0x42, 0xF2];
const ID_EBML_MAX_SIZE_LENGTH: &[u8] = &[0x42, 0xF3];
const ID_DOC_TYPE: &[u8] = &[0x42, 0x82];
const ID_DOC_TYPE_VERSION: &[u8] = &[0x42, 0x87];
const ID_DOC_TYPE_READ_VERSION: &[u8] = &[0x42, 0x85];

const ID_SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
const ID_SEEK_HEAD: &[u8] = &[0x11, 0x4D, 0x9B, 0x74];
const ID_SEEK: &[u8] = &[0x4D, 0xBB];
const ID_SEEK_ID: &[u8] = &[0x53, 0xAB];
const ID_SEEK_POSITION: &[u8] = &[0x53, 0xAC];

const ID_INFO: &[u8] = &[0x15, 0x49, 0xA9, 0x66];
const ID_TIMECODE_SCALE: &[u8] = &[0x2A, 0xD7, 0xB1];
const ID_MUXING_APP: &[u8] = &[0x4D, 0x80];
const ID_WRITING_APP: &[u8] = &[0x57, 0x41];
const ID_DURATION: &[u8] = &[0x44, 0x89];

const ID_TRACKS: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
const ID_TRACK_ENTRY: &[u8] = &[0xAE];
const ID_TRACK_NUMBER: &[u8] = &[0xD7];
const ID_TRACK_UID: &[u8] = &[0x73, 0xC5];
const ID_TRACK_TYPE: &[u8] = &[0x83];
const ID_FLAG_LACING: &[u8] = &[0x9C];
const ID_CODEC_ID: &[u8] = &[0x86];
const ID_DEFAULT_DURATION: &[u8] = &[0x23, 0xE3, 0x83];
const ID_VIDEO: &[u8] = &[0xE0];
const ID_PIXEL_WIDTH: &[u8] = &[0xB0];
const ID_PIXEL_HEIGHT: &[u8] = &[0xBA];
const ID_COLOUR: &[u8] = &[0x55, 0xB0];
const ID_MATRIX_COEFFICIENTS: &[u8] = &[0x55, 0xB1];
const ID_RANGE: &[u8] = &[0x55, 0xB9];
const ID_TRANSFER_CHARACTERISTICS: &[u8] = &[0x55, 0xBA];
const ID_PRIMARIES: &[u8] = &[0x55, 0xBB];

const ID_CLUSTER: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];
const ID_TIMECODE: &[u8] = &[0xE7];
const ID_SIMPLE_BLOCK: &[u8] = &[0xA3];

const ID_CUES: &[u8] = &[0x1C, 0x53, 0xBB, 0x6B];
const ID_CUE_POINT: &[u8] = &[0xBB];
const ID_CUE_TIME: &[u8] = &[0xB3];
const ID_CUE_TRACK_POSITIONS: &[u8] = &[0xB7];
const ID_CUE_TRACK: &[u8] = &[0xF7];
const ID_CUE_CLUSTER_POSITION: &[u8] = &[0xF1];
const ID_CUE_RELATIVE_POSITION: &[u8] = &[0xF0];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WebmCodec {
    Vp8,
    #[default]
    Vp9,
}

impl WebmCodec {
    pub fn codec_id(self) -> &'static str {
        match self {
            Self::Vp8 => "V_VP8",
            Self::Vp9 => "V_VP9",
        }
    }

    fn doc_type_version(self) -> u64 {
        match self {
            Self::Vp8 => 2,
            Self::Vp9 => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CuePoint {
    time_ms: u64,
    cluster_pos: u64,
    relative_pos: u64,
}

#[derive(Clone, Copy, Debug)]
struct OpenCluster {
    size_pos: u64,
    data_start: u64,
    segment_rel_pos: u64,
    timecode_ms: u64,
}

/// Summary produced when finalizing a WebM recording.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebmStats {
    pub frames: u64,
    pub duration_ms: u64,
    pub bytes: u64,
}

/// Streaming WebM container writer.
pub struct WebmMuxer<W: Write + Seek> {
    writer: W,
    segment_size_pos: u64,
    segment_data_start: u64,
    seek_info_pos: u64,
    seek_tracks_pos: u64,
    seek_cues_pos: u64,
    duration_pos: u64,
    cluster: Option<OpenCluster>,
    cues: Vec<CuePoint>,
    frames: u64,
    first_ts_ms: Option<u64>,
    last_ts_ms: u64,
    default_frame_ms: u64,
    finished: bool,
}

impl<W: Write + Seek> WebmMuxer<W> {
    pub fn new(mut writer: W, width: u32, height: u32, fps: u32, codec: WebmCodec) -> Result<Self> {
        ensure!(width > 0 && height > 0, "video dimensions must be positive");
        ensure!(fps > 0, "fps must be positive");

        write_ebml_header(&mut writer, codec)?;

        writer.write_all(ID_SEGMENT)?;
        let segment_size_pos = writer.stream_position()?;
        writer.write_all(&UNKNOWN_SIZE_VINT)?;
        let segment_data_start = writer.stream_position()?;

        // Fixed-size SeekHead with 3 entries (Info, Tracks, Cues) using 8-byte
        // SeekPositions.
        let (seek_info_pos, seek_tracks_pos, seek_cues_pos) = write_seek_head(&mut writer)?;

        let info_pos = writer.stream_position()?;
        let duration_pos = write_info(&mut writer)?;

        let tracks_pos = writer.stream_position()?;
        write_tracks(&mut writer, width, height, fps, codec)?;

        // Patch Info and Tracks positions into SeekHead immediately.
        let end_pos = writer.stream_position()?;
        writer.seek(SeekFrom::Start(seek_info_pos))?;
        writer.write_all(&(info_pos - segment_data_start).to_be_bytes())?;
        writer.seek(SeekFrom::Start(seek_tracks_pos))?;
        writer.write_all(&(tracks_pos - segment_data_start).to_be_bytes())?;
        writer.seek(SeekFrom::Start(end_pos))?;

        Ok(Self {
            writer,
            segment_size_pos,
            segment_data_start,
            seek_info_pos,
            seek_tracks_pos,
            seek_cues_pos,
            duration_pos,
            cluster: None,
            cues: Vec::new(),
            frames: 0,
            first_ts_ms: None,
            last_ts_ms: 0,
            default_frame_ms: (1000 / u64::from(fps)).max(1),
            finished: false,
        })
    }

    pub fn write_frame(&mut self, timestamp_ms: u64, keyframe: bool, data: &[u8]) -> Result<()> {
        ensure!(!self.finished, "WebM muxer already finished");
        ensure!(!data.is_empty(), "empty video frame");

        // Keep timestamps monotonic and normalized to start at 0 ms.
        let norm_ts_ms = match self.first_ts_ms {
            Some(first) => timestamp_ms.saturating_sub(first).max(self.last_ts_ms),
            None => {
                self.first_ts_ms = Some(timestamp_ms);
                0
            }
        };

        let need_new_cluster = match self.cluster {
            None => true,
            Some(c) => {
                let rel = norm_ts_ms.saturating_sub(c.timecode_ms);
                (keyframe && rel > 0) || rel > i16::MAX as u64
            }
        };
        if need_new_cluster {
            self.start_cluster(norm_ts_ms)?;
        }

        let cluster = self.cluster.expect("cluster started");
        let rel_ts = i16::try_from(norm_ts_ms - cluster.timecode_ms)
            .context("cluster relative timestamp overflow")?;
        let block_pos = self.writer.stream_position()?;

        let block_payload_len = 4usize
            .checked_add(data.len())
            .context("WebM SimpleBlock too large")?;
        self.writer.write_all(ID_SIMPLE_BLOCK)?;
        write_vint_u64(&mut self.writer, block_payload_len as u64)?;
        let flags: u8 = if keyframe { 0x80 } else { 0x00 };
        let mut header = [0x81, 0, 0, flags];
        header[1..3].copy_from_slice(&rel_ts.to_be_bytes());
        self.writer.write_all(&header)?;
        self.writer.write_all(data)?;

        if keyframe {
            self.cues.push(CuePoint {
                time_ms: norm_ts_ms,
                cluster_pos: cluster.segment_rel_pos,
                relative_pos: block_pos - cluster.data_start,
            });
        }

        self.frames += 1;
        self.last_ts_ms = norm_ts_ms;
        Ok(())
    }

    fn start_cluster(&mut self, timecode_ms: u64) -> Result<()> {
        self.close_cluster()?;
        let cluster_pos = self.writer.stream_position()?;
        self.writer.write_all(ID_CLUSTER)?;
        let size_pos = self.writer.stream_position()?;
        self.writer.write_all(&UNKNOWN_SIZE_VINT)?;
        let data_start = self.writer.stream_position()?;
        write_uint_element(&mut self.writer, ID_TIMECODE, timecode_ms)?;
        self.cluster = Some(OpenCluster {
            size_pos,
            data_start,
            segment_rel_pos: cluster_pos - self.segment_data_start,
            timecode_ms,
        });
        Ok(())
    }

    fn close_cluster(&mut self) -> Result<()> {
        if let Some(cluster) = self.cluster.take() {
            let end_pos = self.writer.stream_position()?;
            let size = end_pos - cluster.data_start;
            self.writer.seek(SeekFrom::Start(cluster.size_pos))?;
            self.writer.write_all(&vint_u64_8byte(size))?;
            self.writer.seek(SeekFrom::Start(end_pos))?;
        }
        Ok(())
    }

    pub fn finish(mut self, total_duration_ms: Option<u64>) -> Result<(W, WebmStats)> {
        self.close_cluster()?;
        let cues_pos = self.writer.stream_position()?;
        write_cues(&mut self.writer, &self.cues)?;
        let segment_end = self.writer.stream_position()?;

        let duration_ms = if self.frames == 0 {
            0
        } else {
            let min_duration = self.last_ts_ms.saturating_add(self.default_frame_ms);
            total_duration_ms.unwrap_or(min_duration).max(min_duration)
        };

        self.writer.seek(SeekFrom::Start(self.duration_pos))?;
        self.writer.write_all(&(duration_ms as f64).to_be_bytes())?;

        self.writer.seek(SeekFrom::Start(self.seek_cues_pos))?;
        self.writer
            .write_all(&(cues_pos - self.segment_data_start).to_be_bytes())?;

        let _ = (self.seek_info_pos, self.seek_tracks_pos);

        self.writer.seek(SeekFrom::Start(self.segment_size_pos))?;
        self.writer
            .write_all(&vint_u64_8byte(segment_end - self.segment_data_start))?;
        self.writer.seek(SeekFrom::Start(segment_end))?;
        self.writer.flush()?;
        self.finished = true;

        let stats = WebmStats {
            frames: self.frames,
            duration_ms,
            bytes: segment_end,
        };
        Ok((self.writer, stats))
    }
}

fn write_ebml_header<W: Write>(w: &mut W, codec: WebmCodec) -> Result<()> {
    let mut body = Vec::with_capacity(64);
    write_uint_element(&mut body, ID_EBML_VERSION, 1)?;
    write_uint_element(&mut body, ID_EBML_READ_VERSION, 1)?;
    write_uint_element(&mut body, ID_EBML_MAX_ID_LENGTH, 4)?;
    write_uint_element(&mut body, ID_EBML_MAX_SIZE_LENGTH, 8)?;
    write_bytes_element(&mut body, ID_DOC_TYPE, b"webm")?;
    write_uint_element(&mut body, ID_DOC_TYPE_VERSION, codec.doc_type_version())?;
    write_uint_element(&mut body, ID_DOC_TYPE_READ_VERSION, 2)?;
    write_bytes_element(w, ID_EBML, &body)
}

fn write_seek_head<W: Write + Seek>(w: &mut W) -> Result<(u64, u64, u64)> {
    let entries = [ID_INFO, ID_TRACKS, ID_CUES];
    let entry_size = (ID_SEEK_ID.len() + 1 + 4) + (ID_SEEK_POSITION.len() + 1 + 8);
    let seek_entry_total = ID_SEEK.len() + 1 + entry_size;
    let payload_len = entries.len() * seek_entry_total;

    w.write_all(ID_SEEK_HEAD)?;
    write_vint_u64(w, payload_len as u64)?;

    let mut positions = [0u64; 3];
    for (slot, target_id) in positions.iter_mut().zip(entries) {
        w.write_all(ID_SEEK)?;
        write_vint_u64(w, entry_size as u64)?;
        write_bytes_element(w, ID_SEEK_ID, target_id)?;
        w.write_all(ID_SEEK_POSITION)?;
        write_vint_u64(w, 8)?;
        *slot = w.stream_position()?;
        w.write_all(&0u64.to_be_bytes())?;
    }
    Ok((positions[0], positions[1], positions[2]))
}

fn write_info<W: Write + Seek>(w: &mut W) -> Result<u64> {
    let mut prefix = Vec::with_capacity(64);
    write_uint_element(&mut prefix, ID_TIMECODE_SCALE, 1_000_000)?;
    write_bytes_element(&mut prefix, ID_MUXING_APP, b"rho-wayland")?;
    write_bytes_element(&mut prefix, ID_WRITING_APP, b"rho-wayland")?;

    let duration_elem_len = ID_DURATION.len() + 1 + 8;
    let total_len = prefix.len() + duration_elem_len;

    w.write_all(ID_INFO)?;
    write_vint_u64(w, total_len as u64)?;
    w.write_all(&prefix)?;
    w.write_all(ID_DURATION)?;
    write_vint_u64(w, 8)?;
    let duration_pos = w.stream_position()?;
    w.write_all(&0.0f64.to_be_bytes())?;
    Ok(duration_pos)
}

fn write_tracks<W: Write>(
    w: &mut W,
    width: u32,
    height: u32,
    fps: u32,
    codec: WebmCodec,
) -> Result<()> {
    let mut colour = Vec::with_capacity(32);
    write_uint_element(&mut colour, ID_MATRIX_COEFFICIENTS, 5)?; // BT.601
    write_uint_element(&mut colour, ID_RANGE, 1)?; // Broadcast / limited range
    write_uint_element(&mut colour, ID_TRANSFER_CHARACTERISTICS, 1)?; // BT.709
    write_uint_element(&mut colour, ID_PRIMARIES, 1)?; // BT.709

    let mut video = Vec::with_capacity(64);
    write_uint_element(&mut video, ID_PIXEL_WIDTH, u64::from(width))?;
    write_uint_element(&mut video, ID_PIXEL_HEIGHT, u64::from(height))?;
    write_bytes_element(&mut video, ID_COLOUR, &colour)?;

    let mut entry = Vec::with_capacity(128);
    write_uint_element(&mut entry, ID_TRACK_NUMBER, 1)?;
    write_uint_element(&mut entry, ID_TRACK_UID, 1)?;
    write_uint_element(&mut entry, ID_TRACK_TYPE, 1)?; // video
    write_uint_element(&mut entry, ID_FLAG_LACING, 0)?;
    write_bytes_element(&mut entry, ID_CODEC_ID, codec.codec_id().as_bytes())?;
    write_uint_element(
        &mut entry,
        ID_DEFAULT_DURATION,
        1_000_000_000 / u64::from(fps),
    )?;
    write_bytes_element(&mut entry, ID_VIDEO, &video)?;

    let mut tracks = Vec::with_capacity(entry.len() + 8);
    write_bytes_element(&mut tracks, ID_TRACK_ENTRY, &entry)?;
    write_bytes_element(w, ID_TRACKS, &tracks)
}

fn write_cues<W: Write>(w: &mut W, cues: &[CuePoint]) -> Result<()> {
    let mut body = Vec::with_capacity(cues.len() * 32);
    for cue in cues {
        let mut pos = Vec::with_capacity(24);
        write_uint_element(&mut pos, ID_CUE_TRACK, 1)?;
        write_uint_element(&mut pos, ID_CUE_CLUSTER_POSITION, cue.cluster_pos)?;
        write_uint_element(&mut pos, ID_CUE_RELATIVE_POSITION, cue.relative_pos)?;

        let mut point = Vec::with_capacity(32);
        write_uint_element(&mut point, ID_CUE_TIME, cue.time_ms)?;
        write_bytes_element(&mut point, ID_CUE_TRACK_POSITIONS, &pos)?;
        write_bytes_element(&mut body, ID_CUE_POINT, &point)?;
    }
    write_bytes_element(w, ID_CUES, &body)
}

fn write_bytes_element<W: Write>(w: &mut W, id: &[u8], payload: &[u8]) -> Result<()> {
    w.write_all(id)?;
    write_vint_u64(w, payload.len() as u64)?;
    w.write_all(payload)?;
    Ok(())
}

fn write_uint_element<W: Write>(w: &mut W, id: &[u8], value: u64) -> Result<()> {
    let bytes = value.to_be_bytes();
    let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(7);
    write_bytes_element(w, id, &bytes[first_nonzero..])
}

fn vint_u64_8byte(value: u64) -> [u8; 8] {
    debug_assert!(value < 0x00FF_FFFF_FFFF_FFFF);
    (value | 0x0100_0000_0000_0000).to_be_bytes()
}

fn write_vint_u64<W: Write>(w: &mut W, value: u64) -> Result<()> {
    if value < 0x7F {
        w.write_all(&[(value as u8) | 0x80])?;
    } else if value < 0x3FFF {
        w.write_all(&((value as u16) | 0x4000).to_be_bytes())?;
    } else if value < 0x1F_FFFF {
        let bytes = ((value as u32) | 0x20_0000).to_be_bytes();
        w.write_all(&bytes[1..4])?;
    } else if value < 0x0FFF_FFFF {
        w.write_all(&((value as u32) | 0x1000_0000).to_be_bytes())?;
    } else {
        anyhow::ensure!(value < 0x00FF_FFFF_FFFF_FFFF, "EBML VINT overflow");
        w.write_all(&vint_u64_8byte(value))?;
    }
    Ok(())
}

/// Encodes BGRA8 frames as I420 VP8/VP9 and muxes them into a WebM stream.
#[cfg(feature = "encoder")]
pub struct WebmRecorder<W: Write + Seek> {
    encoder: shiguredo_libvpx::Encoder,
    muxer: WebmMuxer<W>,
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

#[cfg(feature = "encoder")]
impl<W: Write + Seek> WebmRecorder<W> {
    pub fn new(writer: W, width: u32, height: u32, fps: u32, codec: WebmCodec) -> Result<Self> {
        use shiguredo_libvpx as vpx;

        ensure!(
            width > 0
                && height > 0
                && width <= rho_desktop_proto::MAX_DIMENSION
                && height <= rho_desktop_proto::MAX_DIMENSION,
            "invalid video dimensions: {width}x{height}"
        );
        ensure!((1..=120).contains(&fps), "fps must be between 1 and 120");

        let w = width as usize;
        let h = height as usize;
        let codec_cfg = match codec {
            WebmCodec::Vp8 => vpx::CodecConfig::Vp8(vpx::Vp8Config::default()),
            WebmCodec::Vp9 => vpx::CodecConfig::Vp9(vpx::Vp9Config {
                profile: vpx::Vp9Profile::Profile0,
                tune_content: Some(vpx::ContentType::Screen),
                row_mt: true,
                tile_columns: Some(2),
                frame_parallel_decoding: true,
                ..Default::default()
            }),
        };
        let mut config = vpx::EncoderConfig::new(w, h, vpx::ImageFormat::I420, codec_cfg);
        config.fps_numerator = fps as usize;
        config.fps_denominator = 1;
        config.target_bitrate = 4_000_000;
        config.rate_control = vpx::RateControlMode::Cq;
        config.cq_level = 24;
        config.min_quantizer = 4;
        config.max_quantizer = 44;
        config.deadline = vpx::EncodingDeadline::Realtime;
        config.cpu_used = Some(8);
        config.threads = std::num::NonZeroUsize::new(4);
        config.automatic_keyframes = true;
        config.keyframe_interval = std::num::NonZeroUsize::new((fps as usize * 2).max(1));

        let encoder = vpx::Encoder::new(config).context("create libvpx encoder")?;
        let muxer = WebmMuxer::new(writer, width, height, fps, codec)?;
        let y_stride = w.next_multiple_of(2);
        let chroma_len = w.div_ceil(2) * h.div_ceil(2);

        Ok(Self {
            encoder,
            muxer,
            width: w,
            height: h,
            y: vec![0; y_stride * h],
            u: vec![0; chroma_len],
            v: vec![0; chroma_len],
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    pub fn frames(&self) -> u64 {
        self.muxer.frames
    }

    pub fn write_bgra_frame(&mut self, timestamp_ms: u64, bgra: &[u8]) -> Result<()> {
        use shiguredo_libvpx as vpx;

        let expected = self.width * self.height * 4;
        ensure!(
            bgra.len() == expected,
            "invalid BGRA frame size: got {}, expected {expected}",
            bgra.len()
        );

        let y_stride = self.width.next_multiple_of(2);
        let chroma_w = self.width.div_ceil(2);
        let mut planar = yuv::YuvPlanarImageMut {
            y_plane: yuv::BufferStoreMut::Borrowed(&mut self.y),
            y_stride: y_stride as u32,
            u_plane: yuv::BufferStoreMut::Borrowed(&mut self.u),
            u_stride: chroma_w as u32,
            v_plane: yuv::BufferStoreMut::Borrowed(&mut self.v),
            v_stride: chroma_w as u32,
            width: self.width as u32,
            height: self.height as u32,
        };
        yuv::bgra_to_yuv420(
            &mut planar,
            bgra,
            self.width as u32 * 4,
            yuv::YuvRange::Limited,
            yuv::YuvStandardMatrix::Bt601,
            yuv::YuvConversionMode::Balanced,
        )?;

        self.encoder.encode(
            &vpx::ImageData::I420 {
                y: &self.y,
                u: &self.u,
                v: &self.v,
            },
            &vpx::EncodeOptions::default(),
        )?;
        while let Some(frame) = self.encoder.next_frame() {
            self.muxer
                .write_frame(timestamp_ms, frame.is_keyframe(), frame.data())?;
        }
        Ok(())
    }

    pub fn finish(mut self, total_duration_ms: Option<u64>) -> Result<(W, WebmStats)> {
        self.encoder.finish()?;
        let last_ts = self.muxer.last_ts_ms;
        while let Some(frame) = self.encoder.next_frame() {
            self.muxer
                .write_frame(last_ts, frame.is_keyframe(), frame.data())?;
        }
        self.muxer.finish(total_duration_ms)
    }
}

/// Parsed summary of a WebM file's headers, tracks, clusters, cues, and frame
/// payloads.
#[derive(Debug, Default)]
pub struct WebmInspection {
    pub doc_type: String,
    pub codec_id: String,
    pub pixel_width: u64,
    pub pixel_height: u64,
    pub duration_ms: f64,
    pub clusters: usize,
    pub cue_points: usize,
    /// `(timestamp_ms, is_keyframe, payload)` for each `SimpleBlock`.
    pub blocks: Vec<(u64, bool, Vec<u8>)>,
}

fn read_vint_id(data: &[u8], pos: &mut usize) -> Result<u32> {
    ensure!(*pos < data.len(), "unexpected EOF reading EBML ID");
    let first = data[*pos];
    let len = first.leading_zeros() as usize + 1;
    ensure!(len <= 4 && *pos + len <= data.len(), "invalid EBML ID");
    let mut val = 0u32;
    for &b in &data[*pos..*pos + len] {
        val = (val << 8) | u32::from(b);
    }
    *pos += len;
    Ok(val)
}

fn read_vint_size(data: &[u8], pos: &mut usize) -> Result<u64> {
    ensure!(*pos < data.len(), "unexpected EOF reading EBML size");
    let first = data[*pos];
    let len = first.leading_zeros() as usize + 1;
    ensure!(len <= 8 && *pos + len <= data.len(), "invalid EBML size");
    let mask = 0xFFu8.checked_shr(len as u32).unwrap_or(0);
    let mut val = u64::from(first & mask);
    for &b in &data[*pos + 1..*pos + len] {
        val = (val << 8) | u64::from(b);
    }
    *pos += len;
    Ok(val)
}

fn read_uint(slice: &[u8]) -> u64 {
    let mut val = 0u64;
    for &b in slice {
        val = (val << 8) | u64::from(b);
    }
    val
}

/// Parses and validates a finalized WebM container's EBML structure and
/// extracts its video blocks.
pub fn inspect_webm(bytes: &[u8]) -> Result<WebmInspection> {
    let mut parsed = WebmInspection::default();
    let mut pos = 0usize;

    let ebml_id = read_vint_id(bytes, &mut pos)?;
    ensure!(ebml_id == 0x1A45DFA3, "missing EBML header");
    let ebml_size = read_vint_size(bytes, &mut pos)? as usize;
    let ebml_end = pos + ebml_size;
    ensure!(ebml_end <= bytes.len(), "truncated EBML header");
    while pos < ebml_end {
        let id = read_vint_id(bytes, &mut pos)?;
        let sz = read_vint_size(bytes, &mut pos)? as usize;
        let payload = &bytes[pos..pos + sz];
        pos += sz;
        if id == 0x4282 {
            parsed.doc_type = String::from_utf8(payload.to_vec())?;
        }
    }

    let seg_id = read_vint_id(bytes, &mut pos)?;
    ensure!(seg_id == 0x18538067, "missing WebM Segment");
    let seg_size = read_vint_size(bytes, &mut pos)? as usize;
    let seg_start = pos;
    let seg_end = seg_start + seg_size;
    ensure!(seg_end == bytes.len(), "Segment size must match EOF");

    let mut seek_positions = Vec::new();
    while pos < seg_end {
        let id = read_vint_id(bytes, &mut pos)?;
        let sz = read_vint_size(bytes, &mut pos)? as usize;
        let end = pos + sz;
        ensure!(end <= seg_end, "WebM element extends past Segment");
        match id {
            0x114D9B74 => {
                while pos < end {
                    let sid = read_vint_id(bytes, &mut pos)?;
                    let ssz = read_vint_size(bytes, &mut pos)? as usize;
                    ensure!(sid == 0x4DBB, "unexpected element in SeekHead");
                    let send = pos + ssz;
                    let mut target_id = 0u32;
                    let mut target_pos = 0u64;
                    while pos < send {
                        let fid = read_vint_id(bytes, &mut pos)?;
                        let fsz = read_vint_size(bytes, &mut pos)? as usize;
                        let fp = &bytes[pos..pos + fsz];
                        pos += fsz;
                        if fid == 0x53AB {
                            target_id = u32::from_be_bytes(fp.try_into()?);
                        } else if fid == 0x53AC {
                            target_pos = read_uint(fp);
                        }
                    }
                    seek_positions.push((target_id, target_pos));
                }
            }
            0x1549A966 => {
                while pos < end {
                    let iid = read_vint_id(bytes, &mut pos)?;
                    let isz = read_vint_size(bytes, &mut pos)? as usize;
                    let ip = &bytes[pos..pos + isz];
                    pos += isz;
                    if iid == 0x4489 {
                        parsed.duration_ms = f64::from_be_bytes(ip.try_into()?);
                    }
                }
            }
            0x1654AE6B => {
                while pos < end {
                    let tid = read_vint_id(bytes, &mut pos)?;
                    let tsz = read_vint_size(bytes, &mut pos)? as usize;
                    ensure!(tid == 0xAE, "unexpected element in Tracks");
                    let tend = pos + tsz;
                    while pos < tend {
                        let eid = read_vint_id(bytes, &mut pos)?;
                        let esz = read_vint_size(bytes, &mut pos)? as usize;
                        let eend = pos + esz;
                        match eid {
                            0x86 => {
                                parsed.codec_id = String::from_utf8(bytes[pos..eend].to_vec())?;
                            }
                            0xE0 => {
                                while pos < eend {
                                    let vid = read_vint_id(bytes, &mut pos)?;
                                    let vsz = read_vint_size(bytes, &mut pos)? as usize;
                                    let vp = &bytes[pos..pos + vsz];
                                    pos += vsz;
                                    if vid == 0xB0 {
                                        parsed.pixel_width = read_uint(vp);
                                    } else if vid == 0xBA {
                                        parsed.pixel_height = read_uint(vp);
                                    }
                                }
                            }
                            _ => {}
                        }
                        pos = eend;
                    }
                }
            }
            0x1F43B675 => {
                parsed.clusters += 1;
                let mut cluster_ts = 0u64;
                while pos < end {
                    let cid = read_vint_id(bytes, &mut pos)?;
                    let csz = read_vint_size(bytes, &mut pos)? as usize;
                    let cp = &bytes[pos..pos + csz];
                    pos += csz;
                    if cid == 0xE7 {
                        cluster_ts = read_uint(cp);
                    } else if cid == 0xA3 {
                        ensure!(cp.len() >= 4 && cp[0] == 0x81, "invalid SimpleBlock header");
                        let rel = i16::from_be_bytes([cp[1], cp[2]]) as i64;
                        let keyframe = (cp[3] & 0x80) != 0;
                        let ts = (cluster_ts as i64 + rel) as u64;
                        parsed.blocks.push((ts, keyframe, cp[4..].to_vec()));
                    }
                }
            }
            0x1C53BB6B => {
                while pos < end {
                    let cid = read_vint_id(bytes, &mut pos)?;
                    let csz = read_vint_size(bytes, &mut pos)? as usize;
                    ensure!(cid == 0xBB, "unexpected element in Cues");
                    parsed.cue_points += 1;
                    pos += csz;
                }
            }
            _ => {
                pos = end;
            }
        }
    }
    ensure!(seek_positions.len() == 3, "expected 3 SeekHead entries");
    for (target_id, rel_offset) in seek_positions {
        let mut p = seg_start + rel_offset as usize;
        let actual_id = read_vint_id(bytes, &mut p)?;
        ensure!(actual_id == target_id, "SeekHead offset mismatch");
    }
    Ok(parsed)
}

/// Inspects a WebM file and decodes every frame using libvpx, returning the
/// number of decoded frames.
#[cfg(feature = "decoder")]
pub fn verify_webm_decodable(bytes: &[u8]) -> Result<(WebmInspection, usize)> {
    use shiguredo_libvpx as vpx;

    let info = inspect_webm(bytes)?;
    let codec = match info.codec_id.as_str() {
        "V_VP9" => vpx::DecoderCodec::Vp9,
        "V_VP8" => vpx::DecoderCodec::Vp8,
        other => anyhow::bail!("unsupported WebM codec: {other}"),
    };
    let mut decoder = vpx::Decoder::new(vpx::DecoderConfig::new(codec))?;
    let mut decoded_count = 0usize;
    for (_, _, payload) in &info.blocks {
        decoder.decode(payload)?;
        while let Some(frame) = decoder.next_frame()? {
            ensure!(
                (frame.width() as u64, frame.height() as u64)
                    == (info.pixel_width, info.pixel_height),
                "decoded dimensions mismatch"
            );
            decoded_count += 1;
        }
    }
    decoder.finish()?;
    while let Some(frame) = decoder.next_frame()? {
        ensure!(
            (frame.width() as u64, frame.height() as u64) == (info.pixel_width, info.pixel_height),
            "decoded dimensions mismatch"
        );
        decoded_count += 1;
    }
    Ok((info, decoded_count))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn webm_muxer_produces_valid_seekable_container() -> Result<()> {
        let cursor = Cursor::new(Vec::new());
        let mut muxer = WebmMuxer::new(cursor, 640, 480, 10, WebmCodec::Vp9)?;
        muxer.write_frame(1000, true, b"frame0")?;
        muxer.write_frame(1100, false, b"frame1")?;
        muxer.write_frame(2000, true, b"frame2")?;
        muxer.write_frame(40_000, false, b"frame3-forces-cluster-split")?;
        let (cursor, stats) = muxer.finish(None)?;
        assert_eq!(stats.frames, 4);
        assert_eq!(stats.duration_ms, 39_100);

        let parsed = inspect_webm(cursor.get_ref())?;
        assert_eq!(parsed.doc_type, "webm");
        assert_eq!(parsed.codec_id, "V_VP9");
        assert_eq!((parsed.pixel_width, parsed.pixel_height), (640, 480));
        assert_eq!(parsed.duration_ms, 39_100.0);
        assert_eq!(parsed.clusters, 3);
        assert_eq!(parsed.cue_points, 2);
        assert_eq!(parsed.blocks.len(), 4);
        assert_eq!(parsed.blocks[0], (0, true, b"frame0".to_vec()));
        assert_eq!(parsed.blocks[1], (100, false, b"frame1".to_vec()));
        assert_eq!(parsed.blocks[2], (1000, true, b"frame2".to_vec()));
        assert_eq!(
            parsed.blocks[3],
            (39_000, false, b"frame3-forces-cluster-split".to_vec())
        );
        Ok(())
    }

    #[cfg(all(feature = "encoder", feature = "decoder"))]
    #[test]
    fn webm_recorder_round_trips_vp9_and_vp8_frames() -> Result<()> {
        for codec in [WebmCodec::Vp9, WebmCodec::Vp8] {
            let (width, height) = (129u32, 75u32);
            let mut recorder =
                WebmRecorder::new(Cursor::new(Vec::new()), width, height, 15, codec)?;
            let mut frame = vec![0u8; width as usize * height as usize * 4];
            for idx in 0..8u64 {
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let i = (y * width as usize + x) * 4;
                        let r = ((x * 3 + idx as usize * 25) % 250) as u8;
                        let g = ((y * 5 + idx as usize * 15) % 250) as u8;
                        let b = if x < 40 && y < 30 { 220 } else { 40 };
                        frame[i..i + 4].copy_from_slice(&[b, g, r, 255]);
                    }
                }
                recorder.write_bgra_frame(idx * 67, &frame)?;
            }
            let (cursor, stats) = recorder.finish(Some(600))?;
            assert_eq!(stats.frames, 8);
            assert_eq!(stats.duration_ms, 600);

            let (parsed, decoded_count) = verify_webm_decodable(cursor.get_ref())?;
            assert_eq!(parsed.codec_id, codec.codec_id());
            assert_eq!(
                (parsed.pixel_width, parsed.pixel_height),
                (u64::from(width), u64::from(height))
            );
            assert_eq!(parsed.blocks.len(), 8);
            assert!(parsed.blocks[0].1, "first block must be a keyframe");
            assert_eq!(decoded_count, 8);
        }
        Ok(())
    }
}
