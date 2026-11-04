//! Native epochs retain generation tombstones across explicit talk/mute cycles.
//! A blocked replacement stays in the existing authenticated reliable queue.
use super::{
    Error, HostController, HostInstant, Shared, State, Statistics, admission, audio, native,
};
use asupersync::cx::Cx;
use fr_core::audio::{AudioDirection, AudioStopReason};
use fr_transport::quic::{DatagramReceipt, Disposition};
use fr_wire::{audio::AudioConfiguration, negotiation::Selection};

const SETUP_US: u64 = fr_media::worker::microphone::SETUP_NS / 1000;

pub(super) struct Parameters<'a> {
    pub cx: &'a Cx,
    pub controller: &'a HostController,
    pub shared: &'a Shared,
    pub selection: &'a Selection,
    pub launch: &'a native::Launch,
    pub channel: native::Channel,
}
#[derive(Default)]
pub(super) struct Epoch {
    pub floor: u64,
    pub process: Option<native::Process>,
    pub retiring: Option<native::Retirement>,
    pub setup_until: Option<u64>,
    pub acknowledgement: Option<[u8; audio::AUDIO_CONFIGURED_RECORD_BYTES]>,
    pub inflight: Option<u64>,
    pub statistics: Statistics,
    waiting: Option<(AudioConfiguration, u64)>,
    packet_busy: bool,
}
impl Epoch {
    pub fn state(&self) -> State {
        if let Some(process) = &self.process {
            match process.state() {
                native::State::Starting => State::Starting,
                native::State::Ready => State::Ready,
                native::State::Committing => State::Committing,
                native::State::Running => State::Running,
                native::State::Stopped => State::Retiring,
            }
        } else if self.retiring.as_ref().is_some_and(|r| !r.is_complete()) {
            State::Retiring
        } else {
            State::Idle
        }
    }
    pub fn packet_ready(&self) -> bool {
        // With no active configuration, consume and discard old datagrams.
        // During real readiness/Commit they remain blocked with ORIGINAL age.
        !self.packet_busy
            && self
                .process
                .as_ref()
                .is_none_or(|process| process.state() == native::State::Running)
    }
    pub fn check_deadlines(&self, now: u64) -> Result<(), Error> {
        if self.inflight.is_some_and(|until| now >= until) {
            return Err(Error::AcknowledgementExpired);
        }
        if self.waiting.is_some_and(|(_, until)| now >= until)
            || (self.setup_until.is_some_and(|until| now >= until)
                && self
                    .process
                    .as_ref()
                    .is_none_or(|process| process.state() != native::State::Running))
        {
            return Err(Error::SetupExpired);
        }
        Ok(())
    }
    pub fn precommit_deadline(&self) -> Option<u64> {
        self.waiting.map(|(_, until)| until).or_else(|| {
            self.setup_until.filter(|_| {
                self.process
                    .as_ref()
                    .is_none_or(|process| process.state() != native::State::Running)
            })
        })
    }
    pub fn poll(&mut self) -> Result<(), Error> {
        if let Some(process) = self.process.as_mut() {
            match process.state() {
                native::State::Stopped => {
                    return Err(Error::Native(
                        process.error().unwrap_or(native::Error::Stopped),
                    ));
                }
                native::State::Running => {
                    self.setup_until = None;
                    if self.packet_busy {
                        match process.poll_packet().map_err(Error::Native)? {
                            Some(native::PacketOutcome::Admitted) => {
                                self.statistics.packets_admitted =
                                    self.statistics.packets_admitted.saturating_add(1);
                                self.packet_busy = false;
                            }
                            Some(native::PacketOutcome::Ignored) => {
                                self.statistics.packets_ignored =
                                    self.statistics.packets_ignored.saturating_add(1);
                                self.packet_busy = false;
                            }
                            None => {}
                        }
                    }
                }
                _ => {}
            }
        }
        self.reaped()?;
        Ok(())
    }
    fn reaped(&self) -> Result<bool, Error> {
        let Some(retiring) = self.retiring.as_ref() else {
            return Ok(true);
        };
        if !retiring.is_complete() {
            return Ok(false);
        }
        let receipts = retiring.receipts();
        if [receipts.device, receipts.codec].into_iter().all(|exit| {
            matches!(
                exit,
                native::ChildExit::NotStarted | native::ChildExit::Reaped
            )
        }) {
            Ok(true)
        } else {
            Err(Error::UnresolvedChildren)
        }
    }
    fn mute(&mut self, shared: &Shared) {
        shared.clear_epoch();
        if let Some(process) = self.process.take() {
            process.stop();
            self.retiring = Some(process.retirement());
        }
        self.acknowledgement = None;
        self.setup_until = None;
        self.packet_busy = false;
    }
    pub fn stop(&mut self, shared: &Shared) {
        self.mute(shared);
        if let Some(retiring) = &self.retiring {
            retiring.stop();
        }
        self.waiting = None;
        self.inflight = None;
    }
    pub fn receive(
        &mut self,
        bytes: &[u8],
        receipt: Option<DatagramReceipt>,
        parameters: &Parameters<'_>,
    ) -> Result<Disposition, Error> {
        match (audio::record_kind(bytes), receipt) {
            (Some(0x60), None) => self.configure(bytes, parameters),
            (Some(0x63), None) => {
                let stop = audio::decode_stop(bytes, parameters.channel.id)
                    .map_err(|_| Error::Protocol)?;
                if stop.direction != AudioDirection::Uplink {
                    return Err(Error::Protocol);
                }
                if self.floor != 0 && stop.generation.as_raw() == self.floor {
                    self.mute(parameters.shared);
                    self.waiting = None;
                    if !matches!(
                        stop.reason,
                        AudioStopReason::UserMute | AudioStopReason::DeviceChanged
                    ) {
                        return Err(Error::RemoteStopped(stop.reason));
                    }
                }
                Ok(Disposition::Consumed)
            }
            (Some(0x62), Some(receipt)) => {
                let packet = audio::decode_packet(bytes, parameters.channel.id)
                    .map_err(|_| Error::Protocol)?;
                if packet.direction != AudioDirection::Uplink {
                    return Err(Error::Protocol);
                }
                if packet.generation.as_raw() != self.floor || self.process.is_none() {
                    return Ok(Disposition::Consumed);
                }
                let process = self.process.as_mut().ok_or(Error::Protocol)?;
                match process.receive_record_until(
                    bytes,
                    HostInstant::from_micros(receipt.received_at_micros()),
                    HostInstant::from_micros(receipt.expires_at_micros()),
                ) {
                    Ok(native::Admission::Queued) => {
                        self.packet_busy = true;
                        self.statistics.packets_queued =
                            self.statistics.packets_queued.saturating_add(1);
                        Ok(Disposition::Consumed)
                    }
                    Ok(native::Admission::Ignored) => {
                        self.statistics.packets_ignored =
                            self.statistics.packets_ignored.saturating_add(1);
                        Ok(Disposition::Consumed)
                    }
                    Ok(native::Admission::Stopped) => Err(Error::Protocol),
                    Err(native::Error::Busy | native::Error::NotReady) => Ok(Disposition::Blocked),
                    Err(error) => Err(Error::Native(error)),
                }
            }
            _ => Err(Error::Protocol),
        }
    }
    fn configure(
        &mut self,
        bytes: &[u8],
        parameters: &Parameters<'_>,
    ) -> Result<Disposition, Error> {
        let offer = audio::decode_configuration(bytes, parameters.channel.id)
            .map_err(|_| Error::Protocol)?;
        if offer.direction != AudioDirection::Uplink || offer.generation.as_raw() == 0 {
            return Err(Error::Protocol);
        }
        // A repeated/stale configuration cannot recreate a stopped native
        // owner, retransmit a positive ACK, reset sequence state or renew a cap.
        if offer.generation.as_raw() <= self.floor {
            return Ok(Disposition::Consumed);
        }
        let now = admission(parameters.cx, parameters.controller, parameters.shared)?;
        if let Some((original, until)) = self.waiting {
            if original != offer {
                return Err(Error::Protocol);
            }
            if now.as_micros() >= until {
                return Err(Error::SetupExpired);
            }
        } else {
            let cap = parameters
                .controller
                .deadline(now)
                .map_err(Error::Authority)?
                .as_micros();
            self.waiting = Some((
                offer,
                now.as_micros()
                    .checked_add(SETUP_US)
                    .ok_or(Error::Clock)?
                    .min(cap),
            ));
        }
        if self.process.is_some() {
            self.mute(parameters.shared);
        }
        if !self.reaped()? || self.inflight.is_some() {
            return Ok(Disposition::Blocked);
        }
        let timer = parameters.cx.timer_driver().ok_or(Error::Clock)?;
        let process = native::Process::start(
            parameters.controller.clone(),
            parameters.channel,
            parameters.selection,
            true,
            bytes,
            parameters.launch.clone(),
            || HostInstant::from_micros(timer.now().as_nanos() / 1000),
        )
        .map_err(Error::Native)?;
        if let Err(error) = parameters.shared.install(&process.control()) {
            process.stop();
            self.retiring = Some(process.retirement());
            return Err(error);
        }
        self.floor = offer.generation.as_raw();
        self.setup_until = self.waiting.take().map(|(_, until)| until);
        self.retiring = None;
        self.process = Some(process);
        self.statistics.configurations_started =
            self.statistics.configurations_started.saturating_add(1);
        Ok(Disposition::Consumed)
    }
}
