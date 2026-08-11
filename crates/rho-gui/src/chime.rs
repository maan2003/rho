//! One-shot attention chime: the zed fork's bundled `agent_done.wav`,
//! played when an agent enters the user's court (>= Pending).
//!
//! The output device is opened for each chime and closed after its tail. Keeping
//! an idle CPAL stream alive makes some ALSA devices spin an output thread.

use std::io::Cursor;
use std::time::Duration;

use anyhow::Context as _;
use gpui::AssetSource as _;
use rodio::{Decoder, DeviceSinkBuilder, Source as _};

#[derive(Default)]
pub struct Chime;

impl Chime {
    pub fn play(&mut self) {
        if let Err(error) = std::thread::Builder::new()
            .name("rho-chime".into())
            .spawn(|| {
                if let Err(error) = play_once() {
                    eprintln!("rho-gui: attention chime disabled: {error:#}");
                }
            })
        {
            eprintln!("rho-gui: attention chime disabled: {error:#}");
        }
    }
}

fn play_once() -> anyhow::Result<()> {
    let bytes = assets::Assets
        .load("sounds/agent_done.wav")
        .context("load chime asset")?
        .context("chime asset missing")?
        .into_owned();
    let sound = Decoder::new(Cursor::new(bytes)).context("decode chime")?;
    let duration = sound.total_duration().unwrap_or(Duration::from_secs(1));
    let mut sink = DeviceSinkBuilder::open_default_sink().context("open audio output")?;
    sink.log_on_drop(false);
    sink.mixer().add(sound);
    std::thread::sleep(duration + Duration::from_millis(100));
    Ok(())
}
