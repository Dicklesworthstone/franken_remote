//! Explicit file choices for one macOS controlling connection.
//!
//! This module contains no `AppKit` or alternate transfer loop. The original
//! `NativeObserver` owns the existing ATP lanes, disk workers, authority checks
//! and publication receipts. This owner hands it each preselected request once
//! and retains the bounded status/control handles after that handoff. An
//! explicitly enabled local command endpoint owns only selection admission
//! into that same original lane, never another connection or transfer loop.
//! A reconnect or a later control gesture never repeats the file choices.
use crate::{file_selection::BatchStatus, macos_file_drop};
use fr_wire::negotiation::Offer;
use frd::{
    native_files::{
        Absence, Directory, ReceiveControl, ReceiveRequest, ReceiveStatus, SelectRefusal,
        Selection, SendControl, SendIntake, SendRequest, SendSnapshot, local,
    },
    session_startup::{FileSendError, NativeObserver},
};
use std::fmt;

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

#[derive(Debug)]
enum Upload {
    Selected(SendRequest),
    Deferred,
}

#[derive(Default)]
struct Commands {
    connection: Option<String>,
    outcome: Option<Result<local::Results, local::Error>>,
    owner: Option<local::Owner>,
    cancelled: bool,
}
impl fmt::Debug for Commands {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Commands")
            .field("connection_selected", &self.connection.is_some())
            .field("configured", &self.outcome.is_some())
            .field("owner_retained", &self.owner.is_some())
            .field("cancelled", &self.cancelled)
            .finish()
    }
}
impl Commands {
    fn set_connection(&mut self, connection: &str) -> Result<(), local::Error> {
        if self.connection.is_some() || self.outcome.is_some() || self.cancelled {
            return Err(local::Error::Closed);
        }
        // This is the exact original foreground owner's public comparison ID,
        // not a host label, path or credential. The IPC repeats this check.
        if connection.len() != 32 || !connection.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(local::Error::InvalidTarget);
        }
        self.connection = Some(connection.to_owned());
        Ok(())
    }

    fn stop(&mut self) {
        self.cancelled = true;
        if let Some(owner) = &self.owner {
            // Fences queued selections and each admitted batch synchronously.
            // The owner and read-only results remain retained until cleanup.
            owner.stop();
        }
        if self.outcome.is_none() {
            self.outcome = Some(Err(local::Error::Closed));
        }
    }

    fn configure(&mut self, intake: SendIntake, cancelled: bool) -> bool {
        if cancelled || self.cancelled {
            self.stop();
            return false;
        }
        let owner = self
            .connection
            .take()
            .ok_or(local::Error::Unavailable)
            .and_then(|connection| local::Owner::bind_upload(&connection, intake));
        match owner {
            Ok(owner) => {
                self.outcome = Some(Ok(owner.results()));
                self.owner = Some(owner);
                true
            }
            Err(error) => {
                self.outcome = Some(Err(error));
                false
            }
        }
    }
}

