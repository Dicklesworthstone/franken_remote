//! Discover/configure retains the same private child and original native inventory.
use super::{Configuration, Cx, Deadline, Error, Kind, Launch, Role, State, Worker, worker};
use fr_core::limits::ProtocolLimits;
use fr_wire::display::{Catalog, Select};

/// One non-cloneable unconfigured capture worker. The catalog is a discovery
/// snapshot, not permission to capture or proof of an unchanged display today.
/// Keep servicing `check_display` during local choice; selection checks again.
/// Abort/reap uses the same bounded closing-worker slot as configured workers.
pub struct MonitorDiscovery {
    worker: Worker,
    catalog: Catalog,
    native_catalog: Catalog,
}
impl MonitorDiscovery {
    pub async fn start(cx: &Cx, launch: Launch, deadline: Deadline) -> Result<Self, Error> {
        if launch.role != Role::Capture {
            return Err(Error::Protocol(worker::Error::WrongRole));
        }
        super::runtime_ready(cx)?;
        if super::now(cx)? >= deadline.0 {
            return Err(Error::Deadline);
        }
        let mut worker = Worker::spawn(launch, ProtocolLimits::ABSOLUTE)?;
        let result = worker
            .exchange(cx, Kind::DiscoverMonitors, Vec::new(), deadline)
            .await?;
        let native_catalog =
            worker::capture::monitors::decode_catalog(result.body(), &worker.limits)?;
        let catalog = aliases(&native_catalog, &worker.limits)?;
        Ok(Self {
            worker,
            catalog,
            native_catalog,
        })
    }
    pub const fn catalog(&self) -> Catalog {
        self.catalog
    }
    pub fn id(&self) -> Option<u32> {
        self.worker.id()
    }
    pub const fn state(&self) -> State {
        self.worker.state()
    }
    /// No frame is captured; this receipt must never refresh pixel freshness.
    pub async fn check_display(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), Error> {
        if self.worker.state != State::Starting {
            return Err(Error::Unavailable);
        }
        self.worker
            .exchange(cx, Kind::CheckMonitor, Vec::new(), deadline)
            .await?;
        Ok(())
    }
    /// Consume the original catalog's exact full-display choice, without
    /// reopening Xlib. Refusals retain this child for explicit abort/reaping.
    pub async fn configure(
        &mut self,
        cx: &Cx,
        selection: Select,
        configuration: Configuration,
        deadline: Deadline,
    ) -> Result<(), Error> {
        if self.worker.state != State::Starting {
            return Err(Error::Unavailable);
        }
        let chosen = self
            .catalog
            .selected(selection)
            .map_err(|_| worker::Error::GeometryChanged)?;
        let position = self
            .catalog
            .displays()
            .iter()
            .position(|d| *d == chosen)
            .ok_or(Error::Unavailable)?;
        let native = self
            .native_catalog
            .selection(self.native_catalog.displays()[position].handle)
            .map_err(|_| Error::Unavailable)?;
        let body = worker::capture::monitors::encode_configuration(
            configuration,
            native,
            self.native_catalog,
        )?;
        let limits = configuration.limits()?;
        let reply = self
            .worker
            .exchange(cx, Kind::ConfigureMonitor, body.clone(), deadline)
            .await?;
        if reply.body() != body {
            self.worker.abort();
            return Err(Error::Protocol(worker::Error::WrongState));
        }
        self.worker.selected = Some(
            self.catalog
                .selected(selection)
                .map_err(|_| Error::Unavailable)?,
        );
        self.worker.limits = limits;
        self.worker.state = State::Running;
        Ok(())
    }
    pub fn into_worker(self) -> Result<Worker, Error> {
        if self.worker.state != State::Running || self.worker.selected.is_none() {
            return Err(Error::Unavailable);
        }
        Ok(self.worker)
    }
    /// Closing access only; never expose unrestricted requests or replace this
    /// worker while retaining the old discovery catalog as valid evidence.
    pub fn abort(&mut self) {
        self.worker.abort();
    }
    pub async fn stop(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), Error> {
        if self.worker.state != State::Starting {
            return Err(Error::Unavailable);
        }
        self.worker
            .exchange(cx, Kind::Stop, Vec::new(), deadline)
            .await?;
        Ok(())
    }
    pub async fn reap(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), Error> {
        self.worker.reap(cx, deadline).await.map(|_| ())
    }
}

// Disclosure aliases are not credentials. A process-local non-wrapping namespace
// prevents an old network selection from naming a replacement native inventory,
// even when its X server atoms, dimensions, worker epoch or local handle repeat.
static NEXT_CATALOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn aliases(native: &Catalog, limits: &ProtocolLimits) -> Result<Catalog, Error> {
    use std::sync::atomic::Ordering;
    let revision = NEXT_CATALOG
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| Error::Unavailable)?;
    let mut entries = native.displays().to_vec();
    for (i, entry) in entries.iter_mut().enumerate() {
        entry.handle = (u128::from(revision) << 8) | (i as u128 + 1);
    }
    Catalog::new(revision, &entries, limits).map_err(|_| Error::Protocol(worker::Error::Malformed))
}
