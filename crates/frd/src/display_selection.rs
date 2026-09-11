//! One initial full-display choice on an approved native session. The catalog
//! contains host-owned aliases, never OS paths. Selection precedes media binding
//! and never grants input, proves presentation, or replaces local consent.
use crate::media::{ObservationControl, decoder_startup};
use asupersync::{cx::Cx, net::quic_native::StreamRole, types::CancelKind};
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration, ViewportMappingGeneration},
    limits::ProtocolLimits,
};
use fr_transport::quic::{
    self, ChannelScope, ConnectionBinding, ControlRoutes, Disposition, QuicRecords, Route,
};
use fr_wire::{
    Kind, WireError,
    decoder::Binding,
    display::{self, Catalog, Display, Message},
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Selection},
};
use std::{cell::Cell, fmt, time::Duration};

const MAX_TIMEOUT_US: u64 = 60_000_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    CapabilityMissing,
    ForeignConnection,
    Closed,
    Expired,
    Clock,
    Order,
    ChangedDisplay,
    Wire(WireError),
    Transport(quic::Error),
    Authority(crate::media::Error),
    Decoder(decoder_startup::Error),
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
impl From<quic::Error> for Error {
    fn from(e: quic::Error) -> Self {
        Self::Transport(e)
    }
}

struct Life {
    cx: Cx,
    control: Option<ObservationControl>,
    connection: ConnectionBinding,
    routes: ControlRoutes,
    parent: ControlBinding,
    selection: Selection,
    last: Cell<u64>,
    stopped: Cell<bool>,
}
impl Life {
    fn clock(&self) -> Result<u64, Error> {
        if self.stopped.get() || self.cx.checkpoint().is_err() {
            return Err(Error::Closed);
        }
        let n = if let Some(c) = &self.control {
            c.check().map_err(Error::Authority)?.as_micros()
        } else {
            self.cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000
        };
        if n < self.last.get() {
            self.stop();
            return Err(Error::Clock);
        }
        self.last.set(n);
        Ok(n)
    }
    fn check(&self, q: &QuicRecords) -> Result<u64, Error> {
        // Never touch the supplied connection before checking its object identity.
        if !q.is_bound_to(&self.connection) {
            return Err(Error::ForeignConnection);
        }
        if q.is_closed() || q.receive_ended(self.routes.inbound)? {
            return Err(Error::Closed);
        }
        self.clock()
    }
    fn stop(&self) {
        if !self.stopped.replace(true) {
            if let Some(c) = &self.control {
                c.revoke();
            } else {
                self.cx.cancel_fast(CancelKind::User);
            }
        }
    }
}
impl Drop for Life {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    CatalogPending,
    AwaitCatalog,
    Choose,
    SelectPending,
    AwaitSelect,
    Complete,
}

