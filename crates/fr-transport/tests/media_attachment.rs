#![cfg(target_os = "linux")]
//! Actual UDP/TLS attachment; admission, selection and display identity are
//! explicitly local fixtures. No native codec or live tailnet is implied.
#[allow(dead_code)]
mod support;
use asupersync::{cx::Cx, net::quic_native::StreamId};
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_transport::quic::*;
use fr_wire::{
    attachment::{self, Message, Ticket},
    decoder::{self, Binding},
    input::{InputDelivery as T, InputDirection as D},
    negotiation::{Capability, ControlBinding, Offer, Role, Selection},
};
use std::{cell::Cell, time::Duration};
use support::{both, clock, runtime};
const WAIT: Duration = Duration::from_millis(2);
fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn binding(id: u32) -> Binding {
    Binding {
        parent: ControlBinding { id, ..parent() },
        display: 14,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn selection() -> Selection {
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![Capability {
            name: attachment::CAPABILITY.into(),
            version: 1,
            required: true,
        }],
    }
    .select()
    .unwrap()
}
struct Link {
    c: QuicRecords,
    h: QuicRecords,
    cr: ControlRoutes,
    hr: ControlRoutes,
    selection: Selection,
}
impl Link {
    async fn new(cx: &Cx) -> Self {
        let (c, h) = support::native_pair(cx, "localhost", ALPN).await;
        let policy = Policy {
            critical_send_records: 1,
            ..Policy::default()
        };
        let (mut c, cr) = QuicRecords::bootstrap(c.unwrap(), cx, policy).unwrap();
        let (mut h, hr) = QuicRecords::bootstrap(h.unwrap(), cx, policy).unwrap();
        let cr = c.bind_control(cx, cr, 7, 4096, || true).unwrap();
        let hr = h.bind_control(cx, hr, 7, 4096, || true).unwrap();
        Self {
            c,
            h,
            cr,
            hr,
            selection: selection(),
        }
    }
    async fn drive(&mut self, cx: &Cx) {
        let (a, b) = Box::pin(both(
            self.h.drive(cx, WAIT, || true),
            self.c.drive(cx, WAIT, || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    fn offer(&mut self, cx: &Cx, id: u32, ticket: u128) -> MediaChannel {
        self.h
            .offer_media_channel(
                cx,
                ChannelScope {
                    control: self.hr,
                    parent: parent(),
                    selection: &self.selection,
                },
                ChannelRequest {
                    binding: binding(id),
                    ticket: Ticket(ticket),
                    timeout: Duration::from_secs(2),
                },
                || true,
            )
            .unwrap()
    }
    async fn viewer(&mut self, cx: &Cx, h: &mut MediaChannel) -> MediaChannel {
        let until = clock(cx) + 1_000_000;
        let mut sent = false;
        loop {
            assert!(clock(cx) < until);
            if !sent {
                sent = h.transmit(&mut self.h, cx, || true).unwrap();
            }
            self.drive(cx).await;
            let mut bytes = None;
            let ready = Cell::new(true);
            self.c
                .receive_ready(
                    cx,
                    || true,
                    |r| ready.get() && r == Route::Stream(self.cr.inbound),
                    |_, b| {
                        ready.set(false);
                        bytes = Some(b.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(b) = bytes {
                return self
                    .c
                    .accept_media_channel(
                        cx,
                        ChannelScope {
                            control: self.cr,
                            parent: parent(),
                            selection: &self.selection,
                        },
                        &b,
                        Duration::from_secs(2),
                        || true,
                    )
                    .unwrap();
            }
        }
    }
    async fn attached(
        &mut self,
        cx: &Cx,
        h: &mut MediaChannel,
        c: &mut MediaChannel,
    ) -> (AttachedChannel, AttachedChannel) {
        let until = clock(cx) + 1_500_000;
        let (mut ha, mut ca) = (None, None);
        while ha.is_none() || ca.is_none() {
            assert!(
                clock(cx) < until,
                "attachment failed to make progress: {h:?} {c:?}"
            );
            h.transmit(&mut self.h, cx, || true).unwrap();
            c.transmit(&mut self.c, cx, || true).unwrap();
            self.drive(cx).await;
            h.dispatch(&mut self.h, cx, || true).unwrap();
            c.dispatch(&mut self.c, cx, || true).unwrap();
            ha = h.finish(&mut self.h, cx, || true).unwrap();
            ca = c.finish(&mut self.c, cx, || true).unwrap();
        }
        (ha.unwrap(), ca.unwrap())
    }
}
fn encoded(m: Message, dir: D) -> Vec<u8> {
    let mut b = vec![0; attachment::GRANT_RECORD_BYTES];
    let n = attachment::encode(
        m,
        parent(),
        &ProtocolLimits::ABSOLUTE,
        &mut b,
        dir,
        T::Reliable,
    )
    .unwrap();
    b.truncate(n);
    b
}
#[test]
fn ticketed_pair_exchanges_real_records_and_promotes_without_replacing_connection() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let conn = l.h.binding();
        let mut h = l.offer(&cx, 8, 90);
        let mut c = l.viewer(&cx, &mut h).await;
        assert!(!l.h.has_route(Route::Stream(StreamRoute {
            stream: StreamId(7),
            binding: 8,
            messages: Messages::Exact(0x30),
            priority: Priority::Critical,
            outbound: true,
            maximum: 8192
        })));
        let (ha, ca) = l.attached(&cx, &mut h, &mut c).await;
        assert_eq!(ha.descriptor, ca.descriptor);
        assert_eq!(ha.outbound.stream, ca.inbound.stream);
        assert!(l.h.is_bound_to(&conn));
        let mut bytes = vec![0; 256];
        let n = decoder::encode(
            decoder::Message::Configured,
            binding(8),
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            D::ViewerToHost,
            T::Reliable,
        )
        .unwrap();
        bytes.truncate(n);
        l.c.send(
            &cx,
            Route::Stream(ca.outbound),
            &bytes,
            clock(&cx) + 1_000_000,
            || true,
        )
        .unwrap();
        let until = clock(&cx) + 1_000_000;
        let mut got = false;
        while !got {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            l.h.receive_ready(
                &cx,
                || true,
                |r| r == Route::Stream(ha.inbound),
                |_, b| {
                    assert_eq!(b, bytes);
                    got = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        drop(h);
        drop(c);
        assert!(l.h.tick(&cx, || true).is_ok());
        assert!(l.c.tick(&cx, || true).is_ok());
    });
}
#[test]
fn fixed_pending_budget_expiry_and_owner_drop_fence_the_connection() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let h = l.offer(&cx, 8, 90);
        assert!(matches!(
            l.h.offer_media_channel(
                &cx,
                ChannelScope {
                    control: l.hr,
                    parent: parent(),
                    selection: &l.selection
                },
                ChannelRequest {
                    binding: binding(9),
                    ticket: Ticket(91),
                    timeout: Duration::from_secs(1)
                },
                || true
            ),
            Err(Error::Backpressure)
        ));
        drop(h);
        assert!(l.h.tick(&cx, || true).is_err());
        assert!(l.h.is_closed());
        let mut l = Link::new(&cx).await;
        let mut h =
            l.h.offer_media_channel(
                &cx,
                ChannelScope {
                    control: l.hr,
                    parent: parent(),
                    selection: &l.selection,
                },
                ChannelRequest {
                    binding: binding(8),
                    ticket: Ticket(90),
                    timeout: Duration::from_millis(5),
                },
                || true,
            )
            .unwrap();
        let deadline = h.deadline_us();
        asupersync::time::sleep(cx.now(), Duration::from_millis(8)).await;
        assert_eq!(h.transmit(&mut l.h, &cx, || true), Err(Error::Expired));
        assert_eq!(h.deadline_us(), deadline);
        assert!(l.h.is_closed());
    });
}
#[test]
fn malformed_parent_or_unnegotiated_extension_never_allocates_a_route() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let before = l.h.usage();
        let mut absent = l.selection.clone();
        absent.capabilities.clear();
        assert!(
            l.h.offer_media_channel(
                &cx,
                ChannelScope {
                    control: l.hr,
                    parent: parent(),
                    selection: &absent
                },
                ChannelRequest {
                    binding: binding(8),
                    ticket: Ticket(90),
                    timeout: Duration::from_secs(1)
                },
                || true
            )
            .is_err()
        );
        assert_eq!(l.h.usage(), before);
        let mut wrong = binding(8);
        wrong.parent.remote_session = RemoteSessionId::from_raw(999);
        assert!(
            l.h.offer_media_channel(
                &cx,
                ChannelScope {
                    control: l.hr,
                    parent: parent(),
                    selection: &l.selection
                },
                ChannelRequest {
                    binding: wrong,
                    ticket: Ticket(90),
                    timeout: Duration::from_secs(1)
                },
                || true
            )
            .is_err()
        );
        let _h = l.offer(&cx, 8, 90);
        assert!(!l.h.is_closed());
    });
}
#[test]
fn final_authority_and_foreign_connection_refuse_before_packet_copy() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let mut h = l.offer(&cx, 8, 90);
        let mut other = Link::new(&cx).await;
        assert!(h.transmit(&mut other.h, &cx, || true).is_err());
        assert_eq!(other.h.usage().critical_send_records, 0);
        assert!(l.h.tick(&cx, || true).is_err());
        let mut l = Link::new(&cx).await;
        let mut h = l.offer(&cx, 8, 90);
        let mut checks = 0;
        assert!(
            h.transmit(&mut l.h, &cx, || {
                checks += 1;
                checks < 3
            })
            .is_err()
        );
        assert!(l.h.is_closed());
    });
}
#[test]
fn reused_ticket_and_binding_ids_cannot_allocate_a_second_channel() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let mut h = l.offer(&cx, 8, 90);
        let mut c = l.viewer(&cx, &mut h).await;
        l.attached(&cx, &mut h, &mut c).await;
        for (id, ticket) in [(9, 90), (8, 91)] {
            assert!(
                l.h.offer_media_channel(
                    &cx,
                    ChannelScope {
                        control: l.hr,
                        parent: parent(),
                        selection: &l.selection
                    },
                    ChannelRequest {
                        binding: binding(id),
                        ticket: Ticket(ticket),
                        timeout: Duration::from_secs(1)
                    },
                    || true
                )
                .is_err()
            );
        }
        let mut h = l.offer(&cx, 9, 91);
        let mut c = l.viewer(&cx, &mut h).await;
        let (ha, ca) = l.attached(&cx, &mut h, &mut c).await;
        assert_eq!(ha.outbound.stream, StreamId(11));
        assert_eq!(ca.outbound.stream, StreamId(10));
    });
}
#[test]
fn unexpected_ticket_nonce_on_actual_auxiliary_stream_is_terminal() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let mut h = l.offer(&cx, 8, 90);
        let mut c = l.viewer(&cx, &mut h).await;
        c.transmit(&mut l.c, &cx, || true).unwrap();
        let until = clock(&cx) + 1_000_000;
        let mut grant = None;
        while grant.is_none() {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            h.dispatch(&mut l.h, &cx, || true).unwrap();
            h.transmit(&mut l.h, &cx, || true).unwrap();
            l.c.receive_ready(
                &cx,
                || true,
                |r| r == Route::Stream(l.cr.inbound),
                |_, b| {
                    if let Message::Ticket(g) = attachment::decode(
                        b,
                        parent(),
                        7,
                        &ProtocolLimits::ABSOLUTE,
                        D::HostToViewer,
                        T::Reliable,
                    )
                    .unwrap()
                    {
                        grant = Some(g);
                    } else {
                        panic!("not a ticket");
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        let mut grant = grant.unwrap();
        grant.ticket = Ticket(91);
        let bytes = encoded(Message::Attach(grant), D::ViewerToHost);
        let r = StreamRoute {
            stream: StreamId(c.descriptor().viewer_stream),
            binding: 8,
            messages: Messages::Exact(0x19),
            priority: Priority::Critical,
            outbound: true,
            maximum: attachment::GRANT_RECORD_BYTES,
        };
        while let Err(Error::Backpressure) = l.c.send(&cx, Route::Stream(r), &bytes, until, || true)
        {
            l.drive(&cx).await;
        }
        loop {
            l.drive(&cx).await;
            if let Err(e) = h.dispatch(&mut l.h, &cx, || true) {
                assert_eq!(e, Error::WrongRoute);
                break;
            }
            assert!(clock(&cx) < until);
        }
        assert!(l.h.is_closed());
        assert!(!h.is_complete());
    });
}
#[test]
fn unrelated_renewal_and_actual_queue_pressure_do_not_consume_attachment_state() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let mut h = l.offer(&cx, 8, 90);
        let deadline = h.deadline_us();
        let mut challenge = [0; fr_wire::authority::OBSERVATION_CHALLENGE_BYTES];
        fr_wire::authority::encode(
            fr_wire::authority::Message::Challenge {
                scope: fr_wire::authority::Scope::Observation,
                nonce: 50,
                deadline_micros: clock(&cx) + 2_000_000,
            },
            fr_wire::authority::Binding {
                channel: 7,
                session: parent().remote_session,
            },
            &ProtocolLimits::ABSOLUTE,
            &mut challenge,
            D::HostToViewer,
            T::Reliable,
        )
        .unwrap();
        l.h.send(
            &cx,
            Route::Stream(l.hr.outbound),
            &challenge,
            deadline,
            || true,
        )
        .unwrap();
        assert!(!h.transmit(&mut l.h, &cx, || true).unwrap());
        assert!(!h.transmit(&mut l.h, &cx, || true).unwrap());
        assert_eq!(h.deadline_us(), deadline);
        let mut got = false;
        while !got {
            l.drive(&cx).await;
            l.c.receive_ready(
                &cx,
                || true,
                |r| r == Route::Stream(l.cr.inbound),
                |_, b| {
                    assert_eq!(b, challenge);
                    got = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        let mut c = l.viewer(&cx, &mut h).await;
        // An unrelated response arriving ahead of BindingAccepted is left intact for
        // the existing renewal dispatcher, even while native attachment is unarmed.
        let response = fr_wire::authority::Message::Response {
            scope: fr_wire::authority::Scope::Observation,
            nonce: 50,
        };
        let mut b = [0; fr_wire::authority::OBSERVATION_RESPONSE_BYTES];
        fr_wire::authority::encode(
            response,
            fr_wire::authority::Binding {
                channel: 7,
                session: parent().remote_session,
            },
            &ProtocolLimits::ABSOLUTE,
            &mut b,
            D::ViewerToHost,
            T::Reliable,
        )
        .unwrap();
        l.c.send(&cx, Route::Stream(l.cr.outbound), &b, deadline, || true)
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        let mut got = false;
        while !got {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            h.dispatch(&mut l.h, &cx, || true).unwrap();
            l.h.receive_ready(
                &cx,
                || true,
                |r| r == Route::Stream(l.hr.inbound),
                |_, bytes| {
                    assert_eq!(bytes, b);
                    got = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        l.attached(&cx, &mut h, &mut c).await;
    });
}
#[test]
fn seven_retired_pairs_are_bounded_and_an_eighth_refuses_without_recycling() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx).await;
        for i in 0..7 {
            let mut h = link.offer(&cx, 8 + i, 90 + u128::from(i));
            let mut c = link.viewer(&cx, &mut h).await;
            let (a, b) = link.attached(&cx, &mut h, &mut c).await;
            assert_eq!(a.descriptor.binding.parent.id, 8 + i);
            assert_eq!(a.descriptor, b.descriptor);
        }
        let before = link.h.usage();
        assert!(matches!(
            link.h.offer_media_channel(
                &cx,
                ChannelScope {
                    control: link.hr,
                    parent: parent(),
                    selection: &link.selection
                },
                ChannelRequest {
                    binding: binding(15),
                    ticket: Ticket(999),
                    timeout: Duration::from_secs(1)
                },
                || true
            ),
            Err(Error::Backpressure)
        ));
        assert_eq!(link.h.usage(), before);
        assert!(!link.h.is_closed());
    });
}
#[test]
fn decoder_reply_before_attachment_and_attachment_replay_after_promotion_refuse() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx).await;
        let mut h = link.offer(&cx, 8, 90);
        let mut c = link.viewer(&cx, &mut h).await;
        let mut b = [0; 256];
        let n = decoder::encode(
            decoder::Message::Configured,
            binding(8),
            &ProtocolLimits::ABSOLUTE,
            &mut b,
            D::ViewerToHost,
            T::Reliable,
        )
        .unwrap();
        let early = StreamRoute {
            stream: StreamId(c.descriptor().viewer_stream),
            binding: 8,
            messages: Messages::DecoderReplies,
            priority: Priority::Critical,
            outbound: true,
            maximum: 8192,
        };
        let before = link.c.usage();
        assert_eq!(
            link.c.send(
                &cx,
                Route::Stream(early),
                &b[..n],
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::WrongRoute)
        );
        assert_eq!(link.c.usage(), before);
        link.attached(&cx, &mut h, &mut c).await;
        let old = StreamRoute {
            messages: Messages::Exact(0x19),
            maximum: attachment::GRANT_RECORD_BYTES,
            ..early
        };
        assert!(!link.c.has_route(Route::Stream(old)));
        assert_eq!(
            link.c.send(
                &cx,
                Route::Stream(old),
                &b[..n],
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::WrongRoute)
        );
    });
}

#[path = "media_attachment/delivery.rs"]
mod delivery;
