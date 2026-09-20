//! The SAME acknowledgement state machine serves initial and recovery startup.
//! Recovery borrows the original decoder and receiver; it cannot refill storage
//! credit, accept a foreign worker, or stamp a completed stream before decoding.
use super::{
    Bound, Channel, Configuration, Cx, DecoderRecord, Delivery, Direction, Duration, Error, Launch,
    MediaBudget, Message, PresentationReceipt, Presenter, QuicRecords, ReceiveConfig,
    ReceivePipeline, Route, Setup, StreamRole, admit, decoder, quic, timeout,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Configured,
    AwaitingPicture,
    Decoded,
    Complete,
    Closed,
}

pub(crate) struct ViewerHandshake {
    bound: Bound,
    phase: Phase,
    bytes: [u8; decoder::FIRST_DECODED_BYTES],
    len: usize,
}
impl ViewerHandshake {
    fn configured(bound: Bound) -> Result<Self, Error> {
        let mut this = Self {
            bound,
            phase: Phase::Configured,
            bytes: [0; decoder::FIRST_DECODED_BYTES],
            len: 0,
        };
        this.len = decoder::encode(
            Message::Configured,
            this.bound.setup.binding,
            &this.bound.setup.limits,
            &mut this.bytes,
            Direction::ViewerToHost,
            Delivery::Reliable,
        )?;
        Ok(this)
    }
    fn close(&mut self, receiver: &mut ReceivePipeline) {
        self.bound.closed = true;
        receiver.close();
        self.bytes.fill(0);
        self.len = 0;
        self.phase = Phase::Closed;
    }
    pub(crate) fn tick(&mut self, receiver: &mut ReceivePipeline) -> Result<(), Error> {
        let result = self
            .bound
            .tick()
            .and_then(|n| receiver.tick(n).map_err(|_| Error::Closed));
        if result.is_err() {
            self.close(receiver);
        }
        result
    }
    pub(crate) fn check_transport(
        &mut self,
        receiver: &mut ReceivePipeline,
        transport: &QuicRecords,
    ) -> Result<(), Error> {
        let result = self
            .tick(receiver)
            .and_then(|()| self.bound.check(transport).map(|_| ()));
        if result.is_err() {
            self.close(receiver);
        }
        result
    }
    pub(crate) fn awaiting_picture(&self) -> bool {
        self.phase == Phase::AwaitingPicture
    }
    pub(crate) fn pending(&self) -> bool {
        self.len != 0
    }
    pub(crate) fn is_complete(&self) -> bool {
        self.phase == Phase::Complete
    }
    pub(crate) fn transmit(
        &mut self,
        receiver: &mut ReceivePipeline,
        transport: &mut QuicRecords,
    ) -> Result<bool, Error> {
        let result = (|| {
            self.check_transport(receiver, transport)?;
            let next = match self.phase {
                Phase::Configured => Phase::AwaitingPicture,
                Phase::Decoded => Phase::Complete,
                _ => return Err(Error::WrongState),
            };
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
                    self.phase = next;
                    self.len = 0;
                    self.bytes.fill(0);
                    Ok(true)
                }
                Err(quic::Error::Backpressure) => Ok(false),
                Err(e) => Err(e.into()),
            }
        })();
        if result.is_err() {
            self.close(receiver);
        }
        result
    }
    pub(crate) fn receive_media(
        &mut self,
        receiver: &mut ReceivePipeline,
        channel: Channel,
        bytes: &[u8],
    ) -> Result<(), Error> {
        let result = (|| {
            self.tick(receiver)?;
            if !self.awaiting_picture() {
                return Err(Error::WrongState);
            }
            receiver
                .receive(channel, bytes, self.bound.last)
                .map_err(|_| Error::Closed)?;
            Ok(())
        })();
        if result.is_err() {
            self.close(receiver);
        }
        result
    }
    // Called only after the original native DecodeJob completes its actual
    // in-flight receiver obligation. This is never a wire-supplied boolean.
    pub(crate) fn decoded(
        &mut self,
        receiver: &mut ReceivePipeline,
        receipt: &PresentationReceipt,
    ) -> Result<(), Error> {
        self.tick(receiver)?;
        if !self.awaiting_picture()
            || receipt.decoded.epoch().configuration != self.bound.setup.binding.configuration
            || receipt.decoded.epoch().recovery != self.bound.setup.binding.recovery
            || receipt.frame.as_raw() != receipt.decoded.descriptor().frame
        {
            self.close(receiver);
            return Err(Error::WrongState);
        }
        self.len = decoder::encode(
            Message::FirstDecoded {
                frame: receipt.frame.as_raw(),
                decoder_micros: self.bound.last,
            },
            self.bound.setup.binding,
            &self.bound.setup.limits,
            &mut self.bytes,
            Direction::ViewerToHost,
            Delivery::Reliable,
        )?;
        self.phase = Phase::Decoded;
        Ok(())
    }
    fn check_media(
        &mut self,
        receiver: &mut ReceivePipeline,
        transport: &QuicRecords,
        media: &crate::media_quic::NegotiatedMedia,
    ) -> Result<(), Error> {
        self.check_transport(receiver, transport)?;
        media.check(transport).map_err(|_| Error::InvalidRoutes)?;
        if self.bound.setup.binding != media.binding()
            || self.bound.setup.limits != *media.limits().protocol()
        {
            return Err(Error::InvalidRoutes);
        }
        let config = media
            .receiver_config(transport, fr_media::delivery::ReceivePolicy::default())
            .map_err(|_| Error::InvalidRoutes)?;
        receiver
            .check_delivery_configuration(config.limits, config.bindings, config.epoch)
            .map_err(|_| Error::InvalidRoutes)
    }
    fn finish(
        &mut self,
        presenter: &mut Presenter,
        receiver: &mut ReceivePipeline,
    ) -> Result<(), Error> {
        self.tick(receiver)?;
        if !self.is_complete() {
            return Err(Error::WrongState);
        }
        presenter
            .binding
            .check(receiver)
            .map_err(|_| Error::WrongState)?;
        presenter.stream_binding = Some((self.bound.connection.clone(), self.bound.setup.binding));
        Ok(())
    }
}

