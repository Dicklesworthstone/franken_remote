//! Explicit admission/authority fixtures around the real UDP/TLS transport.
#[allow(dead_code)]
#[path = "../../../fr-transport/tests/support/mod.rs"]
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
use support::both;
pub use support::{clock, runtime};
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
pub struct Link {
    pub c: QuicRecords,
    pub h: QuicRecords,
    cr: ControlRoutes,
    hr: ControlRoutes,
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

use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    input::{DesktopPoint, InputBounds, InputCredentials},
    input_submission::{Capabilities, InputSession},
    time::HostInstant,
};
use fr_files::{
    quic::{Configuration, HostReceiver},
    receive::{DropDirectory, Limits},
    session::{Permission, Policy as FilePolicy},
};
use fr_transport::quic::files::FilesChannel;
use fr_wire::{
    attachment::MediaRole,
    files::{self, Body, Message as FileMessage},
};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn owner(cx: &Cx) -> InputSession {
    let c = InputCredentials {
        session: parent().remote_session,
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: fr_core::input::InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let now = HostInstant::from_micros(clock(cx));
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(c.lease, now).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        now,
    )
    .unwrap()
}
async fn channel(l: &mut Link, cx: &Cx, id: u32, role: MediaRole) -> (MediaChannel, MediaChannel) {
    let mut h =
        l.h.offer_media_role(
            cx,
            ChannelScope {
                control: l.hr,
                parent: parent(),
                selection: &l.selection,
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
    let mut c = l.viewer(cx, &mut h).await;
    l.attached(cx, &mut h, &mut c).await;
    (h, c)
}
pub struct Running {
    pub link: Link,
    pub host: HostReceiver,
    pub host_files: StreamRoute,
    pub client: FilesChannel,
    pub input: InputSession,
    pub path: PathBuf,
}
impl Running {
    pub async fn new(cx: &Cx) -> Self {
        let mut link = Link::new(cx).await;
        link.selection.role = Role::RequestControl;
        for (name, version) in [
            (attachment::INPUT_CAPABILITY, 1),
            (attachment::FILES_CAPABILITY, 1),
            (files::CAPABILITY, 1),
        ] {
            link.selection.capabilities.push(Capability {
                name: name.into(),
                version,
                required: true,
            });
        }
        link.selection
            .capabilities
            .sort_by(|a, b| a.name.cmp(&b.name));
        let _input_channel = channel(&mut link, cx, 8, MediaRole::Input).await;
        let (hc, cc) = channel(&mut link, cx, 9, MediaRole::Files).await;
        let input = owner(cx);
        let path = std::env::temp_dir().join(format!(
            "fr-native-files-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let directory = DropDirectory::open(
            &path,
            Limits {
                max_file_bytes: 1_000_000,
                max_reserved_bytes: 1_000_000,
                max_transfers: 1,
            },
        )
        .unwrap();
        let host_files = hc.completed_on(&link.h).unwrap().outbound;
        let h = FilesChannel::new(&link.h, hc, InputLeaseId::from_raw(2), 456).unwrap();
        let client = FilesChannel::new(&link.c, cc, InputLeaseId::from_raw(2), 456).unwrap();
        let host = HostReceiver::spawn(
            cx.clone(),
            &mut link.h,
            h,
            &input,
            Configuration {
                directory,
                permission: Permission::new(true),
                policy: FilePolicy::conservative(),
                reply_lifetime: Duration::from_secs(1),
            },
        )
        .unwrap();
        Self {
            link,
            host,
            host_files,
            client,
            input,
            path,
        }
    }
    pub fn packet(&self, id: u64, body: Body<'_>) -> Vec<u8> {
        let mut bytes = vec![0; self.client.limits().record_bytes()];
        let n = files::encode(
            FileMessage { id, body },
            self.client.outgoing(),
            self.client.limits(),
            &mut bytes,
        )
        .unwrap();
        bytes.truncate(n);
        bytes
    }
    pub async fn pump(&mut self, cx: &Cx) {
        self.host.service(&mut self.link.h, || true).unwrap();
        self.link.drive(cx).await;
        self.host.service(&mut self.link.h, || true).unwrap();
    }
    pub async fn send(&mut self, cx: &Cx, body: Body<'_>) {
        let bytes = self.packet(1, body);
        let deadline = clock(cx) + 1_000_000;
        loop {
            assert!(clock(cx) < deadline);
            match self
                .client
                .send(&mut self.link.c, cx, &bytes, deadline, || true)
            {
                Ok(()) => break,
                Err(Error::Backpressure) => self.pump(cx).await,
                e => panic!("file send: {e:?}"),
            }
        }
    }
    pub async fn reply(&mut self, cx: &Cx) -> Vec<u8> {
        let deadline = clock(cx) + 1_000_000;
        loop {
            assert!(clock(cx) < deadline, "file reply timed out");
            self.pump(cx).await;
            let mut reply = None;
            self.client
                .dispatch(
                    &mut self.link.c,
                    cx,
                    || true,
                    |b| {
                        reply = Some(b.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(reply) = reply {
                return reply;
            }
        }
    }
    pub async fn staged(&mut self, cx: &Cx, bytes: u64) {
        let deadline = clock(cx) + 1_000_000;
        while self.host.progress().is_none_or(|p| p.staged_bytes != bytes) {
            assert!(clock(cx) < deadline, "disk staging timed out");
            self.pump(cx).await;
        }
    }
    pub fn input_live(&self, cx: &Cx) {
        assert!(
            self.input
                .monitor()
                .deadline(HostInstant::from_micros(clock(cx)))
                .is_ok()
        );
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.host.stop();
        let _ = self.host.retire(&mut self.link.h);
        self.link.c.close();
        self.link.h.close();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !self.host.cleanup_finished() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let _ = self.host.try_finish_cleanup();
        fs::remove_dir_all(&self.path).unwrap();
    }
}
use asupersync::net::atp::transport_tcp::ManifestEntry;
use asupersync::net::atp::{
    protocol::{Frame, FrameType, ProtocolVersion},
    transport_common::{StagedEntryReceive, flat_merkle_root_from_digests, hex_encode},
    transport_tcp::TransferManifest,
};
fn wire(kind: FrameType, payload: Vec<u8>) -> Vec<u8> {
    Frame::new(ProtocolVersion::CURRENT, kind, payload)
        .unwrap()
        .to_wire_bytes()
        .unwrap()
}
pub fn manifest(name: &str, bytes: &[u8]) -> TransferManifest {
    let mut digest = StagedEntryReceive::new(PathBuf::from("test-streaming-hash-only"));
    digest.update_with_chunk(bytes);
    let (digest, _, _) = digest.finalize(name.into());
    TransferManifest {
        // Stable ATP label is metadata, NOT a controller/attachment credential.
        transfer_id: "0123456789abcdef0123456789abcdef".into(),
        root_name: name.into(),
        is_directory: false,
        total_bytes: bytes.len() as u64,
        merkle_root_hex: flat_merkle_root_from_digests(std::slice::from_ref(&digest)),
        metadata_root_hex: None,
        directory_metadata: None,
        delta_manifest: None,
        entries: vec![ManifestEntry {
            index: 0,
            rel_path: name.into(),
            size: bytes.len() as u64,
            sha256_hex: hex_encode(&digest.content_sha256),
            metadata: None,
            members: vec![],
        }],
    }
}
pub fn offered(offering: &TransferManifest) -> Vec<u8> {
    wire(
        FrameType::ObjectManifest,
        serde_json::to_vec(offering).unwrap(),
    )
}
pub fn data(index: u32, offset: u64, bytes: &[u8]) -> Vec<u8> {
    let mut payload = index.to_be_bytes().to_vec();
    payload.extend_from_slice(&offset.to_be_bytes());
    payload.extend_from_slice(bytes);
    wire(FrameType::ObjectData, payload)
}
