#![forbid(unsafe_code)]
//! Native device/decoder work, owned exclusively by the output worker thread.
#[path = "../bin/fr_cli/linux/output/threaded.rs"]
pub(super) mod threaded;
use super::Report;
use crate::opus::{
    CodecLimits,
    playout::ReceiveResult,
    process::{ProcessDecoder, Retirement},
};
#[cfg(target_os = "linux")]
use crate::pulse::{
    Error as DeviceError, PlaybackDevice, Selection, State,
    playout::{Error as PlayoutError, PulsePlayout, RenderResult},
};
#[cfg(target_os = "macos")]
use crate::{
    macos_audio::{
        Error as DeviceError, PlaybackDevice, Retirement as DeviceRetirement, Selection, State,
    },
    macos_audio_playout::{Error as PlayoutError, NativePlayout as PulsePlayout, RenderResult},
};
#[cfg(windows)]
use crate::{
    windows_audio::{
        Error as DeviceError, PlaybackDevice, Retirement as DeviceRetirement, Selection, State,
    },
    windows_audio_playout::{Error as PlayoutError, NativePlayout as PulsePlayout, RenderResult},
};
use fr_client::audio::AudioVolumeControl;
use fr_client::audio::output::{
    AudioEnd as ViewerAudioEnd, AudioOutput as ViewerAudioOutput,
    OutputRefused as ViewerAudioRefused,
};
use fr_client::input::ClientInstant;
use fr_core::audio::{AudioStopReason, AudioStreamConfig};
use fr_wire::audio::AudioConfiguration;
use std::{cell::RefCell, path::PathBuf, rc::Rc};

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
    #[cfg_attr(any(target_os = "macos", windows), allow(clippy::unused_self))] // Mac's default-only selection carries no unqualified device name.
    fn open(&self) -> Result<Selection, DeviceError> {
        #[cfg(target_os = "linux")]
        {
            match &self.sink {
                Some(sink) => Selection::new(&self.server, sink),
                None => Selection::default_output(&self.server),
            }
        }
        #[cfg(any(target_os = "macos", windows))]
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
        use crate::opus::playout::Error as ReceiveError;
        use fr_client::audio::{jitter::JitterError, playout::PlayoutError as Playback};
        match error {
            // Each enforces the freshness contract: this sound, or the
            // timeline it continues, outran its window and is never played.
            // A gap or stall longer than the concealment ceiling ends here.
            PlayoutError::Device(DeviceError::Expired)
            | PlayoutError::Receiver(ReceiveError::Playout(
                Playback::Expired
                | Playback::MissedDeviceSlot
                | Playback::Jitter(JitterError::WindowExceeded | JitterError::ConcealmentExhausted),
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
    #[cfg(any(target_os = "macos", windows))]
    device_retirement: Option<DeviceRetirement>,
    selected: Selected,
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
            image: super::decoder_image(),
            retirement: None,
            #[cfg(any(target_os = "macos", windows))]
            device_retirement: None,
            selected,
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
        let mut checkpoint = || {
            if live() {
                Ok(now())
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
// Session receipt, worker custody and native playout share the same local boot
// clock. An unavailable reading expires existing bounds and cannot form a
// new checked deadline.
fn now() -> ClientInstant {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let micros = crate::clock::suspend_inclusive_ns().map_or(u64::MAX, |ns| ns / 1000);
    #[cfg(windows)]
    let micros = crate::windows_audio::playback_now_us();
    ClientInstant(micros)
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
        let device =
            PlaybackDevice::connect(selection, config, now()).map_err(|_| ViewerAudioRefused)?;
        #[cfg(any(target_os = "macos", windows))]
        let device = {
            let (device, retirement) = PlaybackDevice::connect_retained(selection, config, now());
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
        self.service_with_ack(live, &mut |record, _| acknowledge(record))
    }
    fn receive(
        &mut self,
        bytes: &[u8],
        live: &mut dyn FnMut() -> bool,
    ) -> Result<(), ViewerAudioRefused> {
        self.receive_at(bytes, now(), live)
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
                if playout.stop(now()).is_ok() {
                    self.stage = Stage::Stopping(playout);
                }
            }
            Stage::Starting { mut device, .. } => device.disconnect(),
            stage => self.stage = stage,
        }
    }
}
// This callback stays on the same native worker and retains the original
// device, permission checkpoint and setup deadline throughout the ACK wait.
type AckProgress<'a> = dyn FnMut() -> Result<(), PlayoutError> + 'a;
type AckSender<'a> = dyn FnMut(&[u8], &mut AckProgress<'_>) -> Result<(), ViewerAudioRefused> + 'a;

impl Output {
    fn service_with_ack(
        &mut self,
        live: &mut dyn FnMut() -> bool,
        acknowledge: &mut AckSender<'_>,
    ) -> Result<(), ViewerAudioRefused> {
        let mut checkpoint = || {
            if live() {
                Ok(now())
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
                let (decoder, retirement) =
                    ProcessDecoder::new(self.image.as_deref().ok_or(ViewerAudioRefused)?, limits)
                        .map_err(|_| ViewerAudioRefused)?;
                self.retirement = Some(retirement);
                let mut playout =
                    PulsePlayout::with_decoder(binding, offer, device, &mut checkpoint, decoder)
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
                    .acknowledge_with_progress(&mut checkpoint, |record, progress| {
                        acknowledge(record, progress).map_err(|_| ())
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
                // Cleanup only: fresh local time, not a renewed grant.
                if playout.poll_stop(|| Ok(now())).ok() != Some(State::Closed)
                    || self.retirement.as_ref().is_some_and(|r| !r.is_complete())
                {
                    self.stage = Stage::Stopping(playout);
                }
            }
            stage => self.stage = stage,
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "../bin/fr_cli/linux/output/tests.rs"]
mod tests;

impl Drop for Output {
    fn drop(&mut self) {
        if let Some(retirement) = &self.retirement {
            retirement.stop();
        }
    }
}
