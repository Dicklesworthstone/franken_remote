//! Completed-lane metadata is a fixture here, not an attachment/QUIC proof.
use super::*;
use fr_core::{
    ids::*,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::clipboard::{Context, Lane, session::ChannelSession};

#[test]
fn completed_lane_must_match_entire_parent_and_original_grant_scope() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let parent = grant().request.parent;
    let outgoing = context(Role::Controller);
    for i in 0..10 {
        let mut p = parent;
        let mut c = outgoing;
        match i {
            0 => p.id += 1,
            1 => p.host_boot = HostBootId::from_raw(99),
            2 => p.os_session = OsSessionId::from_raw(99),
            3 => p.remote_session = RemoteSessionId::from_raw(99),
            4 => c.scope.session = RemoteSessionId::from_raw(99),
            5 => c.scope.lease = InputLeaseId::from_raw(99),
            6 => c.sender = Role::Host,
            7 => c.sender = Role::Observer,
            8 => c.lane = Lane::Other,
            9 => c.channel = 0,
            _ => unreachable!(),
        }
        assert_eq!(
            input
                .attach_clipboard_lane(p, c, L, true, client(0))
                .unwrap_err(),
            Error::WrongChannel
        );
    }
    // Invalid metadata does not consume an attachment or change its scope.
    let lane = input
        .attach_clipboard_lane(parent, outgoing, L, true, client(0))
        .unwrap();
    drop(lane);
    assert_eq!(
        input
            .attach_clipboard_lane(parent, outgoing, L, true, client(0))
            .unwrap_err(),
        Error::AlreadyAttached
    );
}

#[test]
fn actual_lane_record_ceiling_chunks_both_directions_without_truncating_text() {
    for ceiling in [256, 2048] {
        for from_viewer in [true, false] {
            let limits = ProtocolLimits::with_overrides(LimitOverrides {
                max_control_message_bytes: Some(ceiling),
                ..LimitOverrides::default()
            })
            .unwrap();
            let mut input = accepted();
            ready(&mut input, 0, 0);
            // The completed auxiliary binding need not share the main channel ID.
            let outgoing = Context {
                channel: 88,
                ..context(Role::Controller)
            };
            let mut viewer = input
                .attach_clipboard_lane(grant().request.parent, outgoing, limits, true, client(0))
                .unwrap();
            viewer.local_switch().set_enabled(true);
            viewer.peer_switch().set_enabled(true);
            let owner = host_owner();
            let mut host = ChannelSession::new(
                &owner,
                Context {
                    sender: Role::Host,
                    ..outgoing
                },
                limits,
                true,
                host_at(0),
            )
            .unwrap();
            host.set_enabled(true, true);
            let text = "🦀 café\\n".repeat(2000);
            if from_viewer {
                viewer.offer(1, &text, None, client(0)).unwrap();
            } else {
                host.offer(1, &text, None, host_at(0)).unwrap();
            }
            let mut gate = Gate::default();
            let mut platform = Platform::default();
            let mut scratch = vec![0xa5; ceiling as usize];
            let mut done = false;
            for _ in 0..1030 {
                let pump = if from_viewer {
                    viewer.pump(&mut scratch, &mut gate, || client(0)).unwrap()
                } else {
                    host.pump(&mut scratch, &mut gate, || host_at(0)).unwrap()
                };
                assert!(gate.last.len() <= ceiling as usize);
                assert!(scratch.iter().all(|b| *b == 0));
                let receipt = if from_viewer {
                    host.receive(&gate.last, &mut platform, || host_at(0))
                        .unwrap()
                } else {
                    viewer
                        .receive(&gate.last, &mut platform, || client(0))
                        .unwrap()
                };
                if matches!(pump, Pump::ItemAccepted(_)) {
                    assert_eq!(receipt.unwrap().publication, Publication::SubmittedToOs);
                    done = true;
                    break;
                }
            }
            assert!(done);
            assert_eq!(platform.text, [text]);
            assert!(gate.calls > 3, "small allowance must produce real chunks");
        }
    }
}

#[test]
fn lane_limits_never_raise_session_limits_and_unusable_caps_do_not_attach() {
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(512),
        max_clipboard_item_bytes: Some(4096),
        ..LimitOverrides::default()
    })
    .unwrap();
    let mut input = accepted_with_limits(limits);
    ready(&mut input, 0, 0);
    let parent = grant().request.parent;
    let outgoing = context(Role::Controller);
    let tiny = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(16),
        ..LimitOverrides::default()
    })
    .unwrap();
    assert_eq!(
        input
            .attach_clipboard_lane(parent, outgoing, tiny, true, client(0))
            .unwrap_err(),
        Error::InvalidLimits
    );
    let mut viewer = input
        .attach_clipboard_lane(parent, outgoing, L, true, client(0))
        .unwrap();
    viewer.local_switch().set_enabled(true);
    viewer.peer_switch().set_enabled(true);
    assert!(viewer.offer(1, &"x".repeat(4097), None, client(0)).is_err());
    assert_eq!(viewer.retained_bytes(), 0);
    viewer.offer(2, &"y".repeat(4096), None, client(0)).unwrap();
    let mut gate = Gate::default();
    let mut scratch = [0; 512];
    let mut complete = false;
    for _ in 0..32 {
        let p = viewer.pump(&mut scratch, &mut gate, || client(0)).unwrap();
        assert!(gate.last.len() <= 512);
        if matches!(p, Pump::ItemAccepted(_)) {
            complete = true;
            break;
        }
    }
    assert!(complete);
}
