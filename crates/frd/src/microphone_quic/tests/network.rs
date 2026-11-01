//! Actual original UDP/TLS connections and ticketed, role-specific attachments.
//! Tailnet admission and input authority establishment are explicit test policy.
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

pub fn parent() -> ControlBinding {
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
pub struct Link {
    pub c: QuicRecords,
    pub h: QuicRecords,
    pub cr: ControlRoutes,
    pub hr: ControlRoutes,
    pub selection: Selection,
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
        let cr = c.bind_control(cx, cr, parent().id, 4096, || true).unwrap();
        let hr = h.bind_control(cx, hr, parent().id, 4096, || true).unwrap();
        let mut capabilities: Vec<_> = [
            attachment::CAPABILITY,
            attachment::INPUT_CAPABILITY,
            fr_wire::audio_up::CAPABILITY,
        ]
        .into_iter()
        .map(|name| Capability {
            name: name.into(),
            version: 1,
            required: true,
        })
        .collect();
        capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        let selection = Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::RequestControl,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities,
        }
        .select()
        .unwrap();
        Self {
            c,
            h,
            cr,
            hr,
            selection,
        }
    }
    pub async fn drive(&mut self, cx: &Cx) {
        let (a, b) = Box::pin(both(
            self.h.drive(cx, Duration::from_micros(200), || true),
            self.c.drive(cx, Duration::from_micros(200), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
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
        let until = clock(cx) + 1_000_000;
        let mut sent = false;
        let mut c = loop {
            assert!(clock(cx) < until, "real microphone attachment offer");
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
                    |route| ready.get() && route == Route::Stream(self.cr.inbound),
                    |_, record| {
                        ready.set(false);
                        bytes = Some(record.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(bytes) = bytes {
                break self
                    .c
                    .accept_media_channel(
                        cx,
                        ChannelScope {
                            control: self.cr,
                            parent: parent(),
                            selection: &self.selection,
                        },
                        &bytes,
                        Duration::from_secs(2),
                        || true,
                    )
                    .unwrap();
            }
        };
        let until = clock(cx) + 1_500_000;
        let (mut ha, mut ca) = (None, None);
        while ha.is_none() || ca.is_none() {
            assert!(clock(cx) < until, "real microphone attachment completion");
            h.transmit(&mut self.h, cx, || true).unwrap();
            c.transmit(&mut self.c, cx, || true).unwrap();
            self.drive(cx).await;
            h.dispatch(&mut self.h, cx, || true).unwrap();
            c.dispatch(&mut self.c, cx, || true).unwrap();
            ha = h.finish(&mut self.h, cx, || true).unwrap();
            ca = c.finish(&mut self.c, cx, || true).unwrap();
        }
        (h, c)
    }
}
