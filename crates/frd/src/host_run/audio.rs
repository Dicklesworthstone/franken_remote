//! `frd run --audio`: the operator's LOCAL playback-audio enable (plan §15.4).
//! Off by default. It arms a demand-driven `fr-media-worker --audio` child per
//! share; frd itself links neither libpulse nor libopus. Nothing here is
//! peer-selectable, approval, or capture freshness.
use super::{Error, Options};
use crate::{
    media::shared_publisher::AudioProfile, session_startup::shared_viewers, worker::Retirement,
};
use fr_media::worker::audio::{Monitor, Source};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Explicit local playback scope. It is independent of the selected display
/// and never authorizes microphone capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioOptions {
    Pulse {
        /// Absolute path of the local `PulseAudio` server's UNIX socket.
        server: PathBuf,
        /// The selected sink, or the default resolved once at capture start.
        sink: Option<String>,
    },
    /// `ScreenCaptureKit` application/system playback, not display isolation.
    MacSystem,
}
impl AudioOptions {
    fn source(&self) -> Result<Source, Error> {
        match self {
            Self::Pulse { server, sink } if cfg!(target_os = "linux") && server.is_absolute() => {
                Ok(Source::Pulse {
                    server: server.to_str().ok_or(Error::Configuration)?.to_owned(),
                    monitor: sink.clone().map_or(Monitor::DefaultSink, Monitor::Sink),
                })
            }
            Self::MacSystem if cfg!(target_os = "macos") => Ok(Source::MacSystem),
            _ => Err(Error::Configuration),
        }
    }
}

/// Validate before any runtime/listener exists: a bad path or sink name is a
/// configuration refusal, never a silent fallback to "no audio".
pub(super) fn check(options: &Options) -> Result<(), Error> {
    let Some(audio) = &options.audio else {
        return Ok(());
    };
    profile(options, audio, Arc::new(Mutex::new(None))).map(drop)
}

pub(super) fn profile(
    options: &Options,
    audio: &AudioOptions,
    retired: Arc<Mutex<Option<Retirement>>>,
) -> Result<AudioProfile, Error> {
    let entropy: shared_viewers::Entropy =
        Arc::new(|| super::random_nonzero_u128().map_err(|_| ()));
    AudioProfile::with_source(
        options.worker.clone(),
        options.display.clone(),
        audio.source()?,
        entropy,
        retired,
    )
    .map_err(|_| Error::Configuration)
}
