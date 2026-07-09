//! One-shot attention chime: the zed fork's bundled `agent_done.wav`,
//! played when an agent enters the user's court (>= Pending).
//!
//! The output device opens lazily on the first chime and stays open, like
//! zed's own sound pipeline — reopening per chime risks cutting the tail
//! off when the sink drops. A missing or failing audio device downgrades
//! to silence; a lamp the user can't hear is still a lamp.

use std::io::Cursor;

use anyhow::Context as _;
use gpui::AssetSource as _;
use rodio::source::Buffered;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Source as _};

#[derive(Default)]
pub struct Chime {
    /// `Some(None)` after a failed open, so we don't retry every event.
    output: Option<Option<Output>>,
}

struct Output {
    sink: MixerDeviceSink,
    sound: Buffered<Decoder<Cursor<Vec<u8>>>>,
}

impl Chime {
    pub fn play(&mut self) {
        let output = self.output.get_or_insert_with(|| match open() {
            Ok(output) => Some(output),
            Err(error) => {
                eprintln!("rho-gui: attention chime disabled: {error:#}");
                None
            }
        });
        if let Some(output) = output {
            output.sink.mixer().add(output.sound.clone());
        }
    }
}

fn open() -> anyhow::Result<Output> {
    let bytes = assets::Assets
        .load("sounds/agent_done.wav")
        .context("load chime asset")?
        .context("chime asset missing")?
        .into_owned();
    let sound = Decoder::new(Cursor::new(bytes))
        .context("decode chime")?
        .buffered();
    let mut sink = DeviceSinkBuilder::open_default_sink().context("open audio output")?;
    sink.log_on_drop(false);
    Ok(Output { sink, sound })
}
