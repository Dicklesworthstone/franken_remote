#![cfg(target_os = "linux")]
//! Actual UDP/TLS media retirement and fresh attachment. Admission is a local
//! fixture; these tests do not qualify HEVC or live tailnet interoperability.
#[allow(dead_code)]
mod support;
use asupersync::cx::Cx;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_transport::quic::*;
use fr_wire::{
    attachment::{self, Ticket},
    decoder::Binding,
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
macro_rules! run_test {
    ($cx:ident, $body:expr) => {{
        runtime().block_on(async {
            let $cx = Cx::current().unwrap();
            $body
        });
    }};
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
async fn transfer(l: &mut Link, cx: &Cx, host: bool, send: Route, recv: Route, bytes: &[u8]) {
    let until = clock(cx) + 1_000_000;
    loop {
        let q = if host { &mut l.h } else { &mut l.c };
        match q.send(cx, send, bytes, until, || true) {
            Ok(()) => break,
            Err(Error::Backpressure) => l.drive(cx).await,
            e => panic!("unexpected send: {e:?}"),
        }
        assert!(clock(cx) < until);
    }
    let mut got = false;
    while !got {
        assert!(clock(cx) < until);
        l.drive(cx).await;
        let q = if host { &mut l.c } else { &mut l.h };
        q.receive_ready(
            cx,
            || true,
            |r| r == recv,
            |_, b| {
                assert_eq!(b, bytes);
                got = true;
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
    }
}

use fr_wire::attachment::MediaRole;

fn enable(l: &mut Link) {
    l.selection.capabilities.push(Capability {
        name: attachment::DELIVERY_CAPABILITY.into(),
        version: attachment::DELIVERY_VERSION,
        required: true,
    });
}
async fn attach(
    l: &mut Link,
    cx: &Cx,
    role: MediaRole,
    id: u32,
    generation: RecoveryGeneration,
) -> (MediaChannel, MediaChannel, AttachedChannel, AttachedChannel) {
    let mut b = binding(id);
    b.recovery = generation;
    let mut h =
        l.h.offer_media_role(
            cx,
            ChannelScope {
                control: l.hr,
                parent: parent(),
                selection: &l.selection,
            },
            ChannelRequest {
                binding: b,
                ticket: Ticket(1000 + u128::from(id)),
                timeout: Duration::from_secs(2),
            },
            role,
            || true,
        )
        .unwrap();
    let mut c = l.viewer(cx, &mut h).await;
    let (ha, ca) = l.attached(cx, &mut h, &mut c).await;
    (h, c, ha, ca)
}
fn record(kind: u16, binding: u32, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    bytes[..4].copy_from_slice(b"FRD0");
    bytes[6..8].copy_from_slice(&kind.to_be_bytes());
    bytes[12..16].copy_from_slice(&u32::try_from(len - 24).unwrap().to_be_bytes());
    bytes[16..20].copy_from_slice(&binding.to_be_bytes());
    bytes
}
async fn quiet(l: &mut Link, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    while l.h.usage().retained_send_records != 0 || l.c.usage().retained_send_records != 0 {
        assert!(clock(cx) < until, "original control work did not drain");
        l.drive(cx).await;
    }
}
async fn retired(l: &mut Link, cx: &Cx, peer: AttachedChannel, host_initiated: bool) {
    let until = clock(cx) + 1_000_000;
    loop {
        l.drive(cx).await;
        let q = if host_initiated { &l.c } else { &l.h };
        if !q.has_route(Route::Stream(peer.inbound)) {
            assert!(!q.has_route(Route::Stream(peer.outbound)));
            if let Some(route) = peer.datagram {
                assert!(!q.has_route(Route::Datagram(route)));
            }
            break;
        }
        assert!(clock(cx) < until, "peer media pair was not retired");
    }
    assert!(!l.h.is_closed());
    assert!(!l.c.is_closed());
}
async fn control_works(l: &mut Link, cx: &Cx) {
    for host in [true, false] {
        let (send, recv) = if host {
            (l.hr.outbound, l.cr.inbound)
        } else {
            (l.cr.outbound, l.hr.inbound)
        };
        transfer(
            l,
            cx,
            host,
            Route::Stream(send),
            Route::Stream(recv),
            &record(0x12, 7, 96),
        )
        .await;
    }
}

async fn queue_stale_work(
    l: &mut Link,
    cx: &Cx,
    recovery: AttachedChannel,
    video: AttachedChannel,
    viewer: AttachedChannel,
) {
    // Retain a reliable recovery record AND a blocked application datagram.
    let hr = recovery;
    l.h.send(
        cx,
        Route::Stream(hr.outbound),
        &record(0x32, 9, 6000),
        clock(cx) + 1_000_000,
        || true,
    )
    .unwrap();
    let hv = video.datagram.unwrap();
    l.h.send(
        cx,
        Route::Datagram(hv),
        &record(0x34, 10, 100),
        clock(cx) + 1_000_000,
        || true,
    )
    .unwrap();
    let mut blocked = false;
    let until = clock(cx) + 1_000_000;
    while !blocked {
        assert!(clock(cx) < until);
        l.drive(cx).await;
        l.c.receive_ready(
            cx,
            || true,
            |r| r == Route::Datagram(viewer.datagram.unwrap()),
            |_, _| {
                blocked = true;
                Ok(Disposition::Blocked)
            },
        )
        .unwrap();
    }
    assert!(l.c.usage().remainder_bytes >= 100);
}

#[test]
fn media_reset_and_fresh_generation_keep_the_original_control_connection() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let identity = l.h.binding();
        let mut old = Vec::new();
        for (i, role) in [
            MediaRole::Configuration,
            MediaRole::Recovery,
            MediaRole::Video,
        ]
        .into_iter()
        .enumerate()
        {
            old.push(
                attach(
                    &mut l,
                    &cx,
                    role,
                    8 + u32::try_from(i).unwrap(),
                    RecoveryGeneration::INITIAL,
                )
                .await,
            );
        }
        quiet(&mut l, &cx).await;
        queue_stale_work(&mut l, &cx, old[1].2, old[2].2, old[2].3).await;
        let hv = old[2].2.datagram.unwrap();
        for (h, _, _, ca) in &mut old {
            h.retire_media(&mut l.h, &cx).unwrap();
            retired(&mut l, &cx, *ca, true).await;
        }
        assert_eq!(l.c.usage().remainder_bytes, 0);
        assert!(!l.h.has_route(Route::Datagram(hv)));
        control_works(&mut l, &cx).await;
        let mut fresh = Vec::new();
        for (i, role) in [
            MediaRole::Configuration,
            MediaRole::Recovery,
            MediaRole::Video,
        ]
        .into_iter()
        .enumerate()
        {
            fresh.push(
                attach(
                    &mut l,
                    &cx,
                    role,
                    11 + u32::try_from(i).unwrap(),
                    RecoveryGeneration::INITIAL.next().unwrap(),
                )
                .await,
            );
        }
        assert!(l.h.is_bound_to(&identity));
        for (index, kind, size) in [(0, 0x30, 300), (1, 0x32, 6000), (2, 0x37, 120)] {
            let (ha, ca) = (fresh[index].2, fresh[index].3);
            transfer(
                &mut l,
                &cx,
                true,
                Route::Stream(ha.outbound),
                Route::Stream(ca.inbound),
                &record(kind, ha.outbound.binding, size),
            )
            .await;
        }
        let (ha, ca) = (fresh[2].2, fresh[2].3);
        transfer(
            &mut l,
            &cx,
            true,
            Route::Datagram(ha.datagram.unwrap()),
            Route::Datagram(ca.datagram.unwrap()),
            &record(0x34, 13, 1150),
        )
        .await;
        // Old owners cannot be revived by fresh attachments with equal payloads.
        for (mut h, mut c, ha, ca) in old {
            assert!(h.completed_on(&l.h).is_err());
            assert!(c.completed_on(&l.c).is_err());
            assert!(!l.h.has_route(Route::Stream(ha.outbound)));
            assert!(!l.c.has_route(Route::Stream(ca.inbound)));
            h.retire_media(&mut l.h, &cx).unwrap();
            c.retire_media(&mut l.c, &cx).unwrap();
        }
        control_works(&mut l, &cx).await;
    });
}

#[test]
fn viewer_initiated_video_retirement_preserves_input_ordering_and_control() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        l.selection.role = Role::RequestControl;
        l.selection.capabilities.push(Capability {
            name: attachment::INPUT_CAPABILITY.into(),
            version: attachment::INPUT_VERSION,
            required: true,
        });
        l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        let (_hi, _ci, hi, ci) = attach(
            &mut l,
            &cx,
            MediaRole::Input,
            8,
            RecoveryGeneration::INITIAL,
        )
        .await;
        let (_h, mut c, ha, _) = attach(
            &mut l,
            &cx,
            MediaRole::Video,
            9,
            RecoveryGeneration::INITIAL,
        )
        .await;
        quiet(&mut l, &cx).await;
        c.retire_media(&mut l.c, &cx).unwrap();
        retired(&mut l, &cx, ha, false).await;
        control_works(&mut l, &cx).await;
        for (host, send, recv, kind) in [
            (true, hi.outbound, ci.inbound, 0x17),
            (false, ci.outbound, hi.inbound, 0x40),
        ] {
            transfer(
                &mut l,
                &cx,
                host,
                Route::Stream(send),
                Route::Stream(recv),
                &record(kind, 8, 96),
            )
            .await;
        }
        assert!(l.h.has_route(Route::Datagram(hi.datagram.unwrap())));
        assert!(l.c.has_route(Route::Datagram(ci.datagram.unwrap())));
    });
}

