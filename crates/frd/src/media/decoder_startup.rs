//! Decoder startup on already attached media channels. Configuration is checked
//! against actual HEVC before native startup. A configured decoder, a decoded
//! picture and a visible desktop are distinct; none of these grants input.
use super::{CaptureUpdate, ObservationControl, PresentationReceipt, Presenter, host_now};
use crate::worker::Launch;
use asupersync::{cx::Cx, net::quic_native::StreamRole, time::timeout};
use fr_core::limits::ProtocolLimits;
use fr_media::{
    delivery::{MediaBudget, ReceiveConfig, ReceivePipeline},
    hevc::{DecoderRecord, HevcError, HevcGuard},
    worker::{Backend, Configuration},
};
use fr_transport::quic::{self, ConnectionBinding, Messages, QuicRecords, Route, StreamRoute};
use fr_wire::{
    Channel, WireError,
    decoder::{self, Binding, Message},
    input::{InputDelivery as Delivery, InputDirection as Direction},
};
use std::{fmt, time::Duration};

const MAX_STARTUP_US: u64 = 2_000_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Wire(WireError),
    Hevc(HevcError),
    Media(super::Error),
    Transport(quic::Error),
    UnsupportedConfiguration,
    InvalidRoutes,
    ForeignConnection,
    WrongState,
    Expired,
    Closed,
    Allocation,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<WireError> for Error {
    fn from(e: WireError) -> Self {
        Self::Wire(e)
    }
}
impl From<HevcError> for Error {
    fn from(e: HevcError) -> Self {
        Self::Hevc(e)
    }
}
impl From<super::Error> for Error {
    fn from(e: super::Error) -> Self {
        Self::Media(e)
    }
}
impl From<quic::Error> for Error {
    fn from(e: quic::Error) -> Self {
        Self::Transport(e)
    }
}
/// Local installed configuration, never a peer's proposed channel attachment.
/// The caller has negotiated `hevc-decoder-startup` version 1 and authorized
/// this immutable tuple on this exact connection. No constructor attaches it.
#[derive(Clone, Copy)]
pub struct Setup {
    binding: Binding,
    limits: ProtocolLimits,
    configuration: StreamRoute,
    replies: StreamRoute,
    timeout: Duration,
    required_display: Option<(u32, u32)>,
}
impl Setup {
    pub fn new(
        binding: Binding,
        configuration: StreamRoute,
        replies: StreamRoute,
        selection: &fr_wire::negotiation::Selection,
        timeout: Duration,
    ) -> Result<Self, Error> {
        selection
            .validate()
            .map_err(|_| Error::UnsupportedConfiguration)?;
        if !selection
            .capabilities
            .iter()
            .any(|c| c.name == decoder::CAPABILITY && c.version == decoder::VERSION)
        {
            return Err(Error::UnsupportedConfiguration);
        }
        binding.validate()?;
        Ok(Self {
            binding,
            limits: selection.limits,
            configuration,
            replies,
            timeout,
            required_display: None,
        })
    }
    pub(crate) fn require_display(mut self, width: u32, height: u32) -> Result<Self, Error> {
        self.limits
            .validate_coded_dimensions(width, height)
            .map_err(|_| Error::UnsupportedConfiguration)?;
        self.required_display = Some((width, height));
        Ok(self)
    }
}
impl fmt::Debug for Setup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DecoderSetup([installed local bindings])")
    }
}
struct Bound {
    cx: Cx,
    connection: ConnectionBinding,
    setup: Setup,
    until: u64,
    last: u64,
    closed: bool,
}
impl Bound {
    fn new(cx: Cx, transport: &QuicRecords, setup: Setup, role: StreamRole) -> Result<Self, Error> {
        setup.binding.validate()?;
        let us = u64::try_from(setup.timeout.as_micros()).map_err(|_| Error::Expired)?;
        if us == 0 || us > MAX_STARTUP_US {
            return Err(Error::Expired);
        }
        let host = role == StreamRole::Server;
        if transport.role()? != role
            || setup.configuration.outbound != host
            || setup.replies.outbound == host
            || setup.configuration.messages != Messages::Exact(0x30)
            || setup.replies.messages != Messages::DecoderReplies
            || setup.configuration.stream == setup.replies.stream
            || setup.configuration.maximum < decoder::CONFIGURATION_OVERHEAD
            || setup.replies.maximum < decoder::FIRST_DECODED_BYTES
            || [setup.configuration, setup.replies].iter().any(|r| {
                r.binding != setup.binding.parent.id
                    || r.maximum > setup.limits.max_control_message_bytes() as usize
                    || !transport.has_route(Route::Stream(*r))
            })
        {
            return Err(Error::InvalidRoutes);
        }
        let last = host_now(&cx)?.as_micros();
        let until = last.checked_add(us).ok_or(Error::Expired)?;
        let mut this = Self {
            cx,
            connection: transport.binding(),
            setup,
            until,
            last,
            closed: false,
        };
        this.check(transport)?;
        Ok(this)
    }
    fn tick(&mut self) -> Result<u64, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        let n = match self.current() {
            Ok(n) => n,
            Err(e) => {
                self.closed = true;
                return Err(e);
            }
        };
        if n < self.last || n >= self.until {
            self.closed = true;
            return Err(Error::Expired);
        }
        self.last = n;
        Ok(n)
    }
    fn current(&self) -> Result<u64, Error> {
        self.cx.checkpoint().map_err(|_| Error::Closed)?;
        Ok(host_now(&self.cx)?.as_micros())
    }
    fn live(&self) -> bool {
        !self.closed
            && self
                .current()
                .is_ok_and(|n| n >= self.last && n < self.until)
    }
    fn check(&mut self, transport: &QuicRecords) -> Result<u64, Error> {
        let n = self.tick()?;
        if !transport.is_bound_to(&self.connection) {
            self.closed = true;
            return Err(Error::ForeignConnection);
        }
        let incoming = if self.setup.configuration.outbound {
            self.setup.replies
        } else {
            self.setup.configuration
        };
        if transport.is_closed() || transport.receive_ended(incoming)? {
            self.closed = true;
            return Err(Error::Closed);
        }
        Ok(n)
    }
}
fn codec_identifier(record: &DecoderRecord) -> Result<String, Error> {
    let suffix = record
        .codec()
        .strip_prefix("hvc1.")
        .ok_or(Error::UnsupportedConfiguration)?;
    let mut result = String::new();
    result
        .try_reserve_exact(5 + suffix.len())
        .map_err(|_| Error::Allocation)?;
    result.push_str("hev1.");
    result.push_str(suffix);
    Ok(result)
}
fn declaration<'a>(
    cfg: Configuration,
    codec: &'a str,
    record: &'a DecoderRecord,
) -> Result<decoder::Configuration<'a>, Error> {
    let c = cfg.codec().map_err(|_| Error::UnsupportedConfiguration)?;
    let g = c.geometry();
    Ok(decoder::Configuration {
        coded_width: g.coded_width(),
        coded_height: g.coded_height(),
        crop_width: g.crop_width(),
        crop_height: g.crop_height(),
        fps: cfg.fps,
        primaries: 1,
        transfer: 1,
        matrix: 1,
        full_range: false,
        decoded_pictures: 4,
        codec,
        hvcc: record.bytes(),
    })
}
/// The native worker currently implements the explicit BT.709/limited, aligned
/// Main8 software-decoder profile. Other valid wire profiles refuse rather than
/// expanding negotiated limits or silently changing color. Parsing is completed
/// BEFORE creating a process, allocating native surfaces or acknowledging setup.
fn admit(bytes: &[u8], setup: Setup) -> Result<(Configuration, DecoderRecord), Error> {
    let Message::Configuration(c) = decoder::decode(
        bytes,
        setup.binding,
        &setup.limits,
        Direction::HostToViewer,
        Delivery::Reliable,
    )?
    else {
        return Err(Error::WrongState);
    };
    if setup
        .required_display
        .is_some_and(|g| g != (c.crop_width, c.crop_height))
    {
        return Err(Error::UnsupportedConfiguration);
    }
    let cfg = Configuration {
        width: c.crop_width,
        height: c.crop_height,
        fps: c.fps,
        backend: Backend::SoftwareExplicit,
        bitrate: 10_000,
        max_access_unit_bytes: setup.limits.max_encoded_access_unit_bytes(),
        generation: setup.binding.configuration,
    };
    let codec = cfg.codec().map_err(|_| Error::UnsupportedConfiguration)?;
    // This worker's private IPC currently transmits max-AU but not arbitrary
    // negotiated protocol limits. Refuse incompatible limits, never expand them.
    if cfg.limits().map_err(|_| Error::UnsupportedConfiguration)? != setup.limits
        || c.coded_width != codec.geometry().coded_width()
        || c.coded_height != codec.geometry().coded_height()
        || (c.primaries, c.transfer, c.matrix, c.full_range) != (1, 1, 1, false)
        || c.decoded_pictures > 4
    {
        return Err(Error::UnsupportedConfiguration);
    }
    let guard = HevcGuard::from_decoder_record(codec, setup.limits, c.decoded_pictures, c.hvcc)?;
    let record = guard.decoder_record()?;
    if c.codec != codec_identifier(&record)? {
        return Err(Error::UnsupportedConfiguration);
    }
    Ok((cfg, record))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPhase {
    Configuration,
    Configuring,
    RecoveryReady,
    AwaitingDecode,
    Complete,
    Closed,
}
/// One exact configuration record and one source-owned bootstrap IDR. No pixels
/// leave this owner until the matching `DecoderConfigured` is received. A late ack
/// cannot refresh that IDR's original capture-anchored deadline. Drop only frees
/// this subscriber's startup storage; the shared capture/authority owner survives.
pub struct Host {
    bound: Bound,
    control: ObservationControl,
    update: Option<CaptureUpdate>,
    first: u64,
    bytes: Vec<u8>,
    phase: HostPhase,
}
impl Host {
    pub fn new(
        control: ObservationControl,
        transport: &QuicRecords,
        setup: Setup,
        cfg: Configuration,
        update: CaptureUpdate,
    ) -> Result<Self, Error> {
        let mut bound = Bound::new(control.cx.clone(), transport, setup, StreamRole::Server)?;
        let n = control.check()?.as_micros();
        if control
            .authority
            .lock()
            .map_err(|_| Error::Closed)?
            .session()
            != setup.binding.parent.remote_session
        {
            return Err(Error::InvalidRoutes);
        }
        if setup
            .required_display
            .is_some_and(|g| g != (cfg.width, cfg.height))
        {
            return Err(Error::UnsupportedConfiguration);
        }
        let unit = update.encoded().ok_or(Error::WrongState)?;
        if !unit.is_idr()
            || unit.config_generation() != setup.binding.configuration
            || cfg.generation != setup.binding.configuration
            || unit.capture_micros() > n
        {
            return Err(Error::WrongState);
        }
        bound.until = bound.until.min(
            unit.capture_micros()
                .checked_add(MAX_STARTUP_US)
                .ok_or(Error::Expired)?,
        );
        bound.tick()?;
        let mut guard = HevcGuard::new(
            cfg.codec().map_err(|_| Error::UnsupportedConfiguration)?,
            setup.limits,
            4,
        )?;
        guard.validate_length_prefixed(unit.bytes(), true)?;
        let record = guard.decoder_record()?;
        let codec = codec_identifier(&record)?;
        let c = declaration(cfg, &codec, &record)?;
        let n = decoder::CONFIGURATION_OVERHEAD + codec.len() + record.bytes().len();
        if n > setup.configuration.maximum {
            return Err(Error::Wire(WireError::ResourceLimit));
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(n).map_err(|_| Error::Allocation)?;
        bytes.resize(n, 0);
        decoder::encode(
            Message::Configuration(c),
            setup.binding,
            &setup.limits,
            &mut bytes,
            Direction::HostToViewer,
            Delivery::Reliable,
        )?;
        Ok(Self {
            first: unit.frame().as_raw(),
            bound,
            control,
            update: Some(update),
            bytes,
            phase: HostPhase::Configuration,
        })
    }
    /// End startup's deadline exactly once, after the actual matching report.
    /// No visibility evidence or input grant is manufactured by this handoff.
    pub(crate) fn finish_stream(
        mut self,
        transport: &QuicRecords,
    ) -> Result<(ObservationControl, decoder::Binding), Error> {
        self.check_transport(transport)?;
        if !self.is_complete() {
            return Err(Error::WrongState);
        }
        Ok((self.control, self.bound.setup.binding))
    }
    pub fn close(&mut self) {
        self.bound.closed = true;
        self.update = None;
        self.bytes.clear();
        self.phase = HostPhase::Closed;
    }
    pub fn deadline_us(&self) -> u64 {
        self.bound.until
    }
    pub fn is_complete(&self) -> bool {
        self.phase == HostPhase::Complete
    }
    pub fn tick(&mut self) -> Result<(), Error> {
        let result = self
            .bound
            .tick()
            .map(|_| ())
            .and_then(|()| self.control.check().map(|_| ()).map_err(Error::Media));
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Returns true only after ownership of the exact bytes enters QUIC. False
    /// means backpressure; keep this owner and its original deadline unchanged.
    pub fn transmit(&mut self, transport: &mut QuicRecords) -> Result<bool, Error> {
        let result = (|| {
            self.tick()?;
            self.bound.check(transport)?;
            if self.phase != HostPhase::Configuration {
                return Err(Error::WrongState);
            }
            match transport.send(
                &self.bound.cx,
                Route::Stream(self.bound.setup.configuration),
                &self.bytes,
                self.bound.until,
                || self.bound.live() && self.control.check().is_ok(),
            ) {
                Ok(()) => {
                    self.phase = HostPhase::Configuring;
                    self.bytes.clear();
                    Ok(true)
                }
                Err(quic::Error::Backpressure) => Ok(false),
                Err(e) => Err(e.into()),
            }
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Called by the already authenticated connection dispatcher. The first
    /// decoded message remains a peer report, never a native visibility proof.
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<(), Error> {
        let result = (|| {
            self.tick()?;
            if route != Route::Stream(self.bound.setup.replies) {
                return Err(Error::InvalidRoutes);
            }
            let message = decoder::decode(
                bytes,
                self.bound.setup.binding,
                &self.bound.setup.limits,
                Direction::ViewerToHost,
                Delivery::Reliable,
            )?;
            match (self.phase, message) {
                (HostPhase::Configuring, Message::Configured) => {
                    self.phase = HostPhase::RecoveryReady;
                }
                (HostPhase::AwaitingDecode, Message::FirstDecoded { frame, .. })
                    if frame == self.first =>
                {
                    self.phase = HostPhase::Complete;
                }
                _ => return Err(Error::WrongState),
            }
            Ok(())
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Consume only this owner's reply lane; unrelated control/media remains
    /// queued for its existing dispatcher. Connection identity is checked before
    /// receiving anything, not inferred from an equal numeric route binding.
    pub fn dispatch(&mut self, transport: &mut QuicRecords) -> Result<(), Error> {
        self.check_transport(transport)?;
        let cx = self.bound.cx.clone();
        let control = self.control.clone();
        let incoming = self.bound.setup.replies;
        let until = self.bound.until;
        let mut failure = None;
        let result = transport.receive_ready(
            &cx,
            || control.check().is_ok_and(|n| n.as_micros() < until),
            |route| route == Route::Stream(incoming),
            |route, bytes| match self.receive(route, bytes) {
                Ok(()) => Ok(quic::Disposition::Consumed),
                Err(e) => {
                    failure = Some(e);
                    Err(())
                }
            },
        );
        if let Some(e) = failure {
            self.close();
            return Err(e);
        }
        if let Err(e) = result {
            self.close();
            return Err(e.into());
        }
        self.check_transport(transport)
    }
    /// Transfer the same source-provenance object once, only after configuration.
    /// The existing subscriber/egress must admit it on its reliable recovery lane.
    /// This does not manufacture a new capture or source timestamp. The session
    /// must service this startup deadline until the first-frame report arrives,
    /// independently of the delivery engine's reference/recovery horizons.
    pub fn take_recovery(&mut self) -> Result<Option<CaptureUpdate>, Error> {
        self.tick()?;
        if self.phase != HostPhase::RecoveryReady {
            return Ok(None);
        }
        self.phase = HostPhase::AwaitingDecode;
        Ok(self.update.take())
    }
    /// Service FIN, cancellation and deadlines even when there are no replies.
    pub fn check_transport(&mut self, transport: &QuicRecords) -> Result<(), Error> {
        let result = self
            .tick()
            .and_then(|()| self.bound.check(transport).map(|_| ()));
        if result.is_err() {
            self.close();
        }
        result
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewerPhase {
    Configured,
    AwaitingPicture,
    Decoded,
    Complete,
    Closed,
}
/// A native decoder and its receiver, not a forgeable "configured" boolean.
/// Construction runs the real process startup; failures/cancellation close the
/// receiver and worker. Pending acknowledgements have a fixed original deadline.
pub struct Viewer {
    bound: Bound,
    receiver: ReceivePipeline,
    presenter: Presenter,
    phase: ViewerPhase,
    bytes: [u8; decoder::FIRST_DECODED_BYTES],
    len: usize,
}
impl Viewer {
    pub async fn start(
        cx: Cx,
        transport: &QuicRecords,
        setup: Setup,
        bytes: &[u8],
        launch: Launch,
        receive: ReceiveConfig,
    ) -> Result<Self, Error> {
        let mut bound = Bound::new(cx, transport, setup, StreamRole::Client)?;
        if receive.epoch.configuration != setup.binding.configuration
            || receive.epoch.recovery != setup.binding.recovery
            || receive.limits.protocol() != &setup.limits
        {
            return Err(Error::InvalidRoutes);
        }
        let (cfg, record) = admit(bytes, setup)?;
        let budget =
            MediaBudget::new(&setup.limits).map_err(|_| Error::UnsupportedConfiguration)?;
        let mut receiver =
            ReceivePipeline::new(receive, budget).map_err(|_| Error::UnsupportedConfiguration)?;
        let n = bound.tick()?;
        let result = timeout(
            bound.cx.now(),
            Duration::from_micros(bound.until - n),
            Presenter::start(&bound.cx, launch, cfg, &record, &mut receiver),
        )
        .await;
        let presenter = match result {
            Ok(Ok(presenter)) => presenter,
            Ok(Err(e)) => return Err(Error::Media(e)),
            Err(_) => {
                receiver.close();
                return Err(Error::Expired);
            }
        };
        // A ready native reply that crossed expiry cannot mint an acknowledgement.
        bound.check(transport)?;
        let mut this = Self {
            bound,
            receiver,
            presenter,
            phase: ViewerPhase::Configured,
            bytes: [0; decoder::FIRST_DECODED_BYTES],
            len: 0,
        };
        this.len = decoder::encode(
            Message::Configured,
            setup.binding,
            &setup.limits,
            &mut this.bytes,
            Direction::ViewerToHost,
            Delivery::Reliable,
        )?;
        Ok(this)
    }
    pub fn close(&mut self) {
        self.bound.closed = true;
        self.receiver.close();
        self.presenter.abort();
        self.bytes.fill(0);
        self.len = 0;
        self.phase = ViewerPhase::Closed;
    }
    pub fn tick(&mut self) -> Result<(), Error> {
        let result = (|| {
            let n = self.bound.tick()?;
            self.receiver.tick(n).map_err(|_| Error::Closed)
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn transmit(&mut self, transport: &mut QuicRecords) -> Result<bool, Error> {
        let result = (|| {
            self.tick()?;
            self.bound.check(transport)?;
            if self.len == 0 {
                return Err(Error::WrongState);
            }
            match transport.send(
                &self.bound.cx,
                Route::Stream(self.bound.setup.replies),
                &self.bytes[..self.len],
                self.bound.until,
                || self.bound.live(),
            ) {
                Ok(()) => {
                    self.phase = match self.phase {
                        ViewerPhase::Configured => ViewerPhase::AwaitingPicture,
                        ViewerPhase::Decoded => ViewerPhase::Complete,
                        _ => return Err(Error::WrongState),
                    };
                    self.len = 0;
                    self.bytes.fill(0);
                    Ok(true)
                }
                Err(quic::Error::Backpressure) => Ok(false),
                Err(e) => Err(e.into()),
            }
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// The authenticated dispatcher maps an installed media route to `channel`.
    /// Receiver binding and epoch checks reject stale/foreign records. No media
    /// is accepted until the real configured acknowledgement enters transport.
    pub fn receive_media(&mut self, channel: Channel, bytes: &[u8]) -> Result<(), Error> {
        let result = (|| {
            self.tick()?;
            if self.phase != ViewerPhase::AwaitingPicture {
                return Err(Error::WrongState);
            }
            self.receiver
                .receive(channel, bytes, self.bound.last)
                .map_err(|_| Error::Closed)?;
            Ok(())
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// The genuine decoder completion supplies the first-frame report. Returning
    /// a compositor-submission receipt still does not certify visible pixels.
    pub async fn present_first(&mut self) -> Result<Option<PresentationReceipt>, Error> {
        self.tick()?;
        if self.phase != ViewerPhase::AwaitingPicture {
            self.close();
            return Err(Error::WrongState);
        }
        let mut operation = PresentOperation {
            viewer: self,
            complete: false,
        };
        let result = operation
            .viewer
            .presenter
            .present_next(&operation.viewer.bound.cx, &mut operation.viewer.receiver)
            .await?;
        operation.viewer.tick()?;
        if let Some(receipt) = &result {
            let message = Message::FirstDecoded {
                frame: receipt.frame.as_raw(),
                decoder_micros: operation.viewer.bound.last,
            };
            operation.viewer.len = decoder::encode(
                message,
                operation.viewer.bound.setup.binding,
                &operation.viewer.bound.setup.limits,
                &mut operation.viewer.bytes,
                Direction::ViewerToHost,
                Delivery::Reliable,
            )?;
            operation.viewer.phase = ViewerPhase::Decoded;
        }
        operation.complete = true;
        Ok(result)
    }
    pub fn is_complete(&self) -> bool {
        self.phase == ViewerPhase::Complete
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.presenter.worker_id()
    }
    /// Finalize a refused/cancelled startup using a separate live cleanup Cx.
    /// Revoke the receiver first; no acknowledgement or pixel effect is replayed.
    pub async fn reap(
        &mut self,
        cleanup: &Cx,
        deadline: crate::worker::Deadline,
    ) -> Result<asupersync::process::ExitStatus, Error> {
        self.close();
        self.presenter
            .reap(cleanup, deadline)
            .await
            .map_err(Error::Media)
    }
    /// Join only the original authenticated connection and its completed media
    /// attachments. Numeric decoder IDs alone do not authorize this transfer.
    pub(crate) fn finish_stream(
        mut self,
        transport: &QuicRecords,
        media: &crate::media_quic::NegotiatedMedia,
    ) -> Result<(Presenter, ReceivePipeline), Error> {
        let checked = (|| {
            self.bound.check(transport)?;
            media.check(transport).map_err(|_| Error::InvalidRoutes)?;
            if self.bound.setup.binding != media.binding()
                || self.bound.setup.limits != *media.limits().protocol()
            {
                return Err(Error::InvalidRoutes);
            }
            let config = media
                .receiver_config(transport, fr_media::delivery::ReceivePolicy::default())
                .map_err(|_| Error::InvalidRoutes)?;
            self.receiver
                .check_delivery_configuration(config.limits, config.bindings, config.epoch)
                .map_err(|_| Error::InvalidRoutes)?;
            Ok(())
        })();
        if let Err(error) = checked {
            self.close();
            return Err(error);
        }
        self.finish()
    }
    /// Move the configured owner into the regular session, preserving receiver
    /// identity, reference state, decoder reservations and native process.
    pub fn finish(mut self) -> Result<(Presenter, ReceivePipeline), Error> {
        self.tick()?;
        if self.phase != ViewerPhase::Complete {
            return Err(Error::WrongState);
        }
        self.presenter.stream_binding =
            Some((self.bound.connection.clone(), self.bound.setup.binding));
        Ok((self.presenter, self.receiver))
    }
}
struct PresentOperation<'a> {
    viewer: &'a mut Viewer,
    complete: bool,
}
impl Drop for PresentOperation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.viewer.close();
        }
    }
}
