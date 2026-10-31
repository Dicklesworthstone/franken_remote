//! The actual `HostLane` over authenticated UDP/TLS record owners. Cursor samples
//! and delayed scheduling are fixtures; no native cursor or WAN claim is made.
use super::*;
use crate::session_startup::test_network as support;
use asupersync::types::{Budget, CancelKind};
use fr_core::ids::*;
use fr_media::worker::cursor::{Observation, Snapshot};
use fr_transport::quic::{
    ALPN, Disposition, MAX_DELIVERY_ALLOWANCE_US, Messages, Policy, Priority,
};
use fr_wire::negotiation::ControlBinding;
use std::{future::Future, time::Duration};

struct Fixture {
    host: QuicRecords,
    viewer: QuicRecords,
    lanes: CursorLanes,
    cursor: HostCursor,
    lane: HostLane,
}
impl Fixture {
    async fn new(cx: &Cx) -> Self {
        let (viewer, host) = support::native_pair(cx, "localhost", ALPN).await;
        let mut host = host.unwrap();
        let shape = StreamRoute {
            stream: host.connection_mut().open_uni_stream(cx).unwrap(),
            binding: 8,
            messages: Messages::MediaConfiguration,
            priority: Priority::Critical,
            outbound: true,
            maximum: 4096,
        };
        let position = DatagramRoute {
            binding: 9,
            kind: 0x0034,
            outbound: true,
        };
        let policy = Policy::default();
        let host = QuicRecords::new(host, cx, &[shape], &[position], policy).unwrap();
        let viewer = QuicRecords::new(
            viewer.unwrap(),
            cx,
            &[StreamRoute {
                outbound: false,
                ..shape
            }],
            &[DatagramRoute {
                outbound: false,
                ..position
            }],
            policy,
        )
        .unwrap();
        let view = Binding {
            parent: ControlBinding {
                id: shape.binding,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            display: 4,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        };
        let mut fixture = Self {
            host,
            viewer,
            lanes: CursorLanes {
                shape,
                position,
                position_maximum: policy.datagram_record_bytes,
                limits: ProtocolLimits::ABSOLUTE,
                view,
            },
            cursor: HostCursor::new(),
            lane: HostLane::default(),
        };
        fixture.observe(1, 20);
        fixture
    }

    fn observe(&mut self, serial: u32, x: i32) -> u32 {
        let target = self
            .cursor
            .observe(&Observation::Inside(Snapshot {
                native_serial: serial,
                x,
                y: 30,
                width: 1,
                height: 1,
                hotspot_x: 0,
                hotspot_y: 0,
                rgba: &[1, 2, 3, 255],
            }))
            .unwrap()
            .unwrap();
        self.lane.lane.observe(Some(target));
        target.shape
    }

    fn service(
        &mut self,
        cx: &Cx,
        at: u64,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<CursorTurn, quic::Error> {
        self.lane.service(
            cx,
            &mut self.host,
            &self.lanes,
            &self.cursor,
            at,
            &mut authorize,
        )
    }

    async fn received(&mut self, cx: &Cx) -> (Vec<u32>, Vec<wire::CursorPosition>) {
        let mut shapes = Vec::new();
        let mut positions = Vec::new();
        let until = support::clock(cx) + 1_000_000;
        while shapes.is_empty() || positions.is_empty() {
            assert!(
                support::clock(cx) < until,
                "cursor did not reach the real peer"
            );
            let (host, viewer) = Box::pin(support::both(
                self.host.drive(cx, Duration::from_millis(1), || true),
                self.viewer.drive(cx, Duration::from_millis(1), || true),
            ))
            .await;
            host.unwrap();
            viewer.unwrap();
            self.viewer
                .receive(
                    cx,
                    || true,
                    |route, bytes| {
                        match route {
                            Route::Stream(route) => {
                                assert_eq!(route.binding, self.lanes.shape.binding);
                                shapes.push(
                                    wire::decode_shape_record(
                                        bytes,
                                        route.binding,
                                        route.maximum,
                                        &self.lanes.limits,
                                    )
                                    .unwrap()
                                    .shape_id,
                                );
                            }
                            Route::Datagram(route) => {
                                assert_eq!(route.binding, self.lanes.position.binding);
                                positions.push(
                                    wire::decode_position_record(
                                        bytes,
                                        route.binding,
                                        self.lanes.position_maximum,
                                    )
                                    .unwrap(),
                                );
                            }
                        }
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        (shapes, positions)
    }
}

fn run<F, Fut>(test: F)
where
    F: FnOnce(Cx, Fixture) -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let fixture = Box::pin(Fixture::new(&cx)).await;
        Box::pin(test(cx, fixture)).await;
    });
}

/// Simulate this owner being descheduled across its ORIGINAL send deadline.
/// The actual transport clock still decides expiry; no transport is mocked.
fn pass(cx: &Cx, until: u64) {
    let remaining = until.saturating_sub(support::clock(cx));
    std::thread::sleep(Duration::from_micros(remaining + 1_000));
    assert!(support::clock(cx) >= until);
}

#[test]
fn expired_unadmitted_shape_preserves_the_session_and_next_turn_uses_latest_state() {
    run(|cx, mut f| async move {
        let at = support::clock(&cx);
        let before = f.host.usage();
        let old = f.cursor.target().unwrap().shape;
        let mut checks = 0;
        let turn = f
            .service(&cx, at, || {
                checks += 1;
                if checks == 2 {
                    pass(&cx, at + SHAPE_SEND_US);
                }
                true
            })
            .unwrap();
        assert_eq!(
            checks, 2,
            "expiry at the final real transport admission check"
        );
        assert_eq!(turn.accepted, 0);
        assert!(turn.pending && !f.host.is_closed());
        assert_eq!(
            f.host.usage(),
            before,
            "no bytes acquired transport custody"
        );
        assert!(f.lane.lane.delivered().is_empty());
        assert_eq!(f.lane.lane.next(at), Next::Shape(old));

        let current = f.observe(2, 90);
        let turn = f.service(&cx, support::clock(&cx), || true).unwrap();
        assert_eq!(turn.accepted, 2);
        assert!(!turn.pending);
        assert!(!f.lane.lane.delivered().contains(old));
        assert!(f.lane.lane.delivered().contains(current));
        let (shapes, positions) = f.received(&cx).await;
        assert_eq!(shapes, [current]);
        assert_eq!(positions.len(), 1);
        assert_eq!(
            (positions[0].shape_id, positions[0].x, positions[0].sequence),
            (current, 90, 1)
        );
    });
}

#[test]
fn expired_unadmitted_position_keeps_its_sequence_consumed_and_coalesces_latest_target() {
    run(|cx, mut f| async move {
        let at = support::clock(&cx);
        let shape = f.cursor.target().unwrap().shape;
        let mut checks = 0;
        let turn = f
            .service(&cx, at, || {
                checks += 1;
                // The shape's two admission checks succeed. Only the following
                // position crosses its short deadline before transport custody.
                if checks == 4 {
                    pass(&cx, at + POSITION_SEND_US);
                }
                true
            })
            .unwrap();
        assert_eq!(checks, 4);
        assert_eq!(turn.accepted, 1);
        assert!(turn.pending && !f.host.is_closed());
        assert_eq!(
            f.host.usage().retained_send_records,
            1,
            "only the shape was admitted"
        );
        assert!(f.lane.lane.delivered().contains(shape));
        assert_eq!(
            f.lane.lane.next(at),
            Next::Position,
            "expired position was not marked sent"
        );

        assert_eq!(f.observe(1, 91), shape);
        let turn = f.service(&cx, support::clock(&cx), || true).unwrap();
        assert_eq!(turn.accepted, 1);
        assert!(!turn.pending);
        let (shapes, positions) = f.received(&cx).await;
        assert_eq!(shapes, [shape]);
        assert_eq!(positions.len(), 1);
        assert_eq!((positions[0].x, positions[0].sequence), (91, 2));
    });
}

#[test]
fn expired_retained_shape_remains_a_terminal_transport_failure() {
    run(|cx, mut f| async move {
        let turn = f.service(&cx, support::clock(&cx), || true).unwrap();
        assert_eq!(turn.accepted, 2);
        assert_eq!(f.host.usage().retained_send_records, 1);
        // Keep the original reliable bytes unflushed/unacknowledged across the
        // real delivery bound. A new optional update cannot forgive this debt.
        pass(
            &cx,
            support::clock(&cx) + SHAPE_SEND_US.max(MAX_DELIVERY_ALLOWANCE_US),
        );
        let current = f.observe(2, 92);
        let result = f.service(&cx, support::clock(&cx), || true);
        assert_eq!(result.unwrap_err(), quic::Error::Expired);
        assert!(f.host.is_closed());
        assert!(!f.lane.lane.delivered().contains(current));
    });
}

#[test]
fn closed_cancelled_and_unauthorized_transports_cannot_become_pending_cursor_work() {
    for expected in [
        quic::Error::Closed,
        quic::Error::Cancelled,
        quic::Error::Unauthorized,
    ] {
        run(move |cx, mut f| async move {
            match expected {
                quic::Error::Closed => f.host.close(),
                quic::Error::Cancelled => {
                    cx.cancel_fast(CancelKind::User);
                }
                quic::Error::Unauthorized => {}
                _ => unreachable!(),
            }
            let result = f.service(&cx, support::clock(&cx), || {
                expected != quic::Error::Unauthorized
            });
            assert_eq!(result.unwrap_err(), expected);
            assert!(f.host.is_closed());
            assert!(f.lane.lane.delivered().is_empty());
        });
    }
}