/// Locally selected upload and download, with independent consent and results.
/// Construct before network startup; configure on the first opened observer,
/// before its interactive service. No source contents are read by this owner.
#[derive(Debug, Default)]
pub struct Files {
    upload: Option<Direction<Upload, SendControl>>,
    sizes: Vec<Option<u64>>,
    download: Option<Direction<ReceiveRequest, ReceiveControl>>,
    commands: Option<Commands>,
    selections: Option<macos_file_drop::Owner>,
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
            (
                Some(Direction::new(Upload::Selected(selection.request()?))),
                sizes,
            )
        } else {
            (None, Vec::new())
        };
        Ok(Self {
            upload,
            sizes,
            download: download.map(|directory| Direction::new(ReceiveRequest::new(&directory))),
            commands: None,
            selections: None,
        })
    }

    /// Opt in to later explicit selections on the first configured upload.
    /// With no initial sources, negotiate an empty lane whose contents remain
    /// absent until a same-user command names a new bounded selection.
    #[must_use]
    pub fn with_commands(mut self) -> Self {
        self.commands.get_or_insert_with(Commands::default);
        self.upload
            .get_or_insert_with(|| Direction::new(Upload::Deferred));
        self
    }

    /// Explicit native selection consent for this original upload only. No
    /// pasteboard or path is read here; reconnect cannot renew this owner.
    #[must_use]
    pub fn with_file_drop(mut self) -> Self {
        self.selections
            .get_or_insert_with(macos_file_drop::Owner::default);
        self.upload
            .get_or_insert_with(|| Direction::new(Upload::Deferred));
        self
    }

    pub const fn file_drop_requested(&self) -> bool {
        self.selections.is_some()
    }

    pub(crate) fn file_drop_ingress(&self) -> Option<macos_file_drop::Ingress> {
        self.selections
            .as_ref()
            .map(macos_file_drop::Owner::ingress)
    }

    pub(crate) fn file_drop_ready(&self, ready: bool) {
        if let Some(selections) = &self.selections {
            selections.ingress().set_ready(ready);
        }
    }

    /// Independent results for every admitted native gesture, in selection
    /// order. The original preselected and local-command receipts stay intact.
    pub fn dropped_batches(&self) -> Vec<BatchStatus> {
        self.selections
            .as_ref()
            .map_or_else(Vec::new, macos_file_drop::Owner::batches)
    }

    pub(crate) fn drop_progress(&self) -> Option<Result<SendSnapshot, &'static str>> {
        self.selections.as_ref()?.progress()
    }

    /// Bind command consent to the original foreground connection exactly once,
    /// before observer configuration. Reconnects cannot change that identity.
    pub fn command_connection(&mut self, connection: &str) -> Result<(), local::Error> {
        self.commands
            .as_mut()
            .map_or(Ok(()), |commands| commands.set_connection(connection))
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
        let pending = self.upload.as_mut().and_then(|upload| {
            upload
                .pending
                .take()
                .map(|request| (request, upload.pending_cancelled))
        });
        if let Some((request, cancelled)) = pending {
            let configured = self.configure_upload(observer, request, cancelled);
            self.upload
                .as_mut()
                .expect("original upload")
                .configured(configured, SendControl::cancel);
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

    fn configure_upload(
        &mut self,
        observer: &mut NativeObserver,
        request: Upload,
        cancelled: bool,
    ) -> Result<SendControl, Absence> {
        if self.commands.is_none() && self.selections.is_none() {
            return match request {
                Upload::Selected(request) => observer.configure_file_send(request),
                Upload::Deferred => Err(Absence::Unavailable),
            };
        }
        let deferred = matches!(request, Upload::Deferred);
        // Configure exactly once. Every optional local selector shares this
        // original bounded intake, immutable policy and authority checks.
        let configured = match request {
            Upload::Selected(request) => observer.configure_file_send_with_intake(request),
            Upload::Deferred => observer.configure_file_drop(),
        };
        let (control, intake) = match configured {
            Ok(configured) => configured,
            Err(absence) => {
                if let Some(commands) = &mut self.commands {
                    commands
                        .outcome
                        .get_or_insert(Err(local::Error::Unavailable));
                }
                if let Some(selections) = &self.selections {
                    selections.cancel();
                }
                return Err(absence);
            }
        };
        if cancelled {
            control.cancel();
            if let Some(commands) = &mut self.commands {
                commands.stop();
            }
            if let Some(selections) = &self.selections {
                selections.cancel();
            }
            return Ok(control);
        }
        let commands = self
            .commands
            .as_mut()
            .is_some_and(|commands| commands.configure(intake.clone(), false));
        let native = self.selections.as_mut().is_some_and(|selections| {
            if selections.configure(intake).is_ok() {
                true
            } else {
                selections.cancel();
                false
            }
        });
        // An initial --send remains useful when optional local UI setup fails.
        // A deferred lane needs at least one live explicit selection surface.
        if deferred && !commands && !native {
            control.cancel();
            return Err(Absence::Unavailable);
        }
        Ok(control)
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
        if let Some(selections) = &self.selections {
            selections.cancel();
        }
        if let Some(commands) = &mut self.commands {
            commands.stop();
        }
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

    /// Read-only original command history. Pending setup is unavailable, never
    /// evidence that the optional endpoint or a remote file lane opened.
    pub fn command_results(&self) -> Option<Result<local::Results, local::Error>> {
        self.commands.as_ref().map(|commands| {
            commands
                .outcome
                .clone()
                .unwrap_or(Err(local::Error::Unavailable))
        })
    }

    /// Latest admitted batch for native progress. Its independent control and
    /// receipts never replace the initial selection's completion evidence.
    pub fn command_snapshot(&self) -> Option<SendSnapshot> {
        self.commands
            .as_ref()?
            .outcome
            .as_ref()?
            .as_ref()
            .ok()?
            .latest_snapshot()
    }

    /// Actual retirement of the original local socket and selection workers.
    /// A cancellation request alone cannot allow another native attempt.
    pub fn commands_complete(&self) -> bool {
        self.selections
            .as_ref()
            .is_none_or(macos_file_drop::Owner::complete)
            && self
                .commands
                .as_ref()
                .and_then(|commands| commands.owner.as_ref())
                .is_none_or(local::Owner::is_complete)
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
    fn native_selection_consent_is_explicit_combines_with_commands_and_never_reopens() {
        let fixture = Fixture::new();
        for selected in [false, true] {
            let mut files =
                Files::new(selected.then(|| fixture.upload()), Some(fixture.download()))
                    .unwrap()
                    .with_file_drop()
                    .with_commands();
            assert!(files.file_drop_requested());
            assert_eq!(files.upload_sizes().len(), usize::from(selected));
            assert!(files.dropped_batches().is_empty());
            assert!(files.drop_progress().is_none());
            assert!(files.commands_complete());
            assert!(has(&files.offer(false), fr_wire::files::DUPLEX_CAPABILITY));
            let ingress = files.file_drop_ingress().unwrap();
            files.file_drop_ready(true);
            // UI eligibility alone cannot manufacture an original intake.
            assert!(!ingress.ready());
            assert!(!ingress.submit(b"file:///tmp/not-selected", std::time::Instant::now()));
            files.cancel_upload();
            let files = files.with_file_drop().with_commands();
            files.file_drop_ready(true);
            assert!(!ingress.ready());
            assert!(!has(
                &files.offer(false),
                fr_wire::files::CHANNEL_SCOPE_CAPABILITY
            ));
            assert!(has(
                &files.offer(false),
                fr_wire::files::DOWNLOAD_CAPABILITY
            ));
            assert_eq!(files.download_status(), Some(Status::NotStarted));
            assert!(files.dropped_batches().is_empty());
            assert!(files.commands_complete());
        }
        assert!(!Files::default().file_drop_requested());
    }

    #[test]
    fn command_consent_opens_no_source_and_uses_the_original_upload_profile() {
        let fixture = Fixture::new();
        for download in [false, true] {
            let files = Files::new(None, download.then(|| fixture.download()))
                .unwrap()
                .with_commands();
            assert!(files.has_upload());
            assert_eq!(files.has_download(), download);
            assert_eq!(files.upload_sizes(), []);
            assert_eq!(files.upload_status(), Some(Status::NotStarted));
            assert!(files.upload_control().is_none());
            assert!(files.command_snapshot().is_none());
            assert!(files.commands_complete());
            assert!(matches!(
                files.command_results(),
                Some(Err(local::Error::Unavailable))
            ));
            let offer = files.offer(false);
            assert!(has(&offer, fr_wire::files::CHANNEL_SCOPE_CAPABILITY));
            assert_eq!(has(&offer, fr_wire::files::DOWNLOAD_CAPABILITY), download);
            assert_eq!(has(&offer, fr_wire::files::DUPLEX_CAPABILITY), download);
            assert!(!has(&offer, fr_wire::attachment::CLIPBOARD_CAPABILITY));
        }
    }

    #[test]
    fn command_connection_is_exact_and_cannot_be_replaced_or_reused_after_cancellation() {
        let mut files = Files::default().with_commands();
        for invalid in [
            "n-host",
            "../endpoint",
            "",
            "gggggggggggggggggggggggggggggggg",
        ] {
            assert_eq!(
                files.command_connection(invalid),
                Err(local::Error::InvalidTarget)
            );
        }
        let original = "a".repeat(32);
        files.command_connection(&original).unwrap();
        assert_eq!(
            files.command_connection(&"b".repeat(32)),
            Err(local::Error::Closed)
        );
        assert_eq!(
            files.commands.as_ref().unwrap().connection.as_deref(),
            Some(original.as_str())
        );
        files.cancel_upload();
        let mut files = files.with_commands();
        assert_eq!(
            files.command_connection(&original),
            Err(local::Error::Closed)
        );
        assert!(matches!(
            files.command_results(),
            Some(Err(local::Error::Closed))
        ));
        assert!(!has(
            &files.offer(false),
            fr_wire::attachment::FILES_CAPABILITY
        ));
    }

    #[test]
    fn spent_command_consent_never_reoffers_but_keeps_independent_download() {
        let fixture = Fixture::new();
        let mut files = Files::new(None, Some(fixture.download()))
            .unwrap()
            .with_commands();
        files.command_connection(&"a".repeat(32)).unwrap();
        // The same one-use move as configure: no platform endpoint, control
        // grant, source contents or remote publication is simulated here.
        assert!(matches!(
            files.upload.as_mut().unwrap().pending.take(),
            Some(Upload::Deferred)
        ));
        files.commands.as_mut().unwrap().outcome = Some(Err(local::Error::Unavailable));
        let mut files = files.with_commands();
        assert_eq!(
            files.command_connection(&"b".repeat(32)),
            Err(local::Error::Closed)
        );
        let offer = files.offer(false);
        assert!(!has(&offer, fr_wire::files::CHANNEL_SCOPE_CAPABILITY));
        assert!(has(&offer, fr_wire::files::DOWNLOAD_CAPABILITY));
        assert!(!has(&offer, fr_wire::files::DUPLEX_CAPABILITY));
        assert!(files.upload.as_ref().unwrap().pending.is_none());
        assert_eq!(files.download_status(), Some(Status::NotStarted));
    }

    #[test]
    fn upload_cancel_retires_commands_without_cancelling_the_original_destination() {
        let fixture = Fixture::new();
        let mut files = Files::new(Some(fixture.upload()), Some(fixture.download()))
            .unwrap()
            .with_commands();
        files.cancel_upload();
        files.cancel_upload();
        assert_eq!(files.upload_sizes(), [Some(8)]);
        assert_eq!(
            files.upload_status(),
            Some(Status::Unavailable(Absence::Refused(
                FileSendError::Cancelled
            )))
        );
        assert_eq!(files.download_status(), Some(Status::NotStarted));
        assert!(matches!(
            files.command_results(),
            Some(Err(local::Error::Closed))
        ));
        assert!(files.commands_complete());
        let offer = files.offer(false);
        assert!(!has(&offer, fr_wire::files::CHANNEL_SCOPE_CAPABILITY));
        assert!(has(&offer, fr_wire::files::DOWNLOAD_CAPABILITY));
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
