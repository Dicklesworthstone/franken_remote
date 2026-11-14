//! `fr connect --audio`: host playback in either viewing role into the selected local
//! native output. Host Opus is decoded only in the separate per-epoch
//! fr-opus-worker, never on the session thread or by an in-process fallback.
//! Configuration acknowledgement waits for BOTH real native owners. PCM replies
//! retain their original arrival/device deadlines; a receipt is not audibility.
//! The CLI uses a bounded output worker: no device call or native teardown
//! runs on the session thread. Linux requires seccomp; macOS retains a private
//! same-user child with parent-exit binding, without an OS sandbox claim.
use super::super::{Failure, options::AudioRequest};
use std::path::PathBuf;

/// Content-free outcome for the completion record.
#[derive(Debug, Default)]
pub(super) struct Report {
    /// Exact local preference used by the actual output and optional panel.
    pub volume: fr_client::audio::AudioVolumeControl,
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    pub controls:
        Option<Result<fr_native::file_progress::Control, fr_native::file_progress::Error>>,
    /// The host configured a stream and this client acknowledged it.
    pub acknowledged: bool,
    /// Decoded frames the local audio server accepted (not audibility).
    pub submitted: u64,
    /// Valid packets discarded before worker custody because its fixed inbox
    /// was full. Not codec loss, concealment, submitted PCM or a failed epoch.
    pub dropped_packets: u64,
    /// Local output resets, each followed by a fresh host epoch.
    pub resets: u64,
    /// Output generations retired by the original audio freshness deadline.
    pub expired_generations: u64,
    /// Output generations retired for another native/worker failure.
    pub failed_generations: u64,
    /// Prior retired generations followed by acknowledged, submitted fresh PCM.
    pub recovered_generations: u64,
    /// First non-freshness output failure, retained even if playback recovers.
    pub first_failure: Option<&'static str>,
    /// Typed reason audio is absent or ended; a proven replacement can clear it.
    pub absence: Option<&'static str>,
    /// Lifetime submissions at the start of the current output generation.
    pub generation_start: u64,
}
impl Report {
    pub(super) fn for_request(request: &AudioRequest) -> Self {
        Self {
            volume: fr_client::audio::AudioVolumeControl::from_parts(
                f32::from(request.volume) / 100.0,
                request.muted,
            ),
            ..Self::default()
        }
    }
    /// Retain the latest local preference across reconnects, without allowing
    /// an old panel or output handle to change the next attempt's preference.
    #[cfg(any(target_os = "linux", feature = "macos-audio", test))]
    pub(super) fn begin_attempt(&mut self) -> fr_client::audio::AudioVolumeControl {
        let (volume, muted) = self.volume.snapshot();
        self.volume = fr_client::audio::AudioVolumeControl::from_parts(volume, muted);
        #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
        {
            self.controls = None;
        }
        self.acknowledged = false;
        self.generation_start = self.submitted;
        self.volume.clone()
    }
    pub(super) fn active(&self) -> bool {
        self.acknowledged && self.submitted > self.generation_start && self.absence.is_none()
    }
    pub(super) fn absent(&mut self, reason: &'static str) {
        self.absence.get_or_insert(reason);
    }
    #[cfg(any(feature = "linux-audio", feature = "macos-audio", test))]
    fn begin_generation(&mut self) {
        self.acknowledged = false;
        self.generation_start = self.submitted;
    }
    #[cfg(any(feature = "linux-audio", feature = "macos-audio", test))]
    fn failed_generation(&mut self, reason: &'static str, expired: bool) {
        if expired {
            self.expired_generations = self.expired_generations.saturating_add(1);
        } else {
            self.failed_generations = self.failed_generations.saturating_add(1);
            self.first_failure.get_or_insert(reason);
        }
        self.acknowledged = false;
        self.absence = Some(reason);
    }
    /// Called only for the current, fault-free worker. Configuration, silence,
    /// old-generation submissions and cleanup cannot count as recovery.
    #[cfg(any(feature = "linux-audio", feature = "macos-audio", test))]
    fn recovered(&mut self) {
        if self.acknowledged && self.submitted > self.generation_start {
            self.recovered_generations = self
                .expired_generations
                .saturating_add(self.failed_generations);
            self.absence = None;
        }
    }
}

#[cfg(test)]
#[path = "audio/report_tests.rs"]
mod report_tests;

/// The view-only offer: the optional audio-down capability only with `--audio`.
#[cfg(target_os = "linux")]
pub(super) fn observation_offer(audio: bool) -> fr_wire::negotiation::Offer {
    if audio {
        fr_client::native::observation_offer_with_audio()
    } else {
        fr_client::native::observation_offer()
    }
}

