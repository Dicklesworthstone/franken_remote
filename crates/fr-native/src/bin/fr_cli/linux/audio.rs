//! `fr connect --view-only --audio`: host playback into the selected local
//! `PulseAudio` output. Host Opus is decoded only in the restricted per-epoch
//! fr-opus-worker, never on the session thread or by an in-process fallback.
//! Configuration acknowledgement waits for BOTH real native owners. PCM replies
//! retain their original arrival/device deadlines; a receipt is not audibility.
use super::super::{Failure, options::AudioRequest};
use std::path::PathBuf;

/// Content-free outcome for the completion record.
#[derive(Debug, Default)]
pub(super) struct Report {
    /// The host configured a stream and this client acknowledged it.
    pub acknowledged: bool,
    /// Decoded frames the local audio server accepted (not audibility).
    pub submitted: u64,
    /// Local output resets, each followed by a fresh host epoch.
    pub resets: u64,
    /// First typed reason audio was absent or ended.
    pub absence: Option<&'static str>,
}
impl Report {
    pub(super) fn active(&self) -> bool {
        self.acknowledged && self.submitted > 0
    }
    pub(super) fn absent(&mut self, reason: &'static str) {
        self.absence.get_or_insert(reason);
    }
}

/// The view-only offer: the optional audio-down capability only with `--audio`.
pub(super) fn observation_offer(audio: bool) -> fr_wire::negotiation::Offer {
    if audio {
        fr_client::native::observation_offer_with_audio()
    } else {
        fr_client::native::observation_offer()
    }
}

/// An explicit local request only: a build without playback and a missing
/// local output socket are typed refusals before any connection attempt.
pub(super) fn resolve(
    request: Option<&AudioRequest>,
) -> Result<Option<(PathBuf, AudioRequest)>, Failure> {
    let Some(request) = request else {
        return Ok(None);
    };
    if !cfg!(feature = "linux-audio") {
        return Err(Failure::new(
            "audio_unavailable_in_build",
            "This fr was built without the linux-audio feature; rebuild with --features linux-desktop,linux-audio or omit --audio.",
            2,
        ));
    }
    decoder_image()?;
    Ok(Some((server(request)?, request.clone())))
}

/// The codec image is installed beside fr, never selected by a host record,
/// environment search path or downloaded on demand. Missing is a typed refusal.
fn decoder_image() -> Result<PathBuf, Failure> {
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(|p| p.join("fr-opus-worker")))
        .filter(|p| p.is_absolute() && p.is_file())
        .ok_or_else(|| Failure::new("audio_decoder_unavailable",
            "--audio requires the matching fr-opus-worker installed beside fr; build it with --features linux-opus-process. No in-process decoder fallback was selected.", 2))
}

/// Resolve the local server socket once, before dialing: flag, then
/// `PULSE_SERVER`, then the user runtime dir. Missing is a typed refusal.
fn server(request: &AudioRequest) -> Result<PathBuf, Failure> {
    let server = request.server.clone().or_else(|| {
        std::env::var("PULSE_SERVER")
            .ok()
            .map(|v| PathBuf::from(v.strip_prefix("unix:").unwrap_or(&v)))
            .or_else(|| {
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map(|dir| PathBuf::from(dir).join("pulse").join("native"))
            })
    });
    server
        .filter(|p| p.is_absolute() && p.exists())
        .ok_or_else(|| {
            Failure::new(
                "audio_output_unavailable",
                "--audio needs the local PulseAudio server socket: pass --audio-server /absolute/path/native (or set PULSE_SERVER / XDG_RUNTIME_DIR), or omit --audio.",
                2,
            )
        })
}

