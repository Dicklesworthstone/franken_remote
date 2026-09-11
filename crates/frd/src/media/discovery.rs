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
use fr_wire::display::Catalog;
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
        let guard = SelectionGuard::new(&self.control);
        Ok(async move {
            let mut guard = guard;
            let mut discovery = self.discovery;
            let control = self.control;
            discovery
                .configure(
                    &control.context(),
                    choice,
                    configuration,
                    control.deadline(Duration::from_secs(2))?,
                )
                .await?;
            control.check()?;
            let worker = discovery.into_worker()?;
            guard.complete = true;
            Ok(CaptureSource {
                worker,
                configuration,
                next: Some(FrameId::FIRST),
                source: Arc::new(()),
                last_capture: None,
                selected_control: Some(control),
            })
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