/// One bounded exchange per connection. The containing HostSession/ViewerSession
/// keeps driving observation renewal while this owner waits. Dropping an unfinished
/// exchange stops its original session; it never modifies a foreign connection.
pub struct DisplaySelection {
    life: Option<Life>,
    catalog: Option<Catalog>,
    selected: Option<Display>,
    phase: Phase,
    until: u64,
    bytes: [u8; display::MAX_CATALOG_BYTES],
    len: usize,
}
impl fmt::Debug for DisplaySelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DisplaySelection")
            .field("phase", &self.phase)
            .field("pending_bytes", &self.len)
            .finish_non_exhaustive()
    }
}
impl DisplaySelection {
    pub fn host(
        q: &mut QuicRecords,
        scope: ChannelScope<'_>,
        control: ObservationControl,
        catalog: Catalog,
        timeout: Duration,
    ) -> Result<Self, Error> {
        control.check().map_err(Error::Authority)?;
        if !control.belongs_to_session(scope.parent.remote_session) {
            return Err(Error::Configuration);
        }
        let cx = control.context();
        Self::new(cx, q, scope, Some(control), Some(&catalog), timeout)
    }
    pub fn viewer(
        cx: Cx,
        q: &mut QuicRecords,
        scope: ChannelScope<'_>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        Self::new(cx, q, scope, None, None, timeout)
    }
    fn new(
        cx: Cx,
        q: &mut QuicRecords,
        scope: ChannelScope<'_>,
        control: Option<ObservationControl>,
        catalog: Option<&Catalog>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        scope
            .selection
            .validate()
            .map_err(|_| Error::Configuration)?;
        let host = control.is_some();
        if q.role()?
            != if host {
                StreamRole::Server
            } else {
                StreamRole::Client
            }
            || scope.parent.id != scope.control.outbound.binding
            || scope.parent.host_boot.as_raw() == 0
            || scope.parent.os_session.as_raw() == 0
            || scope.parent.remote_session.as_raw() == 0
        {
            return Err(Error::Configuration);
        }
        if !scope
            .selection
            .capabilities
            .iter()
            .any(|c| c.name == display::CAPABILITY && c.version == display::VERSION)
        {
            return Err(Error::CapabilityMissing);
        }
        let duration = u64::try_from(timeout.as_micros()).map_err(|_| Error::Configuration)?;
        if duration == 0 || duration > MAX_TIMEOUT_US {
            return Err(Error::Configuration);
        }
        cx.checkpoint().map_err(|_| Error::Closed)?;
        let now = cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000;
        let until = now.checked_add(duration).ok_or(Error::Clock)?;
        let mut bytes = [0; display::MAX_CATALOG_BYTES];
        let len = if let Some(catalog) = catalog {
            display::encode(
                &Message::Catalog(*catalog),
                scope.parent,
                &scope.selection.limits,
                &mut bytes,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )?
        } else {
            0
        };
        // Validate everything, including output size, before consuming the unique
        // connection reservation. Failed setup cannot strand an ordinary session.
        if len > scope.control.outbound.maximum
            || scope.control.outbound.maximum < display::SELECT_BYTES
            || scope.control.inbound.maximum < display::SELECT_BYTES
        {
            return Err(Error::Configuration);
        }
        let connection = q.claim_display_selection(scope.control)?;
        Ok(Self {
            life: Some(Life {
                cx,
                control,
                connection,
                routes: scope.control,
                parent: scope.parent,
                selection: scope.selection.clone(),
                last: Cell::new(now),
                stopped: Cell::new(false),
            }),
            catalog: catalog.copied(),
            selected: None,
            phase: if host {
                Phase::CatalogPending
            } else {
                Phase::AwaitCatalog
            },
            until,
            bytes,
            len,
        })
    }
    pub const fn is_complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }
    pub const fn deadline_us(&self) -> u64 {
        self.until
    }
    fn life(&self) -> Result<&Life, Error> {
        self.life.as_ref().ok_or(Error::Closed)
    }
    fn check(&self, q: &QuicRecords) -> Result<(), Error> {
        let life = self.life()?;
        let result = life.check(q).and_then(|now| {
            if now >= self.until {
                Err(Error::Expired)
            } else {
                Ok(())
            }
        });
        if result.is_err() {
            life.stop();
        }
        result
    }
    /// The exact catalog actually received, not a discovery cache. No default
    /// display is selected implicitly, including when there is only one entry.
    pub fn catalog(&self, q: &QuicRecords) -> Result<Option<&Catalog>, Error> {
        self.check(q)?;
        Ok(self.catalog.as_ref())
    }
    /// Local UI choice only. Crops, stale catalogs, and invented handles cannot
    /// enter this path. Backpressure never replaces the selected pending record.
    pub fn choose(&mut self, q: &QuicRecords, handle: u128) -> Result<(), Error> {
        self.check(q)?;
        if self.phase != Phase::Choose {
            return Err(Error::Order);
        }
        let c = self.catalog.as_ref().ok_or(Error::Order)?;
        let request = c.selection(handle)?;
        let selected = c.selected(request)?;
        let (parent, limits) = {
            let life = self.life()?;
            (life.parent, life.selection.limits)
        };
        self.len = display::encode(
            &Message::Select(request),
            parent,
            &limits,
            &mut self.bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )?;
        self.selected = Some(selected);
        self.phase = Phase::SelectPending;
        Ok(())
    }
    /// Admit ONE retained record without driving or cancelling the parent's I/O.
    /// `true` means queue admission, not peer receipt or permission to capture.
    pub fn transmit(&mut self, q: &mut QuicRecords) -> Result<bool, Error> {
        self.check(q)?;
        if self.len == 0 {
            return Ok(false);
        }
        let life = self.life()?;
        let result = q.send(
            &life.cx,
            Route::Stream(life.routes.outbound),
            &self.bytes[..self.len],
            self.until,
            || life.clock().is_ok_and(|n| n < self.until),
        );
        match result {
            Ok(()) => {
                self.phase = match self.phase {
                    Phase::CatalogPending => Phase::AwaitSelect,
                    Phase::SelectPending => Phase::Complete,
                    _ => {
                        self.close();
                        return Err(Error::Order);
                    }
                };
                self.len = 0;
                self.bytes.fill(0);
                self.check(q)?;
                Ok(true)
            }
            Err(quic::Error::Backpressure) => Ok(false),
            Err(e) => {
                self.close();
                Err(e.into())
            }
        }
    }
    fn receive(&mut self, bytes: &[u8]) -> Result<Disposition, Error> {
        let life = self.life()?;
        if life.clock()? >= self.until {
            return Err(Error::Expired);
        }
        // The transport already bounded and framed this exact control record.
        // Inspect only its kind for dispatch; the display codec below validates
        // the full envelope, extensions, parent and direction before use.
        let kind = bytes.get(6..8);
        if kind != Some(&(Kind::DisplayCatalog as u16).to_be_bytes())
            && kind != Some(&(Kind::SelectDisplay as u16).to_be_bytes())
        {
            return Ok(Disposition::Blocked);
        }
        let direction = if life.control.is_some() {
            InputDirection::ViewerToHost
        } else {
            InputDirection::HostToViewer
        };
        let m = display::decode(
            bytes,
            life.parent,
            &life.selection.limits,
            direction,
            InputDelivery::Reliable,
        )?;
        match (self.phase, m) {
            (Phase::AwaitCatalog, Message::Catalog(catalog)) => {
                self.catalog = Some(catalog);
                self.phase = Phase::Choose;
            }
            (Phase::AwaitSelect, Message::Select(request)) => {
                self.selected = Some(
                    self.catalog
                        .as_ref()
                        .ok_or(Error::Order)?
                        .selected(request)?,
                );
                self.phase = Phase::Complete;
            }
            _ => return Err(Error::Order),
        }
        Ok(Disposition::Consumed)
    }
    /// Drain only display records on the original control pair. Unrelated
    /// renewal/clock records remain for their existing typed dispatcher.
    pub fn dispatch(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        self.check(q)?;
        let life = self.life()?;
        let (cx, route, control, until) = (
            life.cx.clone(),
            life.routes.inbound,
            life.control.clone(),
            self.until,
        );
        let mut failure = None;
        let result = q.receive_ready(
            &cx,
            || {
                cx.checkpoint().is_ok()
                    && cx
                        .timer_driver()
                        .is_some_and(|c| c.now().as_nanos() / 1000 < until)
                    && control.as_ref().is_none_or(|c| c.check().is_ok())
            },
            |r| r == Route::Stream(route),
            |_, bytes| match self.receive(bytes) {
                Ok(d) => Ok(d),
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
        self.check(q)
    }
    /// Transfer the unique selection lifetime. On the viewer, selection is still
    /// a request: each subsequent host binding/configuration MUST match this proof.
    pub fn finish(mut self, q: &QuicRecords) -> Result<SelectedDisplay, Error> {
        self.check(q)?;
        if !self.is_complete() {
            return Err(Error::Order);
        }
        Ok(SelectedDisplay {
            display: self.selected.ok_or(Error::Order)?,
            life: self.life.take().ok_or(Error::Closed)?,
        })
    }
    pub fn close(&mut self) {
        if let Some(life) = &self.life {
            life.stop();
        }
        self.bytes.fill(0);
        self.len = 0;
    }
}

/// Keep this owner for the selected view's lifetime. Invalidation/destruction
/// ends the original observation session before an old binding can be reused.
/// The initial profile supports one full display; resizing/reselecting needs a
/// new session rather than recycling a live view's generations.
pub struct SelectedDisplay {
    life: Life,
    display: Display,
}
impl fmt::Debug for SelectedDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SelectedDisplay([connection-bound full display])")
    }
}
impl SelectedDisplay {
    pub fn check(&self, q: &QuicRecords) -> Result<(), Error> {
        let result = self.life.check(q).map(|_| ());
        if result.is_err() {
            self.life.stop();
        }
        result
    }
    pub fn display(&self, q: &QuicRecords) -> Result<Display, Error> {
        self.check(q)?;
        Ok(self.display)
    }
    pub fn invalidate(&self) {
        self.life.stop();
    }
    /// Local topology refresh must keep the exact selected identity and geometry.
    /// Never retarget an old handle to a replacement output with equal dimensions.
    pub fn revalidate(&self, q: &QuicRecords, current: &Catalog) -> Result<(), Error> {
        self.check(q)?;
        if current.find(self.display.handle) != Some(self.display) {
            self.invalidate();
            return Err(Error::ChangedDisplay);
        }
        Ok(())
    }
    pub fn binding(&self, q: &QuicRecords, channel_id: u32) -> Result<Binding, Error> {
        self.check(q)?;
        if channel_id == 0 || channel_id == self.life.parent.id {
            return Err(Error::Configuration);
        }
        Ok(Binding {
            parent: ControlBinding {
                id: channel_id,
                ..self.life.parent
            },
            display: self.display.handle,
            geometry: self.display.geometry,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        })
    }
    pub fn check_binding(&self, q: &QuicRecords, binding: Binding) -> Result<(), Error> {
        if self.binding(q, binding.parent.id)? != binding {
            return Err(Error::ChangedDisplay);
        }
        Ok(())
    }
    /// Bind actual native startup to the selected display, not just an opaque ID.
    /// A valid HEVC stream with different visible dimensions is rejected before
    /// worker launch. Coded padding remains allowed by the existing HEVC guard.
    pub fn decoder_setup(
        &self,
        q: &QuicRecords,
        media: &crate::media_quic::NegotiatedMedia,
        timeout: Duration,
    ) -> Result<decoder_startup::Setup, Error> {
        self.check_binding(q, media.binding())?;
        if media.limits().protocol() != &self.life.selection.limits {
            return Err(Error::Configuration);
        }
        media
            .decoder_setup(q, timeout)
            .and_then(|s| s.require_display(self.display.pixel_width, self.display.pixel_height))
            .map_err(Error::Decoder)
    }
    pub(crate) fn check_capture(
        &self,
        q: &QuicRecords,
        control: &ObservationControl,
        catalog: &Catalog,
    ) -> Result<Display, Error> {
        self.check(q)?;
        if self
            .life
            .control
            .as_ref()
            .is_none_or(|owner| !owner.same_owner(control))
        {
            return Err(Error::Configuration);
        }
        if catalog.find(self.display.handle) != Some(self.display) {
            return Err(Error::ChangedDisplay);
        }
        Ok(self.display)
    }
    pub fn limits(&self) -> ProtocolLimits {
        self.life.selection.limits
    }
}
