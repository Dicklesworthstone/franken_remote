//! Upload selections use the same real broker, request owner and cleanup path.
use super::*;

impl Portal {
    fn files(&self) -> FilePicker {
        FilePicker(
            DirectoryPicker::open_mode_at(&self.address, bus::now().unwrap(), SelectionMode::Files)
                .unwrap(),
        )
    }
    fn wait_files(&self, picker: &mut FilePicker) {
        files_pending(picker, |_| self.state().opening.is_some());
    }
}

fn files_pending(picker: &mut FilePicker, ready: impl Fn(&DirectoryPicker) -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !ready(&picker.0) {
        assert!(Instant::now() < deadline, "upload picker did not progress");
        assert_eq!(picker.poll(), Ok(None));
        thread::sleep(Duration::from_millis(1));
    }
}

fn files_result(picker: &mut FilePicker) -> Result<Vec<PathBuf>, Error> {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "upload picker did not complete");
        match picker.poll() {
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Ok(Some(paths)) => return Ok(paths),
            Err(error) => return Err(error),
        }
    }
}

#[test]
fn real_portal_upload_requests_multiple_files_and_consumes_the_original_selection() {
    // File selection is available from interface v1; directory selection keeps
    // its existing v3 gate and unchanged single-directory request.
    let portal = Portal::start(1, true);
    let mut picker = portal.files();
    portal.wait_files(&mut picker);
    let opening = portal.state().opening.unwrap();
    assert!(!opening.directory);
    assert!(opening.multiple);
    portal.reply(false);
    portal.chosen(vec![
        "file:///tmp/first",
        "file://localhost/tmp/second%20file",
        "file:///tmp/%E6%96%87%E4%BB%B6",
        "file:///tmp/fourth",
        "file:///tmp/fifth",
        "file:///tmp/sixth",
        "file:///tmp/seventh",
        "file:///tmp/eighth",
    ]);
    assert_eq!(
        files_result(&mut picker),
        Ok([
            "/tmp/first",
            "/tmp/second file",
            "/tmp/文件",
            "/tmp/fourth",
            "/tmp/fifth",
            "/tmp/sixth",
            "/tmp/seventh",
            "/tmp/eighth",
        ]
        .map(PathBuf::from)
        .to_vec())
    );
    assert!(picker.0.bus.is_none());
    assert_eq!(picker.poll(), Err(Error::AlreadyUsed));
    assert_eq!(picker.close(), Ok(()));
    assert_eq!(portal.state().close_paths, Vec::<String>::new());
    assert!(!format!("{picker:?}").contains("/tmp"));
}

#[test]
fn real_upload_response_rejects_empty_remote_duplicate_and_oversized_selections() {
    for uris in [
        vec![],
        vec!["file://remote/tmp/a"],
        vec!["file:///tmp/a", "file:///tmp/%61"],
        vec!["file:///tmp/a\nfile:///tmp/b"],
        vec!["file:///tmp/../a"],
        vec!["file:///tmp/%2fa"],
        vec![
            "file:///tmp/a",
            "file:///tmp/b",
            "file:///tmp/c",
            "file:///tmp/d",
            "file:///tmp/e",
            "file:///tmp/f",
            "file:///tmp/g",
            "file:///tmp/h",
            "file:///tmp/i",
        ],
    ] {
        let portal = Portal::start(1, true);
        let mut picker = portal.files();
        portal.wait_files(&mut picker);
        portal.reply(false);
        portal.chosen(uris);
        assert_eq!(files_result(&mut picker), Err(Error::InvalidSelection));
        assert_eq!(picker.close(), Ok(()));
        assert_eq!(portal.state().close_paths, Vec::<String>::new());
    }
    // Each decoded path is individually within 4096 bytes, but the combined
    // encoded selection exceeds the original 32 KiB intake budget.
    let uris: Vec<_> = (0..8)
        .map(|n| format!("file:///tmp/{n}{}", "a".repeat(4088)))
        .collect();
    let portal = Portal::start(1, true);
    let mut picker = portal.files();
    portal.wait_files(&mut picker);
    portal.reply(false);
    portal.chosen(uris.iter().map(String::as_str).collect());
    assert_eq!(files_result(&mut picker), Err(Error::InvalidSelection));
    assert_eq!(picker.close(), Ok(()));
}

