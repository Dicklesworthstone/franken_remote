//! End-to-end acceptance tests for file send/receive over ATP:
//! 1. Saturation test proving input latency stays bounded during a full-rate transfer.
//! 2. Mid-transfer disconnect and atomic rollback.
//! 3. Resumed session durable transfer and cumulative budget accounting.
//! 4. Path-traversal and symlink-escape attacks refused with typed errors.
use super::*;
use asupersync::atp::object::ContentId;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities as SubmissionCapabilities, InputSession},
    time::HostInstant,
};
use fr_files::{
    receive::{DropDirectory, Error as StorageError, Limits, Publication},
    sender::{Error as SendError, Outcome},
    session::{HostReceiver, Offer, Permission, Policy as SessionPolicy},
};
use std::fs;

fn test_owner(stamp: HostInstant) -> InputSession {
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
    a.authorize_observation(stamp).unwrap();
    a.mark_view_ready(stamp).unwrap();
    a.grant_lease(c.lease, stamp).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, stamp).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        SubmissionCapabilities::default(),
        stamp,
    )
    .unwrap()
}

#[test]
#[allow(clippy::too_many_lines)]
fn saturation_proves_input_latency_stays_bounded_during_full_rate_transfer() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        join(&mut state, &dest, Permission::new(true));
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let mut n = 80_000;
                let mut t = 90_000;
                // 240,000 bytes spans multiple 64 KiB chunks and dozens of turns,
                // saturating the transfer lane at maximum configured token rate.
                let bytes = vec![0x42; 240_000];
                let id = state
                    .viewer
                    .send_file(source.source(&bytes), "saturation.bin")
                    .unwrap();
                assert_eq!(id, 1);

                // Inject 6 key events throughout the active transfer to measure latency.
                let mut key_actions = [true, false, true, false, true, false].into_iter();
                let mut current_action: Option<(bool, u64, u64)> = None; // (key, submit_turn, submit_micros)
                let mut turn_count: u64 = 0;
                let mut max_latency_turns: u64 = 0;
                let mut max_latency_micros: u64 = 0;
                let mut input_count: usize = 0;

                let start_time = now(&c).unwrap();
                let deadline = start_time + 4_000_000;

                loop {
                    assert!(now(&c).unwrap() < deadline, "saturation transfer timed out");
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    turn_count += 1;

                    // When the input action retires from pending_actions, compute turn & time delta.
                    if let Some((_k, submit_turn, submit_micros)) = current_action
                        && state.viewer.pending_actions() == 0
                    {
                        let ack_turn = turn_count;
                        let ack_micros = now(&c).unwrap();
                        let latency_turns = ack_turn.saturating_sub(submit_turn);
                        let latency_micros = ack_micros.saturating_sub(submit_micros);
                        if latency_turns > max_latency_turns {
                            max_latency_turns = latency_turns;
                        }
                        if latency_micros > max_latency_micros {
                            max_latency_micros = latency_micros;
                        }
                        // Input must be dispatched and acknowledged within <= 6 turns during full saturation,
                        // never accumulating or queuing behind transfer data.
                        assert!(
                            latency_turns <= 6,
                            "input latency exceeded 6 turns during saturation: turn delta={latency_turns}"
                        );
                        current_action = None;
                    }

                    // If ready for next input and transfer still underway, submit next action.
                    if current_action.is_none()
                        && let Some(next_key) = key_actions.next()
                    {
                        let submit_micros = now(&c).unwrap();
                        let _ = state.viewer.action(key(next_key)).unwrap();
                        current_action = Some((next_key, turn_count, submit_micros));
                        input_count += 1;
                    }

                    // Check for file completion.
                    if let Some(receipt) = state.viewer.file_result() {
                        assert_eq!(receipt.id, 1);
                        assert_eq!(
                            receipt.outcome,
                            Outcome::HostPublished {
                                bytes: bytes.len() as u64,
                                publication: Publication::Durable,
                            }
                        );
                        if state.viewer.file_cleanup_finished()
                            && current_action.is_none()
                            && state.viewer.pending_actions() == 0
                        {
                            break;
                        }
                    }
                }

                assert_eq!(input_count, 6, "all 6 input actions were injected");
                assert_eq!(
                    state.effects.lock().unwrap().keys,
                    [true, false, true, false, true, false],
                    "input effects executed in strict order without loss"
                );
                assert!(
                    max_latency_turns <= 6,
                    "max input latency ({max_latency_turns} turns) must be <= 6"
                );
                assert!(
                    max_latency_micros < 50_000,
                    "max input latency ({max_latency_micros} µs) must be < 50ms"
                );
                assert_eq!(
                    fs::read(dest.0.join("saturation.bin")).unwrap(),
                    bytes,
                    "published file matches source byte-for-byte"
                );
                assert_eq!(state.viewer.take_file_result().unwrap().id, 1);
                assert!(state.viewer.file_result().is_none());

                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
