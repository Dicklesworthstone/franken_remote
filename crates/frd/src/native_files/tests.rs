//! Real filesystem, real `fr_files` session owner and a real core input lease.
use super::*;
use asupersync::atp::object::ContentId;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, InputSession},
    time::HostInstant,
};
use fr_files::session::{Error as SessionError, HostReceiver, Offer};
use std::{
    fs,
    os::unix::fs::symlink,
    sync::atomic::{AtomicU64, Ordering},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new(mode: u32) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fr-native-files-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
        Self(dir)
    }
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}
impl std::ops::Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
/// A real core controller: observation, view readiness, a granted lease and a
/// ticket, all at t=0 with the plan's default (3 s provisional) deadlines.
fn controller() -> InputSession {
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut authority =
        SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(at(0)).unwrap();
    authority.mark_view_ready(at(0)).unwrap();
    authority.grant_lease(credentials.lease, at(0)).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, at(0))
        .unwrap();
    InputSession::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap()
}
fn receiver(directory: &Directory, input: &InputSession) -> HostReceiver {
    let configuration = directory.configuration();
    HostReceiver::new(
        input,
        configuration.directory,
        configuration.permission,
        configuration.policy,
        at(0),
    )
    .unwrap()
}
fn offer<'a>(receiver: &HostReceiver, id: u64, name: &'a str, bytes: &[u8]) -> Offer<'a> {
    Offer {
        binding: receiver.binding(),
        id,
        name,
        size: bytes.len() as u64,
        content: ContentId::from_bytes(bytes),
    }
}
fn limits(file: u64, session: u64) -> Limits {
    Limits {
        max_file_bytes: file,
        max_session_bytes: session,
    }
}

#[test]
fn only_an_owned_private_real_directory_is_accepted_by_type() {
    let good = Scratch::new(0o700);
    assert!(Directory::open(&good.0, Limits::default()).is_ok());
    // 0o750 is private enough: group may read, nobody else may write.
    let readable = Scratch::new(0o750);
    assert!(Directory::open(&readable.0, Limits::default()).is_ok());
    let refused = |path: &Path| Directory::open(path, Limits::default()).unwrap_err();
    assert_eq!(
        refused(Path::new("relative/dir")),
        DirectoryRefusal::Relative
    );
    assert_eq!(refused(&good.0.join("missing")), DirectoryRefusal::Missing);
    fs::write(good.0.join("plain"), b"x").unwrap();
    assert_eq!(
        refused(&good.0.join("plain")),
        DirectoryRefusal::NotDirectory
    );
    // A symlink to the good directory, and a real directory behind a
    // symlinked parent, both refuse: nothing is reached through a link.
    symlink(&good.0, good.0.join("link")).unwrap();
    assert_eq!(refused(&good.0.join("link")), DirectoryRefusal::Symlink);
    fs::create_dir(good.0.join("real")).unwrap();
    fs::create_dir(good.0.join("real").join("inner")).unwrap();
    symlink(good.0.join("real"), good.0.join("alias")).unwrap();
    assert_eq!(
        refused(&good.0.join("alias").join("inner")),
        DirectoryRefusal::Symlink
    );
    for mode in [0o777, 0o770, 0o702, 0o720] {
        let open = Scratch::new(mode);
        assert_eq!(
            refused(&open.0),
            DirectoryRefusal::WritableByOthers,
            "{mode:o}"
        );
    }
    // Owned by somebody else. Unprivileged: the root-owned `/`. As root:
    // a private directory handed to `nobody`.
    let euid = rustix::process::geteuid().as_raw();
    if euid == 0 {
        let foreign = Scratch::new(0o700);
        std::os::unix::fs::chown(&foreign.0, Some(65534), Some(65534)).unwrap();
        assert_eq!(refused(&foreign.0), DirectoryRefusal::NotOwned);
    } else {
        assert_eq!(refused(Path::new("/")), DirectoryRefusal::NotOwned);
    }
    // Limits must be positive and the session total at least one file.
    for bad in [limits(0, 10), limits(11, 10)] {
        assert_eq!(
            Directory::open(&good.0, bad).unwrap_err(),
            DirectoryRefusal::Limits
        );
    }
    assert_eq!(DirectoryRefusal::Symlink.code(), "files_directory_symlink");
    assert_eq!(
        DirectoryRefusal::WritableByOthers.code(),
        "files_directory_writable_by_others"
    );
}

#[test]
fn per_file_and_per_session_byte_limits_refuse_before_any_write() {
    let scratch = Scratch::new(0o700);
    let directory = Directory::open(&scratch.0, limits(4, 12)).unwrap();
    let input = controller();
    let mut session = receiver(&directory, &input);
    // Over the per-file limit: refused at the reservation, no staging file.
    assert_eq!(
        session.begin(offer(&session, 1, "big", b"12345"), || at(10)),
        Err(SessionError::Storage(fr_files::receive::Error::Quota))
    );
    assert!(scratch.entries().is_empty(), "{:?}", scratch.entries());
    // Within both limits: staged, verified, published.
    session
        .begin(offer(&session, 2, "first", b"abcd"), || at(20))
        .unwrap();
    session
        .write(session.binding(), 2, 0, b"abcd", || at(21))
        .unwrap();
    session.complete(session.binding(), 2, || at(22)).unwrap();
    assert_eq!(scratch.entries(), ["first"]);
    // 5 (refused, still counted) + 4 + 4 > 12: the cumulative declared bytes
    // refuse the next offer before a staging file exists.
    assert_eq!(
        session.begin(offer(&session, 3, "second", b"wxyz"), || at(30)),
        Err(SessionError::Quota)
    );
    assert_eq!(scratch.entries(), ["first"]);
    assert_eq!(fs::read(scratch.0.join("first")).unwrap(), b"abcd");
}