#[test]
fn retirement_refuses_foreign_connections_input_roles_and_partial_attachments() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        let mut foreign = Link::new(&cx).await;
        enable(&mut l);
        let (mut h, _c, ha, _) = attach(
            &mut l,
            &cx,
            MediaRole::Recovery,
            8,
            RecoveryGeneration::INITIAL,
        )
        .await;
        let before = foreign.h.usage();
        assert_eq!(h.retire_media(&mut foreign.h, &cx), Err(Error::WrongRoute));
        assert_eq!(foreign.h.usage(), before);
        assert!(l.h.has_route(Route::Stream(ha.outbound)));
        l.selection.role = Role::RequestControl;
        l.selection.capabilities.push(Capability {
            name: attachment::INPUT_CAPABILITY.into(),
            version: attachment::INPUT_VERSION,
            required: true,
        });
        l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        let (mut hi, _, ia, _) = attach(
            &mut l,
            &cx,
            MediaRole::Input,
            9,
            RecoveryGeneration::INITIAL,
        )
        .await;
        assert_eq!(hi.retire_media(&mut l.h, &cx), Err(Error::WrongRoute));
        assert!(l.h.has_route(Route::Stream(ia.outbound)));
        let mut pending = l.offer(&cx, 10, 5000);
        assert_eq!(pending.retire_media(&mut l.h, &cx), Err(Error::WrongRoute));
        assert!(!l.h.is_closed());
        drop(pending);
        assert_eq!(l.h.tick(&cx, || true), Err(Error::Expired));
        assert!(!foreign.h.is_closed());
    });
}

