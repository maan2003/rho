//! Microphone capture and assistant-audio playback for the voice session.
//!
//! Plain rodio on the default devices, resampled here to the wire format:
//! PCM16 little-endian mono at [`rho_ui_proto::VOICE_SAMPLE_RATE`]. (The
//! zed fork's `audio` crate would add echo cancellation, but it drags in
//! libwebrtc; until then, headphones avoid the model hearing itself.)
//!
//! Capture runs on a plain thread (the mic source blocks) and pushes chunks
//! straight into the connection's outgoing channel. Playback is a shared
//! sample queue drained by an infinite rodio source, so barge-in flush is
//! one queue clear away — nothing buffered in the device layer beyond the
//! mixer's own latency.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use futures::channel::mpsc as futures_mpsc;
use rho_ui_proto::{ClientMessage, VOICE_SAMPLE_RATE};
use rodio::microphone::MicrophoneBuilder;
use rodio::source::UniformSourceIterator;
use rodio::{ChannelCount, DeviceSinkBuilder, SampleRate, Source, nz};

/// 20ms of mono PCM16 per wire frame.
const CHUNK_SAMPLES: usize = VOICE_SAMPLE_RATE as usize / 50;

fn wire_rate() -> SampleRate {
    SampleRate::new(VOICE_SAMPLE_RATE).expect("voice sample rate is nonzero")
}

pub struct VoiceAudio {
    stop: Arc<AtomicBool>,
    playback: Arc<Mutex<VecDeque<f32>>>,
    /// Keeps the output device open for the life of the session.
    _output: rodio::MixerDeviceSink,
}

impl VoiceAudio {
    /// Opens the default input and output devices and starts streaming mic
    /// chunks as [`ClientMessage::VoiceAudio`] into `commands`.
    pub fn start(commands: futures_mpsc::UnboundedSender<ClientMessage>) -> anyhow::Result<Self> {
        let mut output = DeviceSinkBuilder::open_default_sink().context("open audio output")?;
        output.log_on_drop(false);
        let playback = Arc::new(Mutex::new(VecDeque::new()));
        // The device mixer adapts sources to its own rate/channels.
        output.mixer().add(PlaybackSource {
            queue: Arc::clone(&playback),
        });

        // Open the mic on this thread so failure surfaces to the caller;
        // only the blocking read loop moves to the capture thread.
        let microphone = MicrophoneBuilder::new()
            .default_device()
            .context("no microphone available")?
            .default_config()
            .context("microphone config")?
            .prefer_sample_rates([
                wire_rate(),
                wire_rate().saturating_mul(nz!(2)),
                nz!(44100),
                nz!(16000),
            ])
            .prefer_channel_counts([nz!(1), nz!(2)])
            .prefer_buffer_sizes(512..)
            .open_stream()
            .context("open microphone")?;
        let stop = Arc::new(AtomicBool::new(false));
        let capture_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("rho-voice-mic".to_owned())
            .spawn(move || capture_loop(microphone, commands, capture_stop))
            .context("spawn microphone thread")?;

        Ok(Self {
            stop,
            playback,
            _output: output,
        })
    }

    /// Queues assistant audio (wire-format PCM16 bytes) for playback.
    pub fn play(&self, pcm: &[u8]) {
        let mut queue = self.playback.lock().expect("poison");
        queue.extend(
            pcm.chunks_exact(2)
                .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32768.0),
        );
    }

    /// Barge-in: drop everything not yet played.
    pub fn flush_playback(&self) {
        self.playback.lock().expect("poison").clear();
    }
}

impl Drop for VoiceAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn capture_loop(
    microphone: rodio::microphone::Microphone,
    commands: futures_mpsc::UnboundedSender<ClientMessage>,
    stop: Arc<AtomicBool>,
) {
    // Whatever the device produces, arrive at wire format.
    let mut samples = UniformSourceIterator::new(microphone, nz!(1), wire_rate());
    let mut chunk = Vec::with_capacity(CHUNK_SAMPLES * 2);
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Some(sample) = samples.next() else {
            // Device stream ended (unplugged, backend error): stop quietly;
            // the daemon's idle stop will end the session.
            return;
        };
        let quantized = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        chunk.extend_from_slice(&quantized.to_le_bytes());
        if chunk.len() >= CHUNK_SAMPLES * 2
            && commands
                .unbounded_send(ClientMessage::VoiceAudio {
                    pcm: std::mem::take(&mut chunk),
                })
                .is_err()
        {
            return;
        }
    }
}

/// Infinite mono source over the shared queue; silence when empty, so the
/// mixer keeps running and latency stays at the queue depth.
struct PlaybackSource {
    queue: Arc<Mutex<VecDeque<f32>>>,
}

impl Iterator for PlaybackSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        Some(
            self.queue
                .lock()
                .expect("poison")
                .pop_front()
                .unwrap_or(0.0),
        )
    }
}

impl Source for PlaybackSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        nz!(1)
    }

    fn sample_rate(&self) -> SampleRate {
        wire_rate()
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}
