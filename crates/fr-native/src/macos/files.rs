//! Explicit file choices for one macOS controlling connection.
//!
//! This module contains no `AppKit` or alternate transfer loop. The original
//! `NativeObserver` owns the existing ATP lanes, disk workers, authority checks
//! and publication receipts. This owner hands it each preselected request once
//! and retains only the bounded status/control handles after that handoff.
//! A reconnect or a later control gesture never repeats the file choices.
use fr_wire::negotiation::Offer;
use frd::{
    native_files::{
        Absence, Directory, ReceiveControl, ReceiveRequest, ReceiveStatus, SelectRefusal,
        Selection, SendControl, SendRequest, SendSnapshot,
    },
    session_startup::{FileSendError, NativeObserver},
};

/// The original local setup, separate from the transfer's current progress.
/// `Configured` does not mean that control was granted or a disk owner started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status<T> {
    NotStarted,
    Unavailable(Absence),
    Configured(T),
}

#[derive(Debug)]
struct Direction<Request, Control> {
    pending: Option<Request>,
    // A cancelled request is retained only to settle an already negotiated
    // attachment. It is never offered again and never starts source contents.
    pending_cancelled: bool,
    outcome: Option<Result<Control, Absence>>,
}
impl<Request, Control> Direction<Request, Control> {
    fn new(request: Request) -> Self {
        Self {
            pending: Some(request),
            pending_cancelled: false,
            outcome: None,
        }
    }

    fn control(&self) -> Option<&Control> {
        self.outcome
            .as_ref()
            .and_then(|result| result.as_ref().ok())
    }

    fn status<T>(&self, sample: impl FnOnce(&Control) -> T) -> Status<T> {
        match &self.outcome {
            None => Status::NotStarted,
            Some(Err(absence)) => Status::Unavailable(*absence),
            Some(Ok(control)) => Status::Configured(sample(control)),
        }
    }

    fn will_offer(&self) -> bool {
        self.pending.is_some() && !self.pending_cancelled
    }

    fn cancel_pending(&mut self) {
        if self.pending.is_some() {
            self.pending_cancelled = true;
            self.outcome = Some(Err(Absence::Refused(FileSendError::Cancelled)));
        }
    }

    fn configured(
        &mut self,
        result: Result<Control, Absence>,
        cancel: impl FnOnce(&Control) -> bool,
    ) {
        match result {
            Ok(control) => {
                // Configuration is synchronous and precedes every service turn.
                // This cancellation reaches the original core expectation
                // before it can start any source or admit a received file.
                if self.pending_cancelled {
                    cancel(&control);
                }
                self.outcome = Some(Ok(control));
            }
            Err(absence) if self.outcome.is_none() => self.outcome = Some(Err(absence)),
            // A preconfiguration local cancellation remains the original cause
            // if a later, unoffered observer refuses that already spent choice.
            Err(_) => {}
        }
    }
}

/// Locally selected upload and download, with independent consent and results.
/// Construct before network startup; configure on the first opened observer,
/// before its interactive service. No source contents are read by this owner.
#[derive(Debug, Default)]
pub struct Files {
    upload: Option<Direction<SendRequest, SendControl>>,
    sizes: Vec<Option<u64>>,
    download: Option<Direction<ReceiveRequest, ReceiveControl>>,
}
impl Files {
    /// Duplicate the selected source descriptors and retain the pinned local
    /// destination. Failure precedes the connection and carries no path/name.
    pub fn new(
        upload: Option<Selection>,
        download: Option<Directory>,
    ) -> Result<Self, SelectRefusal> {
        let (upload, sizes) = if let Some(selection) = upload {
            let sizes = selection.sizes().collect();
            (Some(Direction::new(selection.request()?)), sizes)
        } else {
            (None, Vec::new())
        };
        Ok(Self {
            upload,
            sizes,
            download: download.map(|directory| Direction::new(ReceiveRequest::new(&directory))),
        })
    }

    /// Offer only directions whose explicit local request is still unspent.
    /// Reuse the existing optional profiles, including positive duplex
    /// negotiation. The independent clipboard choice grants no file access.
    pub fn offer(&self, clipboard: bool) -> Offer {
        let upload = self.upload.as_ref().is_some_and(Direction::will_offer);
        let download = self.download.as_ref().is_some_and(Direction::will_offer);
        match (upload, download, clipboard) {
            (true, true, _) => fr_client::native::control_offer_with_duplex(clipboard, false),
            (false, true, _) => fr_client::native::control_offer_with_download(clipboard, false),
            (true, false, true) => fr_client::native::control_offer_with_clipboard_and_files(),
            (true, false, false) => fr_client::native::control_offer_with_files(),
            (false, false, true) => fr_client::native::control_offer_with_clipboard(),
            (false, false, false) => fr_client::native::control_offer(),
        }
    }