#[test]
fn retiring_media_does_not_recycle_consumed_streams_tickets_or_native_capacity() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        for id in 8..15 {
            let (mut h, _c, _, ca) = attach(
                &mut l,
                &cx,
                MediaRole::Recovery,
                id,
                RecoveryGeneration::INITIAL,
            )
            .await;
            h.retire_media(&mut l.h, &cx).unwrap();
            retired(&mut l, &cx, ca, true).await;
        }
        let before = l.h.usage();
        assert_eq!(l.h.next_channel_binding().unwrap(), 15);
        let result = l.h.offer_media_role(
            &cx,
            ChannelScope {
                control: l.hr,
                parent: parent(),
                selection: &l.selection,
            },
            ChannelRequest {
                binding: binding(15),
                ticket: Ticket(9000),
                timeout: Duration::from_secs(2),
            },
            MediaRole::Recovery,
            || true,
        );
        assert!(matches!(result, Err(Error::Backpressure)));
        assert_eq!(l.h.usage(), before);
        control_works(&mut l, &cx).await;
    });
}

#[test]
fn late_old_datagrams_are_discarded_without_disrupting_another_video_binding() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (_h, mut c, old, _) = attach(
            &mut l,
            &cx,
            MediaRole::Video,
            8,
            RecoveryGeneration::INITIAL,
        )
        .await;
        let (_other_h, _other_c, healthy, healthy_peer) = attach(
            &mut l,
            &cx,
            MediaRole::Video,
            9,
            RecoveryGeneration::INITIAL,
        )
        .await;
        quiet(&mut l, &cx).await;
        c.retire_media(&mut l.c, &cx).unwrap();
        // Host has not driven/received the reset: old datagrams can still be
        // admitted and enter the wire. They must not reach the viewer callback.
        let until = clock(&cx) + 1_000_000;
        l.h.send(
            &cx,
            Route::Datagram(old.datagram.unwrap()),
            &record(0x34, 8, 100),
            until,
            || true,
        )
        .unwrap();
        let expected = record(0x34, 9, 120);
        l.h.send(
            &cx,
            Route::Datagram(healthy.datagram.unwrap()),
            &expected,
            until,
            || true,
        )
        .unwrap();
        let mut got = false;
        while !got {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            l.c.receive_ready(
                &cx,
                || true,
                |_| true,
                |route, bytes| {
                    assert_eq!(route, Route::Datagram(healthy_peer.datagram.unwrap()));
                    assert_eq!(bytes, expected);
                    got = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        assert!(l.h.has_route(Route::Datagram(healthy.datagram.unwrap())));
        assert!(l.c.has_route(Route::Stream(healthy_peer.inbound)));
        control_works(&mut l, &cx).await;
    });
}
