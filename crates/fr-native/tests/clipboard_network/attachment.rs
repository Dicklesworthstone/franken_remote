//! Admission and presentation are explicit fixtures; sockets, TLS, attachment,
//! X11 selections and worker handoff are the real production implementations.
use super::transport_support::{self as support, both, clock};
use asupersync::cx::Cx;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_transport::quic::*;
use fr_wire::{
    attachment::{self, MediaRole, Ticket},
    decoder::Binding,
    negotiation::{Capability, ControlBinding, Offer, Role, Selection},
};
use std::{cell::Cell, time::Duration};
const WAIT: Duration = Duration::from_micros(200);
pub fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
pub fn binding(id: u32) -> Binding {
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
        role: Role::RequestControl,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: {
            let mut capabilities: Vec<_> = [
                attachment::CAPABILITY,
                attachment::INPUT_CAPABILITY,
                attachment::CLIPBOARD_CAPABILITY,
                fr_wire::clipboard::CAPABILITY,
            ]
            .into_iter()
            .map(|name| Capability {
                name: name.into(),
                version: 1,
                required: true,
            })
            .collect();
            capabilities.sort_by(|a, b| a.name.cmp(&b.name));
            capabilities
        },
    }
    .select()
    .unwrap()
}
pub struct Link {
    pub c: QuicRecords,
    pub h: QuicRecords,
    pub cr: ControlRoutes,
    pub hr: ControlRoutes,
    selection: Selection,
}
impl Link {
    pub async fn new(cx: &Cx) -> Self {
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
    pub async fn drive(&mut self, cx: &Cx) {
        let (a, b) = Box::pin(both(
            self.h.drive(cx, WAIT, || true),
            self.c.drive(cx, WAIT, || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
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

impl Link {
    pub async fn attach(
        &mut self,
        cx: &Cx,
        id: u32,
        role: MediaRole,
    ) -> (MediaChannel, MediaChannel) {
        let mut h = self
            .h
            .offer_media_role(
                cx,
                ChannelScope {
                    control: self.hr,
                    parent: parent(),
                    selection: &self.selection,
                },
                ChannelRequest {
                    binding: binding(id),
                    ticket: Ticket(1000 + u128::from(id)),
                    timeout: Duration::from_secs(2),
                },
                role,
                || true,
            )
            .unwrap();
        let mut c = self.viewer(cx, &mut h).await;
        self.attached(cx, &mut h, &mut c).await;
        (h, c)
    }
    pub fn record_cap(&mut self, cap: u32) {
        self.selection.limits = ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
            max_control_message_bytes: Some(cap),
            ..fr_core::limits::LimitOverrides::default()
        })
        .unwrap();
    }
}
