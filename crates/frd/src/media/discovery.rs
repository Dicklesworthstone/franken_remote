//! Native discovery runs through the existing supervised process, not the broker.
//! The original worker, catalog aliases, approved authority and network-selected
//! output stay joined; no peer can provide an X display string or crop rectangle.
use super::{CaptureSource, Error, FrameId, MediaOperation, ObservationControl};
use crate::{
    display_selection::SelectedDisplay,
    worker::{Launch, MonitorDiscovery},
};
use fr_media::worker::{Configuration, Kind};
use fr_transport::quic::QuicRecords;
use fr_wire::display::{Catalog, Select};
use std::{future::Future, sync::Arc, time::Duration};

pub struct DiscoveredSource {
    discovery: MonitorDiscovery,
    control: ObservationControl,
}
impl DiscoveredSource {
    pub async fn start(control: &ObservationControl, launch: Launch) -> Result<Self, Error> {
        control.check()?;
        let discovery = MonitorDiscovery::start(
            &control.context(),
            launch,
            control.deadline(Duration::from_secs(2))?,
        )
        .await?;
        control.check()?;
        Ok(Self {
            discovery,
            control: control.clone(),
        })
    }
    /// Local policy may narrow disclosure before publishing this catalog. The
    /// returned snapshot does not establish current topology or pixel freshness.
    pub fn catalog(&self) -> Result<Catalog, Error> {
        self.control.check()?;
        Ok(self.discovery.catalog())
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.discovery.id()
    }
    pub async fn check_display(&mut self) -> Result<(), Error> {
        let mut guard = SelectionGuard::new(&self.control);
        self.discovery
            .check_display(
                &self.control.context(),
                self.control.deadline(Duration::from_secs(2))?,
            )
            .await?;
        self.control.check()?;
        guard.complete = true;
        Ok(())
    }
    /// Checks the actual network choice synchronously, then returns a future that
    /// borrows neither QUIC nor its driver. Continue session renewal concurrently
    /// while native configuration runs. Dropping even the unpolled future revokes
    /// this original selected lifetime and destroys the unconfigured worker.
    pub fn configure(
        self,
        q: &QuicRecords,
        selected: &SelectedDisplay,
        configuration: Configuration,
    ) -> Result<impl Future<Output = Result<CaptureSource, Error>> + use<>, Error> {
        let display = selected
            .check_capture(q, &self.control, &self.discovery.catalog())
            .map_err(|_| Error::InvalidFrame)?;
        configuration.codec()?;
        if (configuration.width, configuration.height)
            != (display.pixel_width, display.pixel_height)
            || configuration.max_access_unit_bytes
                > selected.limits().max_encoded_access_unit_bytes()
        {
            return Err(Error::InvalidFrame);
        }
        let choice = self
            .discovery
            .catalog()
            .selection(display.handle)
            .map_err(|_| Error::InvalidFrame)?;
        self.configure_choice(choice, configuration)
    }
    /// Configure a display chosen by the LOCAL share-session owner, before any
    /// viewer subscribes. `choice` must come from this original native catalog;
    /// a remote display request must use `configure` instead. No permission is
    /// created here: the caller must already hold independent local observation
    /// consent and keep its OS permission/revocation owner progressing.
    ///
    /// Peer admission, a network renewer or an input-grant owner cannot be used
    /// as the source lifetime. Configuration stays on the same discovered child
    /// and the original absolute deadline includes unpolled time. Every refusal
    /// or abandoned future fences this source before child cleanup. The resulting
    /// source can feed Publisher without borrowing any viewer's authority.
    pub fn configure_local(
        self,
        choice: Select,
        configuration: Configuration,
    ) -> Result<impl Future<Output = Result<CaptureSource, Error>> + use<>, Error> {
        use std::sync::atomic::Ordering;
        let mut guard = SelectionGuard::new(&self.control);
        let now = self.control.check()?;
        if self.control.admission.is_some()
            || self.control.renewal_attached.load(Ordering::Acquire)
            || self.control.control_grant_attached.load(Ordering::Acquire)
            || self
                .control
                .authority
                .lock()
                .map_err(|_| Error::Poisoned)?
                .has_live_control(now)
        {
            return Err(Error::NotIndependentSource);
        }
        let future = self.configure_choice(choice, configuration)?;
        guard.complete = true;
        Ok(future)
    }
    // The original local preparation owns the sticky renewal reservation. Public
    // configure_local still rejects reserved/network-renewed sources. This path
    // cannot transfer the discovered child to another authority allocation.
    pub(crate) fn configure_prepared(
        self,
        original: &ObservationControl,
        choice: Select,
        configuration: Configuration,
    ) -> Result<impl Future<Output = Result<CaptureSource, Error>> + use<>, Error> {
        if !self.control.same_owner(original)
            || !original
                .renewal_attached
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(Error::NotIndependentSource);
        }
        self.configure_choice(choice, configuration)
    }
    fn configure_choice(
        self,
        choice: Select,
        configuration: Configuration,
    ) -> Result<impl Future<Output = Result<CaptureSource, Error>> + use<>, Error> {
        let guard = SelectionGuard::new(&self.control);
        let catalog = self.discovery.catalog();
        let display = catalog.selected(choice).map_err(|_| Error::InvalidFrame)?;
        configuration.codec()?;
        configuration.limits()?;
        if (configuration.width, configuration.height)
            != (display.pixel_width, display.pixel_height)
        {
            return Err(Error::InvalidFrame);
        }
        // Capture the native budget at CALL time, not the first future poll.
        let deadline = self.control.deadline(Duration::from_secs(2))?;
        Ok(async move {
            let mut guard = guard;
            let mut discovery = self.discovery;
            let control = self.control;
            discovery
                .configure(&control.context(), choice, configuration, deadline)
                .await?;
            control.check()?;
            let worker = discovery.into_worker()?;
            let source = Arc::new(());
            let recovery = super::recovery_source::SourceRecovery::new(&source)?;
            let result = CaptureSource {
                worker,
                configuration,
                next: Some(FrameId::FIRST),
                source,
                last_capture: None,
                recovery,
                selected_control: Some(control),
                selected_catalog_revision: Some(catalog.revision()),
            };
            guard.complete = true;
            Ok(result)
        })
    }
    pub fn abort(&mut self) {
        self.discovery.abort();
    }
    pub async fn reap(
        &mut self,
        cx: &asupersync::cx::Cx,
        deadline: crate::worker::Deadline,
    ) -> Result<(), Error> {
        self.discovery
            .reap(cx, deadline)
            .await
            .map_err(Error::Worker)
    }
}
pub(super) struct SelectionGuard {
    control: Option<ObservationControl>,
    complete: bool,
}
impl SelectionGuard {
    fn new(control: &ObservationControl) -> Self {
        Self {
            control: Some(control.clone()),
            complete: false,
        }
    }
    pub(super) fn capture(
        source: &CaptureSource,
        control: &ObservationControl,
    ) -> Result<Self, Error> {
        if let Some(original) = &source.selected_control {
            if !original.same_owner(control) {
                return Err(Error::InvalidFrame);
            }
            original.check()?;
        }
        Ok(Self {
            control: source.selected_control.clone(),
            complete: false,
        })
    }
    pub(super) fn finish(&mut self) {
        self.complete = true;
    }
}
impl Drop for SelectionGuard {
    fn drop(&mut self) {
        if !self.complete
            && let Some(control) = &self.control
        {
            control.revoke();
        }
    }
}
impl CaptureSource {
    /// Only the selected native display, under the same original source consent.
    /// This is an immutable disclosure snapshot, not a new topology check,
    /// freshness witness or permission for another viewer. Neighboring displays
    /// and native identifiers never enter this catalog.
    pub fn selected_catalog(&self, control: &ObservationControl) -> Result<Catalog, Error> {
        control.check()?;
        if self
            .selected_control
            .as_ref()
            .is_none_or(|original| !original.same_owner(control))
        {
            return Err(Error::InvalidFrame);
        }
        let revision = self.selected_catalog_revision.ok_or(Error::InvalidFrame)?;
        let display = self.worker.selected_display().ok_or(Error::InvalidFrame)?;
        Catalog::new(revision, &[display], &self.configuration.limits()?)
            .map_err(|_| Error::InvalidFrame)
    }
    /// Idle topology verification. No capture is taken and neither source age
    /// nor input readiness advances. A refusal revokes the selected observation.
    pub async fn check_selected_display(
        &mut self,
        control: &ObservationControl,
    ) -> Result<(), Error> {
        if self.selected_control.is_none() {
            return Err(Error::InvalidFrame);
        }
        let mut guard = SelectionGuard::capture(self, control)?;
        let mut operation = MediaOperation::new(&mut self.worker);
        operation
            .worker
            .request(
                &control.context(),
                Kind::CheckMonitor,
                Vec::new(),
                control.deadline(Duration::from_secs(2))?,
            )
            .await?;
        control.check()?;
        operation.completed = true;
        guard.finish();
        Ok(())
    }
}
