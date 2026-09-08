#![cfg(target_os = "linux")]
use asupersync::{cx::Cx, runtime::RuntimeBuilder, time::sleep, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{CodecConfigurationGeneration, RecoveryGeneration, RemoteSessionId},
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    delivery::{MediaBindings, MediaEpoch, SendPolicy},
};
use fr_wire::MediaLimits;
use frd::media::{ObservationControl, Subscription, host_now};
use std::time::Duration;
fn gate(cx: Cx, lifetime: u64) -> ObservationControl {
    let mut a = SessionAuthority::new(
        RemoteSessionId::from_raw(9),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(lifetime),
            ticket_lifetime: HostDuration::from_micros(lifetime / 2),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(host_now(&cx).unwrap()).unwrap();
    ObservationControl::new(cx, a).unwrap()
}
fn subscriber(c: ObservationControl) -> Subscription {
    Subscription::new(
        c,
        MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap(),
        MediaBindings::new(1, 2, 3, 4).unwrap(),
        MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        SendPolicy::default(),
    )
    .unwrap()
}
fn unit(cx: &Cx) -> EncodedAccessUnit {
    // Byte fixture for the real authority/packetization policies, not a codec.
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::FIRST,
        FrameKind::Idr {
            recovery: RecoveryGeneration::INITIAL,
        },
        CodecConfigurationGeneration::INITIAL,
        host_now(cx).unwrap().as_micros(),
        vec![1; 100],
    )
    .unwrap()
}
#[test]
fn expired_authority_blocks_already_prepared_packets_and_delayed_renewal() {
    let r = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    r.block_on(async {
        let c = gate(cx.clone(), 40_000);
        c.issue_challenge(17).unwrap();
        let mut s = subscriber(c.clone());
        s.enqueue(unit(&cx)).unwrap();
        let mut out = [0; 1150];
        let packet = s.next_packet(&mut out).unwrap().unwrap();
        let deadline = c.deadline(Duration::from_secs(2)).unwrap();
        assert!(
            deadline.time().as_nanos() <= cx.timer_driver().unwrap().now().as_nanos() + 40_000_000
        );
        sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(60)).await;
        assert!(s.authorize_write(&packet).is_err());
        assert!(s.next_packet(&mut out).is_err());
        assert!(c.renew(17).is_err());
    });
}
#[test]
fn suspend_fences_shared_authority_and_no_unapproved_session_can_construct_a_gate() {
    let r = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    r.block_on(async {
        let a = SessionAuthority::new(
            RemoteSessionId::from_raw(10),
            AuthorityPolicy::plan_defaults(),
        );
        assert!(ObservationControl::new(cx.clone(), a).is_err());
        let c = gate(cx, 3_000_000);
        let other = c.clone();
        c.suspend();
        assert!(other.check().is_err());
        assert!(other.issue_challenge(1).is_err());
    });
}