/// An explicit local request only: a build without playback and a missing
/// local output socket are typed refusals before any connection attempt.
#[cfg(target_os = "linux")]
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
pub(super) fn decoder_image() -> Result<PathBuf, Failure> {
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(|p| p.join("fr-opus-worker")))
        .filter(|p| p.is_absolute() && p.is_file())
        .ok_or_else(|| Failure::new("audio_decoder_unavailable",
            "--audio requires the matching fr-opus-worker installed beside fr; build it with the native Opus process feature. No in-process decoder fallback was selected.", 2))
}

/// Resolve the local server socket once, before dialing: flag, then
/// `PULSE_SERVER`, then the user runtime dir. Missing is a typed refusal.
#[cfg(target_os = "linux")]
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

#[cfg(any(feature = "linux-audio", feature = "macos-audio"))]
pub(super) use output::threaded::Output;
#[cfg(all(target_os = "macos", feature = "macos-audio"))]
pub(super) use output::threaded::Retirement;
#[cfg(any(feature = "linux-audio", feature = "macos-audio"))]
mod output {
    pub(super) mod threaded;
    use super::Report;
    use fr_client::audio::AudioVolumeControl;
    use fr_client::input::ClientInstant;
    use fr_core::audio::{AudioStopReason, AudioStreamConfig};
    use fr_native::opus::{
        CodecLimits,
        playout::ReceiveResult,
        process::{ProcessDecoder, Retirement},
    };
    #[cfg(target_os = "linux")]
    use fr_native::pulse::{
        Error as DeviceError, PlaybackDevice, Selection, State,
        playout::{Error as PlayoutError, PulsePlayout, RenderResult},
    };
    #[cfg(target_os = "macos")]
    use fr_native::{
        macos_audio::{Error as DeviceError, PlaybackDevice, Selection, State},
        macos_audio_playout::{Error as PlayoutError, NativePlayout as PulsePlayout, RenderResult},
    };
    use fr_wire::audio::AudioConfiguration;
    use frd::session_startup::{ViewerAudioEnd, ViewerAudioOutput, ViewerAudioRefused};
    use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Instant};

    /// At most this many decode/render steps per service turn.
    const RENDERS_PER_TURN: usize = 4;

    /// Locally selected device, before any native output is opened. The Mac
    /// subset pins its current default output; Linux retains its named socket.
    #[derive(Clone)]
    pub(super) struct Selected {
        #[cfg(target_os = "linux")]
        server: PathBuf,
        #[cfg(target_os = "linux")]
        sink: Option<String>,
    }
    impl Selected {
        #[cfg_attr(target_os = "macos", allow(clippy::unused_self))] // Mac's default-only selection carries no unqualified device name.
        fn open(&self) -> Result<Selection, DeviceError> {
            #[cfg(target_os = "linux")]
            {
                match &self.sink {
                    Some(sink) => Selection::new(&self.server, sink),
                    None => Selection::default_output(&self.server),
                }
            }
            #[cfg(target_os = "macos")]
            {
                Selection::default_output()
            }
        }
    }

    /// Preserve the native cause across the session trait's content-free refusal.
    /// Expiring old sound is a freshness refusal, not a broken output device.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Failure {
        Expired,
        Native,
    }
    impl From<PlayoutError> for Failure {
        fn from(error: PlayoutError) -> Self {
            use fr_client::audio::{jitter::JitterError, playout::PlayoutError as Playback};
            use fr_native::opus::playout::Error as ReceiveError;
            match error {
                // Each enforces the freshness contract: this sound, or the
                // timeline it continues, outran its window and is never played.
                // A gap or stall longer than the concealment ceiling ends here.
                PlayoutError::Device(DeviceError::Expired)
                | PlayoutError::Receiver(ReceiveError::Playout(
                    Playback::Expired
                    | Playback::MissedDeviceSlot
                    | Playback::Jitter(
                        JitterError::WindowExceeded | JitterError::ConcealmentExhausted,
                    ),
                )) => Self::Expired,
                _ => Self::Native,
            }
        }
    }

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
        #[cfg(target_os = "macos")]
        device_retirement: Option<fr_native::macos_audio::Retirement>,
        selected: Selected,
        origin: Instant,
        stage: Stage,
        failure: Option<Failure>,
        report: Rc<RefCell<Report>>,
        volume: AudioVolumeControl,
    }
    impl Output {
        #[cfg(all(test, target_os = "linux"))]
        pub fn new(
            server: PathBuf,
            sink: Option<String>,
            report: Rc<RefCell<Report>>,
            volume: AudioVolumeControl,
        ) -> Self {
            Self::selected(Selected { server, sink }, report, volume)
        }
        fn selected(
            selected: Selected,
            report: Rc<RefCell<Report>>,
            volume: AudioVolumeControl,
        ) -> Self {
            Self {
                image: super::decoder_image().ok(),
                retirement: None,
                #[cfg(target_os = "macos")]
                device_retirement: None,
                selected,
                origin: Instant::now(),
                stage: Stage::Idle,
                failure: None,
                report,
                volume,
            }
        }
    }
    impl Output {
        fn refuse(&mut self, error: PlayoutError) -> ViewerAudioRefused {
            self.failure.get_or_insert(error.into());
            ViewerAudioRefused
        }
        fn receive_at(
            &mut self,
            bytes: &[u8],
            arrived: ClientInstant,
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
                        .receive_record_at(bytes, arrived, &mut checkpoint)
                        .map_err(|error| self.refuse(error))?
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
            ViewerAudioEnd::ReceiveExpired => "audio_receive_expired",
            ViewerAudioEnd::SendExpired => "audio_send_expired",
            ViewerAudioEnd::ChannelClosed => "audio_channel_closed",
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
            let selection = self.selected.open().map_err(|_| ViewerAudioRefused)?;
            #[cfg(target_os = "linux")]
            let device = PlaybackDevice::connect(selection, config, now(self.origin))
                .map_err(|_| ViewerAudioRefused)?;
            #[cfg(target_os = "macos")]
            let device = {
                let (device, retirement) =
                    PlaybackDevice::connect_retained(selection, config, now(self.origin));
                // Partial native setup can still own an unresolved queue.
                // Retain its cleanup witness before propagating an open error.
                self.device_retirement = Some(retirement);
                device.map_err(|_| ViewerAudioRefused)?
            };
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
                        .map_err(|error| self.refuse(PlayoutError::Device(error)))?;
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
                    let mut playout = PulsePlayout::with_decoder(
                        binding,
                        offer,
                        device,
                        &mut checkpoint,
                        decoder,
                    )
                    .map_err(|error| self.refuse(error))?;
                    *playout.volume_mut() = self.volume.clone();
                    self.stage = Stage::Configuring(playout);
                }
                Stage::Configuring(mut playout) => {
                    if !playout
                        .poll_ready(&mut checkpoint)
                        .map_err(|error| self.refuse(error))?
                    {
                        self.stage = Stage::Configuring(playout);
                        return Ok(());
                    }
                    playout
                        .acknowledge(&mut checkpoint, |record| {
                            acknowledge(record).map_err(|_| ())
                        })
                        .map_err(|error| self.refuse(error))?;
                    self.report.borrow_mut().acknowledged = true;
                    self.stage = Stage::Playing(playout);
                }
                Stage::Playing(mut playout) => {
                    playout
                        .service(&mut checkpoint)
                        .map_err(|error| self.refuse(error))?;
                    for _ in 0..RENDERS_PER_TURN {
                        match playout
                            .render(&mut checkpoint)
                            .map_err(|error| self.refuse(error))?
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
            self.receive_at(bytes, now(self.origin), live)
        }
        fn reset(&mut self) {
            // Retain the exact retirement until proven complete. A new epoch
            // cannot overlap its failed decoder, even if negotiation restarts.
            if let Some(retirement) = &self.retirement {
                retirement.stop();
            }
            self.stage = Stage::Idle;
            self.failure = None;
            let mut report = self.report.borrow_mut();
            report.resets = report.resets.saturating_add(1);
            report.begin_generation();
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
    #[cfg(all(test, target_os = "linux"))]
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
#[cfg(all(target_os = "linux", feature = "linux-audio"))]
pub(super) fn output(
    server: PathBuf,
    request: &AudioRequest,
    report: &std::rc::Rc<std::cell::RefCell<Report>>,
    volume: fr_client::audio::AudioVolumeControl,
) -> Box<dyn frd::session_startup::ViewerAudioOutput> {
    Box::new(Output::new(
        server,
        request.sink.clone(),
        report.clone(),
        volume,
    ))
}

/// The same bounded proxy and decoder ownership, selecting only the Mac's
/// current local default output. The caller retains the real cleanup witness.
#[cfg(all(target_os = "macos", feature = "macos-audio"))]
pub(super) fn macos_output(
    report: &std::rc::Rc<std::cell::RefCell<Report>>,
    volume: fr_client::audio::AudioVolumeControl,
) -> (Box<dyn frd::session_startup::ViewerAudioOutput>, Retirement) {
    let output = Output::macos(report.clone(), volume);
    let retirement = output.retirement();
    (Box::new(output), retirement)
}