#[test]
fn early_upload_response_waits_for_its_open_reply_and_expires_without_replay() {
    for expired in [false, true] {
        let portal = Portal::start(1, true);
        let mut picker = portal.files();
        portal.wait_files(&mut picker);
        portal.chosen(vec!["file:///tmp/first", "file:///tmp/second"]);
        files_pending(&mut picker, |owner| owner.response_received);
        assert!(matches!(picker.0.phase, Phase::Opening(_)));
        assert_eq!(picker.poll(), Ok(None));
        if expired {
            picker.0.response_at = Some(bus::now().unwrap().checked_sub(2_000_000).unwrap());
        }
        portal.reply(false);
        assert_eq!(
            files_result(&mut picker),
            if expired {
                Err(Error::Expired)
            } else {
                Ok(vec![
                    PathBuf::from("/tmp/first"),
                    PathBuf::from("/tmp/second"),
                ])
            }
        );
        assert_eq!(
            picker.poll(),
            Err(if expired {
                Error::Expired
            } else {
                Error::AlreadyUsed
            })
        );
        assert_eq!(picker.close(), Ok(()));
        assert!(picker.0.bus.is_none());
    }
}

#[test]
fn cancelling_upload_open_settles_and_closes_the_original_request() {
    let portal = Portal::start(1, true);
    let mut picker = portal.files();
    portal.wait_files(&mut picker);
    portal
        .actions
        .send(Action::Reply {
            replacement: false,
            delay: Duration::from_millis(40),
        })
        .unwrap();
    assert_eq!(picker.close(), Ok(()));
    let observed = portal.state();
    assert_eq!(observed.opening_replies, 1);
    assert_eq!(
        observed.close_paths,
        [observed.opening.unwrap().path.to_str().unwrap()]
    );
    assert!(!observed.close_before_open_reply);
    assert_eq!(picker.poll(), Err(Error::Cancelled));
}

#[test]
fn replaced_upload_request_never_releases_an_early_file_selection() {
    let portal = Portal::start(1, true);
    let mut picker = portal.files();
    portal.wait_files(&mut picker);
    portal.chosen(vec!["file:///tmp/first", "file:///tmp/second"]);
    files_pending(&mut picker, |owner| owner.response_received);
    portal.reply(true);
    assert_eq!(files_result(&mut picker), Err(Error::InvalidSelection));
    assert_eq!(picker.close(), Ok(()));
    assert_eq!(
        portal.state().close_paths,
        ["/org/freedesktop/portal/desktop/request/actual/replacement"]
    );
}

#[test]
fn unknown_file_chooser_version_refuses_before_opening() {
    let portal = Portal::start(0, true);
    let mut picker = portal.files();
    assert_eq!(files_result(&mut picker), Err(Error::Unavailable));
    assert!(portal.state().opening.is_none());
    assert_eq!(picker.close(), Ok(()));
}

#[test]
fn upload_responses_from_another_owner_or_request_cannot_complete_the_choice() {
    let portal = Portal::start(1, true);
    let mut picker = portal.files();
    portal.wait_files(&mut picker);
    portal.reply(false);
    files_pending(&mut picker, |owner| owner.phase == Phase::Waiting);
    let original_messages = picker.0.messages;
    for (foreign, wrong_path) in [(true, false), (false, true)] {
        portal
            .actions
            .send(Action::Response {
                uris: vec!["file:///tmp/foreign".to_owned()],
                code: 0,
                foreign,
                wrong_path,
            })
            .unwrap();
    }
    files_pending(&mut picker, |owner| owner.messages >= original_messages + 2);
    assert!(!picker.0.response_received);
    portal.chosen(vec!["file:///tmp/original"]);
    assert_eq!(
        files_result(&mut picker),
        Ok(vec![PathBuf::from("/tmp/original")])
    );
}

#[test]
fn native_upload_cancellation_ignores_any_accompanying_files() {
    let portal = Portal::start(1, true);
    let mut picker = portal.files();
    portal.wait_files(&mut picker);
    portal.reply(false);
    portal
        .actions
        .send(Action::Response {
            uris: vec!["file:///tmp/not-consented".to_owned()],
            code: 1,
            foreign: false,
            wrong_path: false,
        })
        .unwrap();
    assert_eq!(files_result(&mut picker), Err(Error::Cancelled));
    assert!(picker.0.bus.is_none());
    assert_eq!(picker.close(), Ok(()));
}

#[test]
fn unacknowledged_upload_cleanup_remains_terminal() {
    let portal = Portal::start(1, false);
    let mut picker = portal.files();
    portal.wait_files(&mut picker);
    portal.reply(false);
    files_pending(&mut picker, |owner| owner.phase == Phase::Waiting);
    assert_eq!(picker.close(), Err(Error::CleanupIncomplete));
    assert_eq!(picker.close(), Err(Error::CleanupIncomplete));
    assert_eq!(picker.poll(), Err(Error::CleanupIncomplete));
    assert!(picker.0.bus.is_none());
    assert_eq!(portal.state().close_paths.len(), 1);
}
