//! The controller explicitly selects the source for a host-advertised job.
use super::{ControlledViewer, Error, Permission, Policy};
use fr_files::sync::{JobStatus, SourceConfig};
use std::time::Duration;

impl ControlledViewer {
    /// Accept one reverse job using only the explicitly pinned local source.
    /// Normal control turns retain all transfer, authority and cleanup custody.
    pub fn expect_folder_upload(
        &mut self,
        source: SourceConfig,
        timeout: Duration,
    ) -> Result<(), Error> {
        self.expect_folder_upload_source(
            &mut Some(source),
            crate::native_files::SendControl::new(),
            timeout,
        )
    }
    pub(in crate::session_startup) fn expect_folder_upload_source(
        &mut self,
        source: &mut Option<SourceConfig>,
        control: crate::native_files::SendControl,
        timeout: Duration,
    ) -> Result<(), Error> {
        let mode = source.as_ref().ok_or(Error::Limits)?.mode();
        if !mode.upload_selected(&self.session.opened.selection) {
            return Err(Error::WrongRole);
        }
        // An ordinary download keeps its independent receive authority. Two
        // standing jobs still require a separate reconciliation contract.
        if self.files.download.folder_job {
            return Err(Error::Busy);
        }
        self.expect_file_drop(Permission::new(true), Policy::default(), timeout)?;
        // No await or I/O separates the original expectation from its source
        // handoff. A transient readiness refusal above retains the same source.
        let pending = self.files.upload.pending.as_mut().ok_or(Error::Closed)?;
        pending.destination =
            super::negotiation::Destination::Sync(source.take().ok_or(Error::Closed)?, control);
        self.files.upload.folder_job = true;
        Ok(())
    }
    pub fn file_upload_sync_status(&self) -> Option<JobStatus> {
        self.files.upload.sync_send.as_ref().map(|job| job.status())
    }
}
