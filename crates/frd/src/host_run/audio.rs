//! `frd run --audio`: the operator's LOCAL playback-audio enable (plan §15.4).
//! Off by default. It arms a demand-driven `fr-media-worker --audio` child per
//! share; frd itself links neither libpulse nor libopus. Nothing here is
//! peer-selectable, approval, or capture freshness.
use super::{Error, Options};
use crate::{
    media::shared_publisher::AudioProfile, session_startup::shared_viewers, worker::Retirement,
};
use fr_media::worker::audio::Monitor;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// The locally selected audio server and playback monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOptions {
    /// Absolute path of the local `PulseAudio` server's UNIX socket.
    pub server: PathBuf,
    /// Sink whose monitor is captured; `None` is the server's default sink,
    /// resolved once when capture starts and pinned there.
    pub sink: Option<String>,
}
impl AudioOptions {
    fn monitor(&self) -> Monitor {
        self.sink
            .clone()
            .map_or(Monitor::DefaultSink, Monitor::Sink)
    }
}

/// Validate before any runtime/listener exists: a bad path or sink name is a
/// configuration refusal, never a silent fallback to "no audio".
pub(super) fn check(options: &Options) -> Result<(), Error> {
    let Some(audio) = &options.audio else {
        return Ok(());
    };
    if !audio.server.is_absolute() {
        return Err(Error::Configuration);
    }
    profile(options, audio, Arc::new(Mutex::new(None))).map(drop)
}

pub(super) fn profile(
    options: &Options,
    audio: &AudioOptions,
    retired: Arc<Mutex<Option<Retirement>>>,
) -> Result<AudioProfile, Error> {
    let server = audio
        .server
        .to_str()
        .ok_or(Error::Configuration)?
        .to_owned();
    let entropy: shared_viewers::Entropy =
        Arc::new(|| super::random_nonzero_u128().map_err(|_| ()));
    AudioProfile::new(
        options.worker.clone(),
        options.display.clone(),
        server,
        audio.monitor(),
        entropy,
        retired,
    )
    .map_err(|_| Error::Configuration)
}
