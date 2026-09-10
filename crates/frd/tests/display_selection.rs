#![cfg(target_os = "linux")]
//! Real UDP/TLS with explicitly synthetic admission and local display metadata.
//! Catalog publication, selection, stale-scope rejection and ownership are real.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod support;
use asupersync::cx::Cx;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    limits::ProtocolLimits,
};
use fr_transport::quic::{
    self, ChannelScope, ControlRoutes, Disposition, Policy, QuicRecords, Route,
};
use fr_wire::{
    display::{self, Catalog, Display, Message},
    input::{InputDelivery as T, InputDirection as D},
    negotiation::{Capability, ControlBinding, Offer, Role, Selection},
};
use frd::{
    display_selection::{DisplaySelection, Error, SelectedDisplay},
    media::{ObservationControl, host_now},
};
use std::time::Duration;
use support::{both, clock, runtime};
const WAIT: Duration = Duration::from_millis(2);
const TIMEOUT: Duration = Duration::from_secs(2);
fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn catalog() -> Catalog {
    let first = Display {
        handle: 21,
        geometry: DisplayGeometryGeneration::INITIAL,
        x: -320,
        y: 0,
        pixel_width: 320,
        pixel_height: 240,
        logical_width: 320,
        logical_height: 240,
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
    };
    Catalog::new(
        1,
        &[
            first,
            Display {
                handle: 22,
                x: 0,
                ..first
            },
        ],
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap()
}
fn selected() -> Selection {
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![Capability {
            name: display::CAPABILITY.into(),
            version: 1,
            required: true,
        }],
    }
    .select()
    .unwrap()
}
fn authority(cx: &Cx, session: RemoteSessionId) -> ObservationControl {
    let mut a = SessionAuthority::new(session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(host_now(cx).unwrap()).unwrap();
    ObservationControl::new(cx.clone(), a).unwrap()
}
struct Link {
    h: QuicRecords,
    c: QuicRecords,
    hr: ControlRoutes,
    cr: ControlRoutes,
    selection: Selection,
}
impl Link {
    async fn new(cx: &Cx) -> Self {
        let (c, h) = support::native_pair(cx, "localhost", quic::ALPN).await;
        let policy = Policy {
            critical_send_records: 1,
            ..Policy::default()
        };
        let (mut c, cr) = QuicRecords::bootstrap(c.unwrap(), cx, policy).unwrap();
        let (mut h, hr) = QuicRecords::bootstrap(h.unwrap(), cx, policy).unwrap();
        let cr = c.bind_control(cx, cr, 7, 4096, || true).unwrap();
        let hr = h.bind_control(cx, hr, 7, 4096, || true).unwrap();
        Self {
            h,
            c,
            hr,
            cr,
            selection: selected(),
        }
    }
    fn host(
        &mut self,
        c: ObservationControl,
        timeout: Duration,
    ) -> Result<DisplaySelection, Error> {
        DisplaySelection::host(
            &mut self.h,
            ChannelScope {
                control: self.hr,
                parent: parent(),
                selection: &self.selection,
            },
            c,
            catalog(),
            timeout,
        )
    }
    fn viewer(&mut self, cx: &Cx) -> DisplaySelection {
        DisplaySelection::viewer(
            cx.clone(),
            &mut self.c,
            ChannelScope {
                control: self.cr,
                parent: parent(),
                selection: &self.selection,
            },
            TIMEOUT,
        )
        .unwrap()
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
    async fn deliver_catalog(
        &mut self,
        cx: &Cx,
        h: &mut DisplaySelection,
        v: &mut DisplaySelection,
    ) {
        let end = clock(cx) + 1_000_000;
        while v.catalog(&self.c).unwrap().is_none() {
            assert!(clock(cx) < end);
            h.transmit(&mut self.h).unwrap();
            self.drive(cx).await;
            v.dispatch(&mut self.c).unwrap();
        }
    }
    async fn deliver_choice(
        &mut self,
        cx: &Cx,
        h: &mut DisplaySelection,
        v: &mut DisplaySelection,
    ) {
        let end = clock(cx) + 1_000_000;
        while !h.is_complete() {
            assert!(clock(cx) < end);
            v.transmit(&mut self.c).unwrap();
            self.drive(cx).await;
            h.dispatch(&mut self.h).unwrap();
        }
    }
    async fn complete(
        &mut self,
        cx: &Cx,
        c: ObservationControl,
    ) -> (SelectedDisplay, SelectedDisplay) {
        let mut h = self.host(c, TIMEOUT).unwrap();
        let mut v = self.viewer(cx);
        self.deliver_catalog(cx, &mut h, &mut v).await;
        v.choose(&self.c, 22).unwrap();
        self.deliver_choice(cx, &mut h, &mut v).await;
        (h.finish(&self.h).unwrap(), v.finish(&self.c).unwrap())
    }
    async fn send_select(&mut self, cx: &Cx, m: &Message) {
        let mut b = [0; display::SELECT_BYTES];
        let n = display::encode(
            m,
            parent(),
            &self.selection.limits,
            &mut b,
            D::ViewerToHost,
            T::Reliable,
        )
        .unwrap();
        let until = clock(cx) + 1_000_000;
        loop {
            match self
                .c
                .send(cx, Route::Stream(self.cr.outbound), &b[..n], until, || true)
            {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => self.drive(cx).await,
                Err(e) => panic!("{e:?}"),
            }
        }
        self.drive(cx).await;
    }
}
#[test]
fn actual_catalog_requires_explicit_choice_and_handoff_keeps_exact_scope() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let control = authority(&cx, parent().remote_session);
        let mut h = l.host(control.clone(), TIMEOUT).unwrap();
        let mut v = l.viewer(&cx);
        assert!(v.catalog(&l.c).unwrap().is_none());
        assert_eq!(v.choose(&l.c, 22), Err(Error::Order));
        l.deliver_catalog(&cx, &mut h, &mut v).await;
        assert_eq!(v.catalog(&l.c).unwrap(), Some(&catalog()));
        assert!(!v.is_complete());
        assert!(!h.is_complete());
        assert!(v.choose(&l.c, 99).is_err());
        v.choose(&l.c, 22).unwrap();
        assert_eq!(v.choose(&l.c, 21), Err(Error::Order));
        assert!(v.transmit(&mut l.c).unwrap());
        assert!(!h.is_complete(), "queue admission is not peer acceptance");
        l.deliver_choice(&cx, &mut h, &mut v).await;
        let (h, v) = (h.finish(&l.h).unwrap(), v.finish(&l.c).unwrap());
        assert_eq!(h.display(&l.h).unwrap(), catalog().find(22).unwrap());
        assert_eq!(h.binding(&l.h, 8).unwrap(), v.binding(&l.c, 8).unwrap());
        assert!(h.binding(&l.h, 7).is_err());
        assert!(h.binding(&l.h, 0).is_err());
        assert!(control.check().is_ok());
        assert!(
            l.host(control.clone(), TIMEOUT).is_err(),
            "only one selection lifetime"
        );
        let mut wrong = v.binding(&l.c, 8).unwrap();
        wrong.geometry = DisplayGeometryGeneration::from_raw(1);
        assert_eq!(v.check_binding(&l.c, wrong), Err(Error::ChangedDisplay));
        assert!(!format!("{h:?} {v:?}").contains("handle"));
    });
}
#[test]
fn real_credit_pressure_preserves_catalog_and_original_exclusive_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let mut buffer = [0; display::MAX_CATALOG_BYTES];
        let length = display::encode(
            &Message::Catalog(catalog()),
            parent(),
            &l.selection.limits,
            &mut buffer,
            D::HostToViewer,
            T::Reliable,
        )
        .unwrap();
        // Occupy the genuine critical-record slot, then drain this fixture record
        // separately before the production viewer consumes the actual exchange.
        l.h.send(
            &cx,
            Route::Stream(l.hr.outbound),
            &buffer[..length],
            clock(&cx) + 1_000_000,
            || true,
        )
        .unwrap();
        let control = authority(&cx, parent().remote_session);
        let mut h = l.host(control, TIMEOUT).unwrap();
        let until = h.deadline_us();
        for _ in 0..8 {
            assert!(!h.transmit(&mut l.h).unwrap());
            assert_eq!(h.deadline_us(), until);
        }
        let mut drained = false;
        let end = clock(&cx) + 1_000_000;
        while !drained {
            assert!(clock(&cx) < end);
            l.drive(&cx).await;
            l.c.receive_ready(
                &cx,
                || true,
                |r| r == Route::Stream(l.cr.inbound),
                |_, bytes| {
                    assert_eq!(bytes, &buffer[..length]);
                    drained = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        let mut v = l.viewer(&cx);
        l.deliver_catalog(&cx, &mut h, &mut v).await;
        assert_eq!(h.deadline_us(), until);
        assert_eq!(v.catalog(&l.c).unwrap(), Some(&catalog()));
        v.choose(&l.c, 21).unwrap();
        l.deliver_choice(&cx, &mut h, &mut v).await;
        assert_eq!(h.finish(&l.h).unwrap().display(&l.h).unwrap().handle, 21);
    });
}
#[test]
fn stale_geometry_crops_and_replayed_selection_never_replace_the_chosen_scope() {
    for alteration in 0..4 {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut l = Link::new(&cx).await;
            let control = authority(&cx, parent().remote_session);
            let mut h = l.host(control.clone(), TIMEOUT).unwrap();
            let mut v = l.viewer(&cx);
            l.deliver_catalog(&cx, &mut h, &mut v).await;
            let mut request = catalog().selection(22).unwrap();
            match alteration {
                0 => request.revision += 1,
                1 => request.geometry = DisplayGeometryGeneration::from_raw(1),
                2 => request.width -= 1,
                _ => {
                    v.choose(&l.c, 22).unwrap();
                    l.deliver_choice(&cx, &mut h, &mut v).await;
                }
            }
            l.send_select(&cx, &Message::Select(request)).await;
            let end = clock(&cx) + 1_000_000;
            loop {
                if h.dispatch(&mut l.h).is_err() {
                    break;
                }
                assert!(clock(&cx) < end);
                l.drive(&cx).await;
            }
            assert!(control.check().is_err());
            assert!(h.finish(&l.h).is_err());
        });
    }
}
#[test]
fn early_selection_expiry_and_abandoned_exchange_are_terminal_without_media() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let control = authority(&cx, parent().remote_session);
        let mut h = l.host(control.clone(), TIMEOUT).unwrap();
        l.send_select(&cx, &Message::Select(catalog().selection(21).unwrap()))
            .await;
        let end = clock(&cx) + 1_000_000;
        loop {
            if h.dispatch(&mut l.h).is_err() {
                break;
            }
            assert!(clock(&cx) < end);
            l.drive(&cx).await;
        }
        assert!(control.check().is_err());
    });
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let control = authority(&cx, parent().remote_session);
        let mut h = l.host(control.clone(), Duration::from_millis(10)).unwrap();
        let deadline = h.deadline_us();
        asupersync::time::sleep(cx.now(), Duration::from_millis(20)).await;
        assert_eq!(h.transmit(&mut l.h), Err(Error::Expired));
        assert_eq!(h.deadline_us(), deadline);
        assert!(control.check().is_err());
    });
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let control = authority(&cx, parent().remote_session);
        drop(l.host(control.clone(), TIMEOUT).unwrap());
        assert!(control.check().is_err());
    });
}
#[test]
fn foreign_connection_cannot_receive_catalog_or_borrow_completed_choice() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let mut foreign = Link::new(&cx).await;
        let control = authority(&cx, parent().remote_session);
        let mut h = l.host(control.clone(), TIMEOUT).unwrap();
        let before = foreign.h.usage();
        assert_eq!(h.transmit(&mut foreign.h), Err(Error::ForeignConnection));
        assert_eq!(foreign.h.usage(), before);
        assert!(!foreign.h.is_closed());
        assert!(control.check().is_err());
    });
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let foreign = Link::new(&cx).await;
        let control = authority(&cx, parent().remote_session);
        let (h, _v) = l.complete(&cx, control.clone()).await;
        let before = foreign.h.usage();
        assert_eq!(h.display(&foreign.h), Err(Error::ForeignConnection));
        assert_eq!(foreign.h.usage(), before);
        assert!(!foreign.h.is_closed());
        assert!(control.check().is_err());
    });
}
#[test]
fn unselected_capability_foreign_authority_and_bad_deadlines_do_not_reserve() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let control = authority(&cx, parent().remote_session);
        l.selection.capabilities.clear();
        assert!(matches!(
            l.host(control.clone(), TIMEOUT),
            Err(Error::CapabilityMissing)
        ));
        l.selection = selected();
        assert!(matches!(
            l.host(control.clone(), Duration::ZERO),
            Err(Error::Configuration)
        ));
        let foreign = authority(&cx, RemoteSessionId::from_raw(99));
        assert!(matches!(
            l.host(foreign, TIMEOUT),
            Err(Error::Configuration)
        ));
        assert!(l.host(control, TIMEOUT).is_ok());
    });
}
#[test]
fn completed_choice_survives_other_catalog_changes_but_not_selected_output_change() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let control = authority(&cx, parent().remote_session);
        let (h, _v) = l.complete(&cx, control.clone()).await;
        let d = h.display(&l.h).unwrap();
        let current = Catalog::new(2, &[d], &ProtocolLimits::ABSOLUTE).unwrap();
        h.revalidate(&l.h, &current).unwrap();
        assert!(control.check().is_ok());
        let changed = Catalog::new(3, &[Display { x: 4, ..d }], &ProtocolLimits::ABSOLUTE).unwrap();
        assert_eq!(h.revalidate(&l.h, &changed), Err(Error::ChangedDisplay));
        assert!(control.check().is_err());
        assert!(h.binding(&l.h, 8).is_err());
    });
}