#[test]
fn no_byte_lands_without_the_controllers_live_lease() {
    let scratch = Scratch::new(0o700);
    let directory = Directory::open(&scratch.0, Limits::default()).unwrap();
    // Mid-transfer expiry: staged at t=0, the 3 s lease is never renewed.
    let input = controller();
    let mut session = receiver(&directory, &input);
    session
        .begin(offer(&session, 1, "report.bin", b"payload"), || at(100))
        .unwrap();
    session
        .write(session.binding(), 1, 0, b"pay", || at(200))
        .unwrap();
    assert_eq!(scratch.entries().len(), 1, "one private staging file");
    assert!(scratch.entries()[0].starts_with(".fr-part-"));
    let expired = at(3_500_000);
    assert!(input.monitor().deadline(expired).is_err(), "lease expired");
    assert!(matches!(
        session.write(session.binding(), 1, 3, b"load", || expired),
        Err(SessionError::Authority(_))
    ));
    // The disk worker's timer turn (it runs without traffic too) removes the
    // fenced transfer's private staging file; nothing was published.
    assert!(session.service(expired).is_err());
    assert!(scratch.entries().is_empty(), "{:?}", scratch.entries());
    assert!(matches!(
        session.complete(session.binding(), 1, || expired),
        Err(SessionError::Closed | SessionError::NoTransfer)
    ));
    assert_eq!(scratch.entries(), Vec::<String>::new());
    // A fresh receiver after expiry cannot even be constructed.
    let late = directory.configuration();
    assert!(
        HostReceiver::new(
            &input,
            late.directory,
            late.permission,
            late.policy,
            expired
        )
        .is_err()
    );
    assert_eq!(scratch.entries(), Vec::<String>::new());
}

#[test]
fn selection_refuses_links_directories_and_special_files_by_type() {
    let scratch = Scratch::new(0o700);
    let name = "résumé-データ λ.bin";
    let regular = scratch.0.join(name);
    fs::write(&regular, b"hello").unwrap();
    fs::create_dir(scratch.0.join("folder")).unwrap();
    symlink(&regular, scratch.0.join("alias.bin")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(scratch.0.join("socket")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        scratch.0.join("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    fs::write(scratch.0.join("a:b.txt"), b"x").unwrap();
    let other = Scratch::new(0o700);
    fs::write(other.0.join(name), b"same basename").unwrap();

    let selection = Selection::select(std::slice::from_ref(&regular)).unwrap();
    assert_eq!(selection.len(), 1);
    assert_eq!(selection.sizes().collect::<Vec<_>>(), [5]);
    // Refusal index is the position of the offending argument.
    for (path, reason) in [
        (scratch.0.join("folder"), SelectReason::Directory),
        (scratch.0.join("alias.bin"), SelectReason::Symlink),
        (scratch.0.join("socket"), SelectReason::SpecialFile),
        (scratch.0.join("fifo"), SelectReason::SpecialFile),
        (scratch.0.join("missing"), SelectReason::Missing),
        (scratch.0.join("a:b.txt"), SelectReason::NotPortable),
        (other.0.join(name), SelectReason::DuplicateName),
    ] {
        assert_eq!(
            Selection::select(&[regular.clone(), path]).unwrap_err(),
            SelectRefusal { index: 1, reason },
        );
    }
    let many = vec![regular.clone(); MAX_SENDS + 1];
    assert_eq!(
        Selection::select(&many).unwrap_err().reason,
        SelectReason::TooMany
    );
    assert_eq!(
        Selection::select(&[]).unwrap_err().reason,
        SelectReason::TooMany
    );
    // A fresh per-attempt request duplicates the same descriptors.
    let request = selection.request().unwrap();
    assert_eq!(request.selection.len(), 1);
    assert_eq!(request.lifetime, SEND_LIFETIME);
}

#[test]
fn names_and_contents_never_reach_debug_or_error_output() {
    let scratch = Scratch::new(0o700);
    let chosen_name = "zqx-name-λ.txt";
    let path = scratch.0.join(chosen_name);
    fs::write(&path, b"zqx-content-bytes").unwrap();
    let selection = Selection::select(std::slice::from_ref(&path)).unwrap();
    let request = selection.request().unwrap();
    let directory = Directory::open(&scratch.0, Limits::default()).unwrap();
    let control = SendControl::new();
    control.end(Absence::NotNegotiated);
    let refusal = Selection::select(&[path.clone(), path.clone()]).unwrap_err();
    let refused_dir = Directory::open(&path, Limits::default()).unwrap_err();
    let rendered = [
        format!("{selection:?}"),
        format!("{request:?}"),
        format!("{directory:?}"),
        format!("{control:?}"),
        format!("{refusal:?}"),
        format!("{refused_dir:?}"),
        refusal.reason.code().to_owned(),
        refused_dir.code().to_owned(),
    ];
    let scratch_path = scratch.0.to_string_lossy().into_owned();
    for text in &rendered {
        for marker in ["zqx", "λ", scratch_path.as_str()] {
            assert!(!text.contains(marker), "{marker} leaked in {text}");
        }
    }
    assert_eq!(refusal.reason, SelectReason::DuplicateName);
    assert_eq!(refused_dir, DirectoryRefusal::NotDirectory);
}