    /// Consume each original choice once, including a refused setup. A local
    /// cancellation during decoder startup must still install and immediately
    /// cancel the original expectation: omitting a negotiated lane would make
    /// the host's later attachment offer fail the whole controlled session.
    /// The existing observer gates all contents on the original control grant.
    pub fn configure(&mut self, observer: &mut NativeObserver) {
        if let Some(upload) = &mut self.upload
            && let Some(request) = upload.pending.take()
        {
            upload.configured(observer.configure_file_send(request), SendControl::cancel);
        }
        if let Some(download) = &mut self.download
            && let Some(request) = download.pending.take()
        {
            download.configured(
                observer.request_receive_files(request),
                ReceiveControl::cancel,
            );
        }
    }

    /// Whether this invocation explicitly selected an upload. Cancellation
    /// changes its state, not the fact that its original results must be shown.
    pub const fn has_upload(&self) -> bool {
        self.upload.is_some()
    }

    /// Whether this invocation explicitly selected a local destination.
    pub const fn has_download(&self) -> bool {
        self.download.is_some()
    }

    /// Fence only the original upload, including before configuration.
    /// The independent download choice and its receipts are unchanged.
    pub fn cancel_upload(&mut self) {
        if let Some(upload) = &mut self.upload {
            upload.cancel_pending();
            if let Some(control) = upload.control() {
                control.cancel();
            }
        }
    }

    /// Fence only the original download, including before configuration.
    /// The independent upload choice and its receipts are unchanged.
    pub fn cancel_download(&mut self) {
        if let Some(download) = &mut self.download {
            download.cancel_pending();
            if let Some(control) = download.control() {
                control.cancel();
            }
        }
    }

    /// Fence both choices synchronously, before any cleanup await. Existing
    /// receipts remain authoritative. This requests cancellation only; the
    /// session must retain its observer and call `reap_files` until it succeeds.
    pub fn cancel(&mut self) {
        self.cancel_upload();
        self.cancel_download();
    }

    /// Original selection sizes only. Directories remain unknown until the
    /// authorized sender walks them; an inode length is not content size.
    pub fn upload_sizes(&self) -> &[Option<u64>] {
        &self.sizes
    }

    /// Status/cancel/pacing of the original upload; never a resubmission handle.
    pub fn upload_control(&self) -> Option<SendControl> {
        self.upload.as_ref().and_then(Direction::control).cloned()
    }

    /// Status/cancel of the original download; cannot select another directory.
    pub fn download_control(&self) -> Option<ReceiveControl> {
        self.download.as_ref().and_then(Direction::control).cloned()
    }

    /// `None` means no upload was selected. A snapshot is not a publication
    /// receipt; only its actual batch outcomes can establish remote effects.
    pub fn upload_status(&self) -> Option<Status<SendSnapshot>> {
        self.upload
            .as_ref()
            .map(|lane| lane.status(SendControl::snapshot))
    }

    /// `None` means no destination was selected. Retained results distinguish
    /// staged bytes, atomic publication, durability and actual worker cleanup.
    pub fn download_status(&self) -> Option<Status<ReceiveStatus>> {
        self.download
            .as_ref()
            .map(|lane| lane.status(ReceiveControl::status))
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frd::native_files::{Limits, SelectReason};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "fr-macos-files-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(root.join("selected.txt"), b"selected").unwrap();
            Self(root)
        }

        fn upload(&self) -> Selection {
            Selection::select(&[self.0.join("selected.txt")]).unwrap()
        }

