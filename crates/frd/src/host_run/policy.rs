//! Saved local policy on the installed host's original lifetime. Filesystem
//! reads stay in the existing bounded Watch; transport checks never do disk I/O.
use super::Error;
use crate::host_policy::{Approval, Sharing, Store, live};
use asupersync::{cx::Cx, time::sleep};
use std::{path::PathBuf, time::Duration};

/// Explicit live-policy opt-in. Overrides are local process choices, never
/// writes to the Store; observed disk revisions still retire previous grants.
#[derive(Debug, Clone)]
pub struct Configuration {
    pub path: PathBuf,
    pub approval: Option<Approval>,
    pub sharing: Option<Sharing>,
}

pub(super) struct Owner {
    watch: Option<live::Watch>,
    handle: Option<live::Handle>,
}
impl Owner {
    pub(super) fn start(cx: &Cx, configuration: Option<Configuration>) -> Result<Self, Error> {
        let Some(configuration) = configuration else {
            return Ok(Self {
                watch: None,
                handle: None,
            });
        };
        let store =
            Store::new(&configuration.path).map_err(|e| Error::Policy(live::Error::Store(e)))?;
        let watch = live::Watch::start(cx, store).map_err(Error::Policy)?;
        let handle = watch
            .handle()
            .with_overrides(configuration.approval, configuration.sharing);
        Ok(Self {
            watch: Some(watch),
            handle: Some(handle),
        })
    }
    pub(super) fn handle(&self) -> Option<&live::Handle> {
        self.handle.as_ref()
    }
    /// No credentials, listener or capture before first complete local evidence.
    /// Watch itself bounds opening and every later read by its original clock.
    pub(super) async fn ready(&self, cx: &Cx) -> Result<(), Error> {
        let Some(handle) = &self.handle else {
            return Ok(());
        };
        loop {
            match handle.status() {
                live::Status::Opening => sleep(cx.now(), Duration::from_millis(10)).await,
                live::Status::Active(_) => return lease(Some(handle)).map(|_| ()),
                live::Status::Stopped(error) => return Err(Error::Policy(error)),
            }
        }
    }
    /// A stopped flag is not thread exit. Preserve the global disk-worker permit
    /// if a native filesystem call hangs; do not start an unbounded replacement.
    pub(super) async fn finish(&mut self, cx: &Cx) -> Result<(), Error> {
        let Some(watch) = &mut self.watch else {
            return Ok(());
        };
        watch.stop();
        let until = cx
            .now()
            .as_nanos()
            .checked_add(1_000_000_000)
            .ok_or(Error::Runtime)?;
        loop {
            if let Some(result) = watch.try_finish() {
                return result.map_err(|_| Error::Cleanup("policy"));
            }
            if cx.is_cancel_requested() || cx.now().as_nanos() >= until {
                return Err(Error::Cleanup("policy"));
            }
            sleep(cx.now(), Duration::from_millis(5)).await;
        }
    }
}

/// Snapshot one source epoch. A source never adopts a new revision in place,
/// even if command-line overrides keep its effective settings unchanged.
pub(super) fn lease(handle: Option<&live::Handle>) -> Result<Option<live::Lease>, Error> {
    handle
        .map(|handle| {
            let lease = handle.lease().map_err(Error::Policy)?;
            let policy = lease.check().map_err(Error::Policy)?;
            if policy.approval_mode == Approval::Local {
                // The installed observation-only host has no prompt process yet.
                // Never silently turn this new local requirement into unattended.
                return Err(Error::LocalApprovalUnavailable);
            }
            Ok(lease)
        })
        .transpose()
}