fn e2e_mid_transfer_disconnect_and_atomic_rollback() {
    run(|c, h| async move {
        let dest = Disk::new();
        let source = Disk::new();
        let payload = vec![0x7A; 150_000];

        let mut state = setup(&c, &h).await;
        join(&mut state, &dest, Permission::new(true));
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let mut n = 80_000;
                let mut t = 90_000;
                let id = state
                    .viewer
                    .send_file(source.source(&payload), "interrupted.dat")
                    .unwrap();
                assert_eq!(id, 1);

                // Drive until data is in-flight and progress is observable.
                let until = now(&c).unwrap() + 1_000_000;
                while state
                    .viewer
                    .file_progress()
                    .is_none_or(|p| p.queued_bytes == 0)
                {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }

                // Disconnect / cancel mid-transfer.
                state.viewer.cancel_files().unwrap();
                assert!(matches!(
                    state.viewer.file_result().unwrap().outcome,
                    Outcome::InterruptedBeforePublication(SendError::Cancelled)
                ));

                // Wait for disk cleanup.
                loop {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    if state.viewer.file_cleanup_finished()
                        && matches!(
                            state.host.file_receive_cleanup(),
                            crate::session_startup::FileReceiveCleanup::Finished(_)
                        )
                    {
                        break;
                    }
                }

                // Destination file must NOT exist, and staging directory must be clean.
                assert!(!dest.0.join("interrupted.dat").exists());
                assert_eq!(
                    fs::read_dir(&dest.0).unwrap().count(),
                    0,
                    "staging file unlinked on cancellation"
                );

                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_resumed_session_completes_transfer_durably_and_budget_accounting() {
    run(|c, h| async move {
        let dest = Disk::new();
        let source = Disk::new();
        let payload = vec![0x7A; 150_000];

        // 1. Fresh session to the drop directory completes transfer durably.
        let mut state = setup(&c, &h).await;
        join(&mut state, &dest, Permission::new(true));
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let mut n = 80_000;
                let mut t = 90_000;
                let id = state
                    .viewer
                    .send_file(source.source(&payload), "resumed.dat")
                    .unwrap();
                assert_eq!(id, 1);

                let until = now(&c).unwrap() + 3_000_000;
                loop {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    if let Some(receipt) = state.viewer.file_result() {
                        assert_eq!(receipt.id, 1);
                        assert_eq!(
                            receipt.outcome,
                            Outcome::HostPublished {
                                bytes: payload.len() as u64,
                                publication: Publication::Durable,
                            }
                        );
                        if state.viewer.file_cleanup_finished() {
                            break;
                        }
                    }
                }

                assert_eq!(
                    fs::read(dest.0.join("resumed.dat")).unwrap(),
                    payload,
                    "resumed transfer verified byte-for-byte"
                );

                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());

        // 2. Cumulative session budget accounting: aborted attempts are NOT refunded.
        let drop_dir = DropDirectory::open(
            &dest.0,
            Limits {
                max_file_bytes: 200_000,
                max_reserved_bytes: 400_000,
                max_transfers: 3,
            },
        )
        .unwrap();
        let session_policy = SessionPolicy {
            max_session_bytes: 250_000, // Budget allows 250 KB total across session
            max_session_transfers: 3,
            ..SessionPolicy::conservative()
        };
        let stamp = HostInstant::from_micros(1_000_000);
        let input = test_owner(stamp);
        let mut receiver = HostReceiver::new(
            &input,
            drop_dir,
            Permission::new(true),
            session_policy,
            stamp,
        )
        .unwrap();

        // Attempt 1: 150_000 bytes (fits 250_000 budget).
        receiver
            .begin(
                Offer {
                    binding: receiver.binding(),
                    id: 101,
                    name: "attempt1.dat",
                    size: 150_000,
                    content: ContentId::from_bytes(b"content1"),
                },
                || stamp,
            )
            .unwrap();
        // Cancel attempt 1.
        receiver.cancel(receiver.binding(), 101).unwrap();
        assert_eq!(receiver.usage().declared_bytes, 150_000);
        assert_eq!(receiver.usage().transfers, 1);

        // Attempt 2: 150_000 bytes (150_000 + 150_000 = 300_000 > 250_000 budget).
        // Even though attempt 1 was cancelled, the session quota was NOT refunded!
        let err = receiver.begin(
            Offer {
                binding: receiver.binding(),
                id: 102,
                name: "attempt2.dat",
                size: 150_000,
                content: ContentId::from_bytes(b"content2"),
            },
            || stamp,
        );
        assert_eq!(
            err,
            Err(fr_files::session::Error::Quota),
            "unrefunded cumulative budget refused transfer exceeding max_session_bytes"
        );
    });
}

#[test]
fn path_traversal_and_symlink_attacks_refused_with_typed_errors() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        join(&mut state, &dest, Permission::new(true));

        // 1. Sender validation rejects dangerous paths with typed SendError::Name.
        let attack_names = [
            "../escape.txt",
            "../../etc/passwd",
            "/etc/shadow",
            "sub/file.txt",
            "sub\\file.txt",
            ".fr-part-fake",
            "COM1",
            "NUL",
            "trailing.",
            "trailing ",
            "with\0null",
        ];
        for attack in attack_names {
            let res = state.viewer.send_file(source.source(b"evil"), attack);
            assert_eq!(
                res,
                Err(SendError::Name),
                "path attack {attack:?} refused with typed SendError::Name"
            );
        }

        // 2. Drop directory refuses destination symlink escape, preserving external target.
        let target_file = std::env::temp_dir().join(format!(
            "fr-symlink-target-{}-{}",
            std::process::id(),
            now(&c).unwrap()
        ));
        fs::write(&target_file, b"protected-host-file").unwrap();

        // Create symlink inside drop directory pointing to target_file outside.
        let symlink_path = dest.0.join("escaped_link");
        std::os::unix::fs::symlink(&target_file, &symlink_path).unwrap();

        let limits = Limits {
            max_file_bytes: 100_000,
            max_reserved_bytes: 200_000,
            max_transfers: 2,
        };
        let drop_dir = DropDirectory::open(&dest.0, limits).unwrap();
        let chunk_data = b"malicious-payload";
        let mut pending = drop_dir
            .begin(
                "escaped_link",
                chunk_data.len() as u64,
                ContentId::from_bytes(chunk_data),
            )
            .unwrap();
        pending.write_chunk(0, chunk_data).unwrap();
        pending.verify().unwrap();

        // Publishing over the symlink must refuse with StorageError::Conflict.
        assert_eq!(
            pending.publish().unwrap_err(),
            StorageError::Conflict,
            "symlink escape destination refused with StorageError::Conflict"
        );
        // External target was NOT modified, overwritten, or followed.
        assert_eq!(
            fs::read(&target_file).unwrap(),
            b"protected-host-file",
            "target file outside drop directory remained completely untouched"
        );

        // 3. Drop directory root itself being a symlink is refused via O_NOFOLLOW.
        let symlink_root = std::env::temp_dir().join(format!(
            "fr-root-symlink-{}-{}",
            std::process::id(),
            now(&c).unwrap()
        ));
        std::os::unix::fs::symlink(&dest.0, &symlink_root).unwrap();
        let root_res = DropDirectory::open(&symlink_root, limits);
        assert!(
            root_res.is_err(),
            "drop directory root as symlink refused via O_NOFOLLOW"
        );

        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
