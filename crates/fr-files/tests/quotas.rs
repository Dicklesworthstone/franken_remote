#![cfg(target_os = "linux")]
use asupersync::atp::object::ContentId;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession},
    time::HostInstant,
};
use fr_files::{
    receive::{DropDirectory, Limits},
    session::{Error, HostReceiver, Offer, Permission, Policy},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn owner() -> InputSession {
    let c = InputCredentials {
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
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.mark_view_ready(at(0)).unwrap();
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap()
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "fr-files-quota-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        Self(p)
    }
    fn root(&self) -> DropDirectory {
        DropDirectory::open(
            &self.0,
            Limits {
                max_file_bytes: 1024 * 1024,
                max_reserved_bytes: 1024 * 1024,
                max_transfers: 2,
            },
        )
        .unwrap()
    }
    fn empty(&self) {
        assert_eq!(fs::read_dir(&self.0).unwrap().count(), 0);
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn begin(session: &mut HostReceiver, id: u64, bytes: &[u8], now: u64) {
    session
        .begin(
            Offer {
                binding: session.binding(),
                id,
                name: "item",
                size: bytes.len() as u64,
                content: ContentId::from_bytes(bytes),
            },
            || at(now),
        )
        .unwrap();
}
#[test]
fn cumulative_declared_bytes_are_not_refunded_by_cancel_or_storage_refusal() {
    let scratch = Scratch::new();
    let input = owner();
    let policy = Policy {
        max_session_bytes: 6,
        max_session_transfers: 3,
        ..Policy::conservative()
    };
    let mut session =
        HostReceiver::new(&input, scratch.root(), Permission::new(true), policy, at(0)).unwrap();
    begin(&mut session, 1, b"abc", 0);
    session.cancel(session.binding(), 1).unwrap();
    begin(&mut session, 2, b"abc", 0);
    session.cancel(session.binding(), 2).unwrap();
    assert_eq!(session.usage().declared_bytes, 6);
    assert_eq!(session.usage().transfers, 2);
    assert_eq!(
        session.begin(
            Offer {
                binding: session.binding(),
                id: 3,
                name: "quota",
                size: 1,
                content: ContentId::from_bytes(b"x")
            },
            || at(0)
        ),
        Err(Error::Quota)
    );
    assert_eq!(session.usage().declared_bytes, 6);
    scratch.empty();
    assert!(input.monitor().deadline(at(0)).is_ok());
}

#[test]
fn cumulative_byte_overflow_refuses_without_wrapping_budget() {
    let scratch = Scratch::new();
    let input = owner();
    let policy = Policy {
        max_session_bytes: u64::MAX,
        ..Policy::conservative()
    };
    let mut session =
        HostReceiver::new(&input, scratch.root(), Permission::new(true), policy, at(0)).unwrap();
    // Even a declared object rejected by the narrower storage quota consumes
    // its admitted attempt/declaration. It cannot be churned indefinitely.
    assert_eq!(
        session.begin(
            Offer {
                binding: session.binding(),
                id: 1,
                name: "huge",
                size: u64::MAX,
                content: ContentId::from_bytes(b"")
            },
            || at(0)
        ),
        Err(Error::Storage(fr_files::receive::Error::Quota))
    );
    assert_eq!(session.usage().declared_bytes, u64::MAX);
    assert_eq!(session.usage().transfers, 1);
    assert_eq!(
        session.begin(
            Offer {
                binding: session.binding(),
                id: 2,
                name: "overflow",
                size: 1,
                content: ContentId::from_bytes(b"x")
            },
            || at(0)
        ),
        Err(Error::Quota)
    );
    assert_eq!(session.usage().declared_bytes, u64::MAX);
    scratch.empty();
}

#[test]
fn zero_byte_attempts_exhaust_count_but_invalid_names_do_not_consume_authority() {
    let scratch = Scratch::new();
    let input = owner();
    let policy = Policy {
        max_session_transfers: 1,
        ..Policy::conservative()
    };
    let mut session =
        HostReceiver::new(&input, scratch.root(), Permission::new(true), policy, at(0)).unwrap();
    assert_eq!(
        session.begin(
            Offer {
                binding: session.binding(),
                id: 1,
                name: "../bad",
                size: 0,
                content: ContentId::from_bytes(b"")
            },
            || at(0)
        ),
        Err(Error::Storage(fr_files::receive::Error::InvalidName))
    );
    assert_eq!(session.usage().transfers, 0);
    begin(&mut session, 1, b"", 0);
    session.cancel(session.binding(), 1).unwrap();
    assert_eq!(session.usage().transfers, 1);
    assert_eq!(session.usage().declared_bytes, 0);
    assert_eq!(
        session.begin(
            Offer {
                binding: session.binding(),
                id: 2,
                name: "next",
                size: 0,
                content: ContentId::from_bytes(b"")
            },
            || at(0)
        ),
        Err(Error::Quota)
    );
    scratch.empty();
}

#[test]
fn zero_session_budgets_are_rejected_before_any_file_exists() {
    let scratch = Scratch::new();
    let input = owner();
    for policy in [
        Policy {
            max_session_bytes: 0,
            ..Policy::conservative()
        },
        Policy {
            max_session_transfers: 0,
            ..Policy::conservative()
        },
    ] {
        assert!(matches!(
            HostReceiver::new(&input, scratch.root(), Permission::new(true), policy, at(0)),
            Err(Error::Policy)
        ));
    }
    scratch.empty();
}

#[test]
fn multigb_session_budget_and_token_bucket_scale_without_overflow() {
    let scratch = Scratch::new();
    let input = owner();
    let drop_limits = Limits {
        max_file_bytes: 10 * 1024 * 1024 * 1024, // 10 GB file limit
        max_reserved_bytes: 20 * 1024 * 1024 * 1024,
        max_transfers: 5,
    };
    let root = DropDirectory::open(&scratch.0, drop_limits).unwrap();

    let policy = Policy {
        max_session_bytes: 20 * 1024 * 1024 * 1024, // 20 GB session budget
        max_session_transfers: 5,
        bytes_per_second: 1_000_000_000, // 1 GB/s rate
        burst_bytes: 4 * (65_536 + 128),
        ..Policy::conservative()
    };
    let mut session =
        HostReceiver::new(&input, root, Permission::new(true), policy, at(0)).unwrap();

    // 1. Declare first multi-GB file: 2.5 GB (2,500,000,000 bytes)
    let size_1 = 2_500_000_000_u64;
    let offer_1 = Offer {
        binding: session.binding(),
        id: 1,
        name: "first_2_5gb.bin",
        size: size_1,
        content: ContentId::from_bytes(b"first"),
    };
    session.begin(offer_1, || at(100)).unwrap();
    assert_eq!(session.usage().transfers, 1);
    assert_eq!(session.usage().declared_bytes, size_1);

    // Cancel releases active reservation but retains session declared bytes charge
    session.cancel(session.binding(), 1).unwrap();
    assert_eq!(session.usage().transfers, 1);
    assert_eq!(session.usage().declared_bytes, size_1);

    // 2. Declare second multi-GB file: 3.5 GB (3,500,000,000 bytes)
    let size_2 = 3_500_000_000_u64;
    let offer_2 = Offer {
        binding: session.binding(),
        id: 2,
        name: "second_3_5gb.bin",
        size: size_2,
        content: ContentId::from_bytes(b"second"),
    };
    session.begin(offer_2, || at(200)).unwrap();
    assert_eq!(session.usage().transfers, 2);
    assert_eq!(session.usage().declared_bytes, size_1 + size_2); // 6,000,000,000 bytes

    session.cancel(session.binding(), 2).unwrap();

    // 3. Declare an over-budget file: 16 GB (16,000,000,000 bytes)
    // 6 GB + 16 GB = 22 GB > 20 GB (21,474,836,480 bytes) max_session_bytes
    // Fails with session Error::Quota before storage layer is touched
    let size_over = 16_000_000_000_u64;
    let offer_over = Offer {
        binding: session.binding(),
        id: 3,
        name: "over_budget.bin",
        size: size_over,
        content: ContentId::from_bytes(b"huge"),
    };
    assert_eq!(session.begin(offer_over, || at(300)), Err(Error::Quota));
    // Usage remains charged at 6 GB, not refunded or corrupted
    assert_eq!(session.usage().declared_bytes, size_1 + size_2);

    // 4. Verify storage layer quota refusal: file size fits remaining session budget
    // (6 GB + 11 GB = 17 GB <= 20 GiB), but exceeds drop_limits.max_file_bytes (10 GiB = 10,737,418,240 bytes):
    let file_over_storage = Offer {
        binding: session.binding(),
        id: 3,
        name: "over_storage.bin",
        size: 11_000_000_000, // 11 GB > 10 GiB storage limit
        content: ContentId::from_bytes(b"too_big"),
    };
    assert_eq!(
        session.begin(file_over_storage, || at(350)),
        Err(Error::Storage(fr_files::receive::Error::Quota))
    );

    // 4. Token bucket refills at 1 GB/s over simulated time without overflow
    // 1 second later (1_000_000 us):
    let chunk = vec![0x55u8; 65536];
    let offer_write = Offer {
        binding: session.binding(),
        id: 4,
        name: "write_test.bin",
        size: 65536,
        content: ContentId::from_bytes(b"write"),
    };
    session.begin(offer_write, || at(1_000_000)).unwrap();
    // Writing chunk succeeds because tokens refilled
    let progress = session
        .write(session.binding(), 4, 0, &chunk, || at(1_000_000))
        .unwrap();
    assert_eq!(progress.staged_bytes, 65536);
    session.cancel(session.binding(), 4).unwrap();

    scratch.empty();
}