        fn download(&self) -> Directory {
            Directory::open(&self.0, Limits::default()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0.join("alias"));
            let _ = fs::remove_file(self.0.join("selected.txt"));
            let _ = fs::remove_dir(&self.0);
        }
    }

    fn has(offer: &Offer, name: &str) -> bool {
        offer
            .capabilities
            .iter()
            .any(|capability| capability.name == name)
    }

    #[test]
    fn absent_file_choices_never_offer_a_file_direction() {
        let files = Files::default();
        assert!(files.upload_status().is_none() && files.download_status().is_none());
        assert!(files.upload_control().is_none() && files.download_control().is_none());
        for clipboard in [false, true] {
            let offer = files.offer(clipboard);
            assert!(!has(&offer, fr_wire::attachment::FILES_CAPABILITY));
            assert!(!has(&offer, fr_wire::files::CAPABILITY));
            assert!(!has(&offer, fr_wire::files::DUPLEX_CAPABILITY));
            assert_eq!(
                has(&offer, fr_wire::attachment::CLIPBOARD_CAPABILITY),
                clipboard
            );
        }
    }

    #[test]
    fn preselected_directions_use_existing_optional_profiles_without_starting_owners() {
        let fixture = Fixture::new();
        for (upload, download) in [(true, false), (false, true), (true, true)] {
            let files = Files::new(
                upload.then(|| fixture.upload()),
                download.then(|| fixture.download()),
            )
            .unwrap();
            assert_eq!(files.upload_status().is_some(), upload);
            assert_eq!(files.download_status().is_some(), download);
            assert!(files.upload_control().is_none() && files.download_control().is_none());
            assert_eq!(
                files.upload_sizes(),
                if upload { &[Some(8)][..] } else { &[] }
            );
            for clipboard in [false, true] {
                let offer = files.offer(clipboard);
                assert!(has(&offer, fr_wire::attachment::FILES_CAPABILITY));
                assert!(has(&offer, fr_wire::files::CAPABILITY));
                assert_eq!(
                    has(&offer, fr_wire::files::CHANNEL_SCOPE_CAPABILITY),
                    upload
                );
                assert_eq!(has(&offer, fr_wire::files::DOWNLOAD_CAPABILITY), download);
                assert_eq!(
                    has(&offer, fr_wire::files::DUPLEX_CAPABILITY),
                    upload && download
                );
                assert_eq!(
                    has(&offer, fr_wire::attachment::CLIPBOARD_CAPABILITY),
                    clipboard
                );
                assert!(offer.capabilities.len() <= 24);
            }
        }
    }

    #[test]
    fn consuming_a_request_prevents_reoffering_or_replaying_it() {
        let fixture = Fixture::new();
        let mut files = Files::new(Some(fixture.upload()), Some(fixture.download())).unwrap();
        // Exercise the same ownership move configure uses, with the actual
        // pinned requests. No endpoint, control grant or transfer is simulated.
        let upload = files.upload.as_mut().unwrap().pending.take().unwrap();
        assert!(!has(
            &files.offer(false),
            fr_wire::files::CHANNEL_SCOPE_CAPABILITY
        ));
        assert!(has(
            &files.offer(false),
            fr_wire::files::DOWNLOAD_CAPABILITY
        ));
        assert!(!has(&files.offer(false), fr_wire::files::DUPLEX_CAPABILITY));
        let download = files.download.as_mut().unwrap().pending.take().unwrap();
        assert!(!has(
            &files.offer(false),
            fr_wire::attachment::FILES_CAPABILITY
        ));
        assert!(files.upload.as_mut().unwrap().pending.take().is_none());
        assert!(files.download.as_mut().unwrap().pending.take().is_none());
        drop((upload, download));
        assert_eq!(files.upload_sizes(), [Some(8)]);
    }

    #[test]
    fn cancellation_before_configuration_retires_both_choices_and_preserves_sizes() {
        let fixture = Fixture::new();
        let mut files = Files::new(Some(fixture.upload()), Some(fixture.download())).unwrap();
        files.cancel();
        files.cancel();
        let cancelled = Absence::Refused(FileSendError::Cancelled);
        // Keep the actual pinned requests only for the synchronous
        // configure-then-cancel handoff, never a fresh offer or source read.
        assert!(files.upload.as_ref().unwrap().pending.is_some());
        assert!(files.download.as_ref().unwrap().pending.is_some());
        assert!(files.upload.as_ref().unwrap().pending_cancelled);
        assert!(files.download.as_ref().unwrap().pending_cancelled);
        assert_eq!(files.upload_status(), Some(Status::Unavailable(cancelled)));
        assert_eq!(
            files.download_status(),
            Some(Status::Unavailable(cancelled))
        );
        assert_eq!(files.upload_sizes(), [Some(8)]);
        assert!(!has(
            &files.offer(false),
            fr_wire::attachment::FILES_CAPABILITY
        ));
        assert!(files.upload_control().is_none() && files.download_control().is_none());
    }

    #[test]
    fn cancelling_one_direction_preserves_the_other_original_choice() {
        let fixture = Fixture::new();
        for cancel_upload in [false, true] {
            let mut files = Files::new(Some(fixture.upload()), Some(fixture.download())).unwrap();
            if cancel_upload {
                files.cancel_upload();
                files.cancel_upload();
            } else {
                files.cancel_download();
                files.cancel_download();
            }
            assert!(files.has_upload() && files.has_download());
            assert_eq!(files.upload_sizes(), [Some(8)]);
            assert_eq!(
                has(
                    &files.offer(false),
                    fr_wire::files::CHANNEL_SCOPE_CAPABILITY
                ),
                !cancel_upload
            );
            assert_eq!(
                has(&files.offer(false), fr_wire::files::DOWNLOAD_CAPABILITY),
                cancel_upload
            );
            assert!(!has(&files.offer(false), fr_wire::files::DUPLEX_CAPABILITY));
            let cancelled = Absence::Refused(FileSendError::Cancelled);
            if cancel_upload {
                assert_eq!(files.upload_status(), Some(Status::Unavailable(cancelled)));
                assert_eq!(files.download_status(), Some(Status::NotStarted));
            } else {
                assert_eq!(
                    files.download_status(),
                    Some(Status::Unavailable(cancelled))
                );
                assert_eq!(files.upload_status(), Some(Status::NotStarted));
            }
        }
    }

    #[test]
    fn unsafe_source_and_destination_refuse_before_any_configuration() {
        let fixture = Fixture::new();
        let alias = fixture.0.join("alias");
        std::os::unix::fs::symlink(fixture.0.join("selected.txt"), &alias).unwrap();
        assert_eq!(
            Selection::select(std::slice::from_ref(&alias))
                .unwrap_err()
                .reason,
            SelectReason::Symlink
        );
        assert_eq!(
            Directory::open(&alias, Limits::default()).unwrap_err(),
            frd::native_files::DirectoryRefusal::Symlink
        );
    }
}