/// A native decoder and its receiver, not a forgeable configured boolean.
/// The actual native startup and both acknowledgements retain one deadline.
pub struct Viewer {
    state: ViewerHandshake,
    receiver: ReceivePipeline,
    presenter: Presenter,
}
pub(crate) struct PreparedViewer {
    pub(super) bound: Bound,
    pub(super) receiver: ReceivePipeline,
    pub(super) configuration: Configuration,
    pub(super) record: DecoderRecord,
}
impl PreparedViewer {
    pub(crate) async fn configure(mut self, launch: Launch) -> Result<Viewer, Error> {
        let n = self.bound.tick()?;
        let result = timeout(
            self.bound.cx.now(),
            Duration::from_micros(self.bound.until - n),
            Presenter::start(
                &self.bound.cx,
                launch,
                self.configuration,
                &self.record,
                &mut self.receiver,
            ),
        )
        .await;
        let mut presenter = match result {
            Ok(Ok(presenter)) => presenter,
            Ok(Err(error)) => return Err(Error::Media(error)),
            Err(_) => {
                self.receiver.close();
                return Err(Error::Expired);
            }
        };
        if let Err(error) = self.bound.tick() {
            self.receiver.close();
            presenter.abort();
            return Err(error);
        }
        Ok(Viewer {
            state: ViewerHandshake::configured(self.bound)?,
            receiver: self.receiver,
            presenter,
        })
    }
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
        let prepared = Self::prepare(cx, transport, &setup, bytes, receive)?;
        let mut viewer = prepared.configure(launch).await?;
        viewer.check_transport(transport)?;
        Ok(viewer)
    }
    pub(crate) fn prepare(
        cx: Cx,
        transport: &QuicRecords,
        setup: &Setup,
        bytes: &[u8],
        receive: ReceiveConfig,
    ) -> Result<PreparedViewer, Error> {
        let bound = Bound::new(cx, transport, setup, StreamRole::Client)?;
        if receive.epoch.configuration != setup.binding.configuration
            || receive.epoch.recovery != setup.binding.recovery
            || receive.limits.protocol() != &setup.limits
        {
            return Err(Error::InvalidRoutes);
        }
        let (configuration, record) = admit(bytes, setup)?;
        let budget =
            MediaBudget::new(&setup.limits).map_err(|_| Error::UnsupportedConfiguration)?;
        let receiver =
            ReceivePipeline::new(receive, budget).map_err(|_| Error::UnsupportedConfiguration)?;
        Ok(PreparedViewer {
            bound,
            receiver,
            configuration,
            record,
        })
    }
    pub(crate) fn check_transport(&mut self, transport: &QuicRecords) -> Result<(), Error> {
        let result = self.state.check_transport(&mut self.receiver, transport);
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn close(&mut self) {
        self.state.close(&mut self.receiver);
        self.presenter.abort();
    }
    pub fn tick(&mut self) -> Result<(), Error> {
        let result = self.state.tick(&mut self.receiver);
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn transmit(&mut self, transport: &mut QuicRecords) -> Result<bool, Error> {
        let result = self.state.transmit(&mut self.receiver, transport);
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn receive_media(&mut self, channel: Channel, bytes: &[u8]) -> Result<(), Error> {
        let result = self.state.receive_media(&mut self.receiver, channel, bytes);
        if result.is_err() {
            self.close();
        }
        result
    }
    pub async fn present_first(&mut self) -> Result<Option<PresentationReceipt>, Error> {
        present_first(&mut self.state, &mut self.presenter, &mut self.receiver).await
    }
    pub fn is_complete(&self) -> bool {
        self.state.is_complete()
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.presenter.worker_id()
    }
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
    pub(crate) fn finish_stream(
        mut self,
        transport: &QuicRecords,
        media: &crate::media_quic::NegotiatedMedia,
    ) -> Result<(Presenter, ReceivePipeline), Error> {
        if let Err(error) = self.state.check_media(&mut self.receiver, transport, media) {
            self.close();
            return Err(error);
        }
        self.finish()
    }
    pub fn finish(mut self) -> Result<(Presenter, ReceivePipeline), Error> {
        self.state.finish(&mut self.presenter, &mut self.receiver)?;
        Ok((self.presenter, self.receiver))
    }
}

/// Exclusive handoff of the ORIGINAL failed receiver and live native decoder.
/// A validated next-generation configuration may reuse only identical codec
/// parameters. The single original deadline includes attachment/configuration,
/// real decode and both acknowledgement writes. Drop fences before native abort.
pub struct ViewerRecovery<'a> {
    state: ViewerHandshake,
    presenter: &'a mut Presenter,
    receiver: &'a mut ReceivePipeline,
    completed: bool,
}
impl<'a> ViewerRecovery<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        cx: Cx,
        transport: &QuicRecords,
        media: &crate::media_quic::NegotiatedMedia,
        bytes: &[u8],
        until_micros: u64,
        report: &mut crate::media_quic::recovery::Receiver,
        presenter: &'a mut Presenter,
        receiver: &'a mut ReceivePipeline,
    ) -> Result<Self, Error> {
        let (connection, previous) = presenter.stream_binding.as_ref().ok_or(Error::WrongState)?;
        if !transport.is_bound_to(connection) {
            return Err(Error::ForeignConnection);
        }
        let setup = media
            .recovery_decoder_setup(transport, *previous, until_micros)
            .map_err(|_| Error::InvalidRoutes)?;

        presenter
            .binding
            .check_recovery(receiver)
            .map_err(|_| Error::WrongState)?;
        let original_until = report
            .replacement_deadline(&cx, transport, receiver, media)
            .map_err(|_| Error::InvalidRoutes)?;
        let bound = Bound::new(
            cx,
            transport,
            &setup.capped_at(original_until),
            StreamRole::Client,
        )?;
        let config = media
            .receiver_config(transport, fr_media::delivery::ReceivePolicy::default())
            .map_err(|_| Error::InvalidRoutes)?;
        let (configuration, record) = admit(bytes, &setup)?;
        let old = presenter.configuration;
        if (
            configuration.width,
            configuration.height,
            configuration.fps,
            configuration.generation,
            configuration.backend,
            configuration.max_access_unit_bytes,
        ) != (
            old.width,
            old.height,
            old.fps,
            old.generation,
            old.backend,
            old.max_access_unit_bytes,
        ) || presenter.decoder_record != record.bytes()
        {
            return Err(Error::UnsupportedConfiguration);
        }
        // All wire, authority, owner, limits and codec checks precede mutation.
        // From this point partial failure/drop must fence the original receiver.
        let mut this = Self {
            state: ViewerHandshake::configured(bound)?,
            presenter,
            receiver,
            completed: false,
        };
        this.presenter.recover(
            &this.state.bound.cx,
            this.receiver,
            config.epoch,
            config.bindings,
        )?;
        this.state.check_media(this.receiver, transport, media)?;
        Ok(this)
    }
    pub fn deadline_us(&self) -> u64 {
        self.state.bound.until
    }
    pub fn is_complete(&self) -> bool {
        self.state.is_complete()
    }
    pub fn pending_acknowledgement(&self) -> bool {
        self.state.pending()
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.presenter.worker_id()
    }
    pub fn tick(&mut self, transport: &QuicRecords) -> Result<(), Error> {
        self.state.check_transport(self.receiver, transport)
    }
    pub fn transmit(&mut self, transport: &mut QuicRecords) -> Result<bool, Error> {
        self.state.transmit(self.receiver, transport)
    }
    pub fn receive_media(&mut self, channel: Channel, bytes: &[u8]) -> Result<(), Error> {
        self.state.receive_media(self.receiver, channel, bytes)
    }
    pub async fn present_first(&mut self) -> Result<Option<PresentationReceipt>, Error> {
        let (state, presenter, receiver) = self.parts();
        present_first(state, presenter, receiver).await
    }
    pub fn finish(
        mut self,
        transport: &QuicRecords,
        media: &crate::media_quic::NegotiatedMedia,
    ) -> Result<(), Error> {
        self.state.check_media(self.receiver, transport, media)?;
        self.state.finish(self.presenter, self.receiver)?;
        self.completed = true;
        Ok(())
    }
    pub(crate) fn parts(&mut self) -> (&mut ViewerHandshake, &mut Presenter, &mut ReceivePipeline) {
        (&mut self.state, self.presenter, self.receiver)
    }
}
impl Drop for ViewerRecovery<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.state.close(self.receiver);
            self.presenter.abort();
        }
    }
}

struct PresentOperation<'a> {
    state: &'a mut ViewerHandshake,
    presenter: &'a mut Presenter,
    receiver: &'a mut ReceivePipeline,
    complete: bool,
}
impl Drop for PresentOperation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.state.close(self.receiver);
            self.presenter.abort();
        }
    }
}
async fn present_first(
    state: &mut ViewerHandshake,
    presenter: &mut Presenter,
    receiver: &mut ReceivePipeline,
) -> Result<Option<PresentationReceipt>, Error> {
    let mut operation = PresentOperation {
        state,
        presenter,
        receiver,
        complete: false,
    };
    operation.state.tick(operation.receiver)?;
    if !operation.state.awaiting_picture() {
        return Err(Error::WrongState);
    }
    let current = operation.state.bound.tick()?;
    let result = timeout(
        operation.state.bound.cx.now(),
        Duration::from_micros(operation.state.bound.until - current),
        operation
            .presenter
            .present_next(&operation.state.bound.cx, operation.receiver),
    )
    .await
    .map_err(|_| Error::Expired)??;
    operation.state.tick(operation.receiver)?;
    if let Some(receipt) = &result {
        operation.state.decoded(operation.receiver, receipt)?;
    }
    operation.complete = true;
    Ok(result)
}