#[cfg(feature = "linux-audio")]
pub(super) use output::Output;
#[cfg(feature = "linux-audio")]
mod output {
    use super::Report;
    use fr_client::input::ClientInstant;
    use fr_core::audio::{AudioStopReason, AudioStreamConfig};
    use fr_native::opus::{
        CodecLimits,
        playout::ReceiveResult,
        process::{ProcessDecoder, Retirement},
    };
    use fr_native::pulse::{
        Error as DeviceError, PlaybackDevice, Selection, State,
        playout::{PulsePlayout, RenderResult},
    };
    use fr_wire::audio::AudioConfiguration;
    use frd::session_startup::{ViewerAudioEnd, ViewerAudioOutput, ViewerAudioRefused};
    use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Instant};

    /// At most this many decode/render steps per service turn.
    const RENDERS_PER_TURN: usize = 4;

    enum Stage {
        Idle,
        Starting {
            device: PlaybackDevice,
            offer: AudioConfiguration,
            binding: u32,
        },
        Configuring(PulsePlayout<ProcessDecoder>),
        Playing(PulsePlayout<ProcessDecoder>),
        Stopping(PulsePlayout<ProcessDecoder>),
        Done,
    }
    pub struct Output {
        image: Option<PathBuf>,
        retirement: Option<Retirement>,
        server: PathBuf,
        sink: Option<String>,
        origin: Instant,
        stage: Stage,
        report: Rc<RefCell<Report>>,
    }
    impl Output {
        pub fn new(server: PathBuf, sink: Option<String>, report: Rc<RefCell<Report>>) -> Self {
            Self {
                image: super::decoder_image().ok(),
                retirement: None,
                server,
                sink,
                origin: Instant::now(),
                stage: Stage::Idle,
                report,
            }
        }
    }
    fn now(origin: Instant) -> ClientInstant {
        ClientInstant(u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX))
    }
    fn end_reason(end: ViewerAudioEnd) -> &'static str {
        match end {
            ViewerAudioEnd::Host(AudioStopReason::HostDisabled) => "host_audio_unavailable",
            ViewerAudioEnd::Host(AudioStopReason::DeviceChanged) => "host_audio_device_changed",
            ViewerAudioEnd::Host(AudioStopReason::SessionEnded) => "host_audio_session_ended",
            ViewerAudioEnd::Host(AudioStopReason::BufferOverflow) => "host_audio_overflow",
            ViewerAudioEnd::Host(AudioStopReason::UserMute) => "host_audio_stopped",
            ViewerAudioEnd::NoOutput => "local_output_not_configured",
            ViewerAudioEnd::Local => "local_output_failed",
        }
    }
    impl ViewerAudioOutput for Output {
        fn configure(
            &mut self,
            binding: u32,
            offer: AudioConfiguration,
        ) -> Result<(), ViewerAudioRefused> {
            if !matches!(self.stage, Stage::Idle)
                || self.retirement.as_ref().is_some_and(|r| !r.is_complete())
                || self.image.is_none()
            {
                return Err(ViewerAudioRefused);
            }
            let config = AudioStreamConfig::new(
                offer.direction,
                offer.generation,
                offer.channels,
                offer.frame_duration_ms,
                offer.jitter_target_ms,
            )
            .map_err(|_| ViewerAudioRefused)?;
            let selection = match &self.sink {
                Some(sink) => Selection::new(&self.server, sink),
                None => Selection::default_output(&self.server),
            }
            .map_err(|_| ViewerAudioRefused)?;
            let device = PlaybackDevice::connect(selection, config, now(self.origin))
                .map_err(|_| ViewerAudioRefused)?;
            self.stage = Stage::Starting {
                device,
                offer,
                binding,
            };
            Ok(())
        }
        fn service(
            &mut self,
            live: &mut dyn FnMut() -> bool,
            acknowledge: &mut dyn FnMut(&[u8]) -> Result<(), ViewerAudioRefused>,
        ) -> Result<(), ViewerAudioRefused> {
            let origin = self.origin;
            let mut checkpoint = || {
                if live() {
                    Ok(now(origin))
                } else {
                    Err(DeviceError::Denied)
                }
            };
            match std::mem::replace(&mut self.stage, Stage::Done) {
                Stage::Starting {
                    mut device,
                    offer,
                    binding,
                } => {
                    let state = device
                        .poll(&mut checkpoint)
                        .map_err(|_| ViewerAudioRefused)?;
                    if state != State::Ready {
                        self.stage = Stage::Starting {
                            device,
                            offer,
                            binding,
                        };
                        return Ok(());
                    }
                    let limits = CodecLimits::new(
                        usize::try_from(offer.max_packet_bytes).map_err(|_| ViewerAudioRefused)?,
                        offer.max_decoded_samples,
                    )
                    .map_err(|_| ViewerAudioRefused)?;
                    let (decoder, retirement) = ProcessDecoder::new(
                        self.image.as_deref().ok_or(ViewerAudioRefused)?,
                        limits,
                    )
                    .map_err(|_| ViewerAudioRefused)?;
                    self.retirement = Some(retirement);
                    let playout = PulsePlayout::with_decoder(
                        binding,
                        offer,
                        device,
                        &mut checkpoint,
                        decoder,
                    )
                    .map_err(|_| ViewerAudioRefused)?;
                    self.stage = Stage::Configuring(playout);
                }
                Stage::Configuring(mut playout) => {
                    if !playout
                        .poll_ready(&mut checkpoint)
                        .map_err(|_| ViewerAudioRefused)?
                    {
                        self.stage = Stage::Configuring(playout);
                        return Ok(());
                    }
                    playout
                        .acknowledge(&mut checkpoint, |record| {
                            acknowledge(record).map_err(|_| ())
                        })
                        .map_err(|_| ViewerAudioRefused)?;
                    self.report.borrow_mut().acknowledged = true;
                    self.stage = Stage::Playing(playout);
                }
                Stage::Playing(mut playout) => {
                    playout
                        .service(&mut checkpoint)
                        .map_err(|_| ViewerAudioRefused)?;
                    for _ in 0..RENDERS_PER_TURN {
                        match playout
                            .render(&mut checkpoint)
                            .map_err(|_| ViewerAudioRefused)?
                        {
                            RenderResult::Submitted(_) => {
                                let mut report = self.report.borrow_mut();
                                report.submitted = report.submitted.saturating_add(1);
                            }
                            RenderResult::Waiting => break,
                        }
                    }
                    self.stage = Stage::Playing(playout);
                }
                Stage::Stopping(mut playout) => {
                    let origin = self.origin;
                    // Cleanup only: fresh local time, not a renewed grant.
                    if playout.poll_stop(|| Ok(now(origin))).ok() != Some(State::Closed)
                        || self.retirement.as_ref().is_some_and(|r| !r.is_complete())
                    {
                        self.stage = Stage::Stopping(playout);
                    }
                }
                stage => self.stage = stage,
            }
            Ok(())
        }
        fn receive(
            &mut self,
            bytes: &[u8],
            live: &mut dyn FnMut() -> bool,
        ) -> Result<(), ViewerAudioRefused> {
            let origin = self.origin;
            let mut checkpoint = || {
                if live() {
                    Ok(now(origin))
                } else {
                    Err(DeviceError::Denied)
                }
            };
            match std::mem::replace(&mut self.stage, Stage::Done) {
                Stage::Playing(mut playout) => {
                    match playout
                        .receive_record(bytes, &mut checkpoint)
                        .map_err(|_| ViewerAudioRefused)?
                    {
                        ReceiveResult::Stopped(_) => self.stage = Stage::Stopping(playout),
                        ReceiveResult::Queued | ReceiveResult::Ignored => {
                            self.stage = Stage::Playing(playout);
                        }
                    }
                }
                Stage::Configuring(mut playout) => {
                    playout.disconnect();
                }
                Stage::Starting { mut device, .. } => {
                    // Only a matching stop reaches us before acknowledgement.
                    device.disconnect();
                }
                stage => self.stage = stage,
            }
            Ok(())
        }
        fn reset(&mut self) {
            // Retain the exact retirement until proven complete. A new epoch
            // cannot overlap its failed decoder, even if negotiation restarts.
            if let Some(retirement) = &self.retirement {
                retirement.stop();
            }
            self.stage = Stage::Idle;
            let mut report = self.report.borrow_mut();
            report.resets = report.resets.saturating_add(1);
        }
        fn ended(&mut self, end: ViewerAudioEnd) {
            self.report.borrow_mut().absent(end_reason(end));
            match std::mem::replace(&mut self.stage, Stage::Done) {
                Stage::Playing(mut playout) | Stage::Configuring(mut playout) => {
                    if playout.stop(now(self.origin)).is_ok() {
                        self.stage = Stage::Stopping(playout);
                    }
                }
                Stage::Starting { mut device, .. } => device.disconnect(),
                stage => self.stage = stage,
            }
        }
    }
    #[cfg(test)]
    mod tests;

    impl Drop for Output {
        fn drop(&mut self) {
            if let Some(retirement) = &self.retirement {
                retirement.stop();
            }
        }
    }
}

/// Build the output for one attempt; `None` when this build has no audio.
#[cfg(feature = "linux-audio")]
pub(super) fn output(
    server: PathBuf,
    request: &AudioRequest,
    report: &std::rc::Rc<std::cell::RefCell<Report>>,
) -> Box<dyn frd::session_startup::ViewerAudioOutput> {
    Box::new(Output::new(server, request.sink.clone(), report.clone()))
}
