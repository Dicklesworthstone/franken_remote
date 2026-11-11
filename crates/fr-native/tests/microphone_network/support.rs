//! Local policy fixtures carried over actual certificate-authenticated UDP/TLS.
//! Presentation is synthetic; the accepted grant, original input attachment,
//! `AudioUp` attachment and all microphone records use the production owners.
use super::{authority, network};
use asupersync::cx::Cx;
use fr_client::{
    audio::uplink::Profile,
    control_grant::RequestControl,
    input::{ClientInstant, InputClient},
};
use fr_core::{
    audio::{AudioChannels, AudioDirection},
    ids::*,
};
use fr_native::audio_process::{
    network::{Options, Uplink},
    transmit::Source,
};
use fr_transport::quic::{
    ALPN, ChannelRequest, ChannelScope, ControlRoutes, Disposition, MediaChannel, Policy,
    QuicRecords, Route, SendAdmission, StreamRoute, audio::AudioUpChannel,
};
use fr_wire::{
    attachment::{self, MediaRole, Ticket},
    audio::{self, AudioConfiguration, AudioConfigured},
    audio_up, control,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::{Capability, Selection},
};
use std::{
    cell::Cell,
    path::Path,
    time::{Duration, Instant},
};

pub const CHANNEL: u32 = 73;
pub const WAIT: Duration = Duration::from_millis(2);
pub fn now(origin: Instant) -> ClientInstant {
    authority::client(u64::try_from(origin.elapsed().as_micros()).unwrap())
}
pub fn options(server: &Path) -> Options {
    Options {
        image: Path::new(env!("CARGO_BIN_EXE_fr-audio-worker")).into(),
        source: Source {
            server: server.into(),
            name: "fr_test_mic".into(),
            bitrate: 64_000,
        },
        profile: Profile {
            channels: AudioChannels::Mono,
            frame_duration_ms: 20,
            max_packet_bytes: 1000,
            max_record_bytes: audio::AUDIO_PACKET_OVERHEAD + 1000,
            jitter_target_ms: 20,
        },
    }
}
pub fn selection() -> Selection {
    let mut capabilities: Vec<_> = [
        (attachment::CAPABILITY, attachment::VERSION),
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (audio_up::CAPABILITY, audio_up::VERSION),
    ]
    .into_iter()
    .map(|(name, version)| Capability {
        name: name.into(),
        version,
        required: false,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Selection {
        version: 0,
        profile: 1,
        profile_version: 0,
        role: fr_wire::negotiation::Role::RequestControl,
        limits: authority::L,
        capabilities,
        picture: None,
    }
}
fn binding(id: u32) -> Binding {
    Binding {
        parent: fr_wire::negotiation::ControlBinding {
            id,
            ..authority::grant().request.parent
        },
        display: 14,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}

pub struct Link {
    pub client: QuicRecords,
    pub host: QuicRecords,
    pub client_control: ControlRoutes,
    pub host_control: ControlRoutes,
    pub selection: Selection,
    pub microphone_out: Option<StreamRoute>,
    // Keep the one original completed input attachment alive, not numeric
    // IDs recreated after the microphone handshake or native source restart.
    input: Option<(MediaChannel, MediaChannel)>,
}
impl Link {
    pub async fn new(cx: &Cx) -> Self {
        Self::with_policy(cx, Policy::default()).await
    }
    pub async fn with_policy(cx: &Cx, policy: Policy) -> Self {
        let (client, host) = network::native_pair(cx, "localhost", ALPN).await;
        let (mut client, client_control) =
            QuicRecords::bootstrap(client.unwrap(), cx, policy).unwrap();
        let (mut host, host_control) = QuicRecords::bootstrap(host.unwrap(), cx, policy).unwrap();
        let client_control = client
            .bind_control(cx, client_control, 7, 4096, || true)
            .unwrap();
        let host_control = host
            .bind_control(cx, host_control, 7, 4096, || true)
            .unwrap();
        let mut link = Self {
            client,
            host,
            client_control,
            host_control,
            selection: selection(),
            microphone_out: None,
            input: None,
        };
        let input = link.attach(cx, MediaRole::Input, 9).await;
        link.input = Some(input);
        link
    }
    pub async fn drive(&mut self, cx: &Cx) {
        let (host, client) = network::both(
            self.host.drive(cx, WAIT, || true),
            self.client.drive(cx, WAIT, || true),
        )
        .await;
        host.unwrap();
        client.unwrap();
    }
    pub async fn drive_uplink(&mut self, cx: &Cx, uplink: &mut Uplink) {
        let (host, client) = network::both(
            self.host.drive(cx, WAIT, || true),
            uplink.drive(&mut self.client, WAIT, || true),
        )
        .await;
        host.unwrap();
        client.unwrap();
    }
    async fn attach(&mut self, cx: &Cx, role: MediaRole, id: u32) -> (MediaChannel, MediaChannel) {
        let parent = authority::grant().request.parent;
        let mut host = self
            .host
            .offer_media_role(
                cx,
                ChannelScope {
                    control: self.host_control,
                    parent,
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
        let until = network::clock(cx) + 1_500_000;
        let mut client = None;
        let mut offered = false;
        while client.is_none() {
            assert!(
                network::clock(cx) < until,
                "actual attachment offer stalled"
            );
            if !offered {
                offered = host.transmit(&mut self.host, cx, || true).unwrap();
            }
            self.drive(cx).await;
            let once = Cell::new(true);
            let mut received = None;
            self.client
                .receive_ready(
                    cx,
                    || true,
                    |route| once.get() && route == Route::Stream(self.client_control.inbound),
                    |_, bytes| {
                        once.set(false);
                        received = Some(bytes.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(bytes) = received {
                client = Some(
                    self.client
                        .accept_media_channel(
                            cx,
                            ChannelScope {
                                control: self.client_control,
                                parent,
                                selection: &self.selection,
                            },
                            &bytes,
                            Duration::from_secs(2),
                            || true,
                        )
                        .unwrap(),
                );
            }
        }
        let mut client = client.unwrap();
        loop {
            assert!(
                network::clock(cx) < until,
                "actual attachment exchange stalled"
            );
            host.transmit(&mut self.host, cx, || true).unwrap();
            client.transmit(&mut self.client, cx, || true).unwrap();
            self.drive(cx).await;
            host.dispatch(&mut self.host, cx, || true).unwrap();
            client.dispatch(&mut self.client, cx, || true).unwrap();
            let host_done = host.finish(&mut self.host, cx, || true).unwrap();
            let client_done = client.finish(&mut self.client, cx, || true).unwrap();
            if host_done.is_some() && client_done.is_some() {
                break;
            }
        }
        (host, client)
    }
    pub async fn microphone(&mut self, cx: &Cx) -> (AudioUpChannel, AudioUpChannel) {
        let (host, client) = self.attach(cx, MediaRole::AudioUp, CHANNEL).await;
        self.microphone_out = Some(host.completed_on(&self.host).unwrap().outbound);
        (
            AudioUpChannel::new(&self.host, host).unwrap(),
            AudioUpChannel::new(&self.client, client).unwrap(),
        )
    }
    /// Accept the fixture's actual Granted record after transport delivery on
    /// this original critical control pair. No second owner is reconstructed
    /// for `AudioUp` or capture renewal.
    pub async fn controller(&mut self, cx: &Cx) -> (InputClient, Instant) {
        let origin = Instant::now();
        let grant = authority::grant();
        let mut request = RequestControl::new(
            grant.request,
            grant.input_channel,
            authority::L,
            authority::client(0),
        )
        .unwrap();
        request.sent(authority::client(0)).unwrap();
        let mut bytes = [0; control::GRANTED_BYTES];
        control::encode_granted(
            grant,
            &mut bytes,
            &authority::L,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        self.host
            .send(
                cx,
                Route::Stream(self.host_control.outbound),
                &bytes,
                network::clock(cx) + 1_000_000,
                || true,
            )
            .unwrap();
        let mut received = None;
        while received.is_none() {
            assert!(origin.elapsed() < Duration::from_secs(1));
            self.drive(cx).await;
            self.client
                .receive_ready(
                    cx,
                    || true,
                    |route| route == Route::Stream(self.client_control.inbound),
                    |_, bytes| {
                        received = Some(bytes.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        let at = now(origin);
        let mut input = request
            .accept(
                &received.unwrap(),
                authority::correlation(0),
                authority::policy(),
                at,
            )
            .unwrap()
            .1;
        authority::ready(&mut input, 0, at.0 - authority::client(0).0);
        (input, origin)
    }
    pub fn uplink(
        &mut self,
        cx: &Cx,
        lane: AudioUpChannel,
        input: &mut InputClient,
        origin: Instant,
        server: &Path,
    ) -> Uplink {
        Uplink::attach(
            &mut self.client,
            lane,
            input,
            &self.selection,
            true,
            options(server),
            cx,
            || now(origin),
        )
        .unwrap()
    }
    pub async fn next_record(
        &mut self,
        cx: &Cx,
        uplink: &mut Uplink,
        lane: &AudioUpChannel,
    ) -> Vec<u8> {
        let until = network::clock(cx) + 1_000_000;
        loop {
            assert!(
                network::clock(cx) < until,
                "microphone record failed to arrive"
            );
            self.drive_uplink(cx, uplink).await;
            let mut received = None;
            lane.dispatch(
                &mut self.host,
                cx,
                || true,
                |_| true,
                |_, bytes, _| {
                    received = Some(bytes.to_vec());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
            if let Some(bytes) = received {
                return bytes;
            }
        }
    }
    pub fn acknowledge(
        &mut self,
        cx: &Cx,
        lane: &AudioUpChannel,
        offer: AudioConfiguration,
        accepted: bool,
    ) {
        let mut bytes = [0; audio::AUDIO_CONFIGURED_RECORD_BYTES];
        audio::encode_configured(
            &AudioConfigured {
                direction: AudioDirection::Uplink,
                generation: offer.generation,
                accepted,
                actual_channels: offer.channels,
                actual_sample_rate: offer.sample_rate,
                actual_frame_duration_ms: offer.frame_duration_ms,
            },
            CHANNEL,
            &mut bytes,
        )
        .unwrap();
        assert!(matches!(
            lane.send_control_prepared(
                &mut self.host,
                cx,
                &bytes,
                network::clock(cx) + 1_000_000,
                || true,
                || Ok::<_, ()>(())
            ),
            Ok(SendAdmission::Accepted)
        ));
    }
    pub async fn assert_original_control_usable(&mut self, cx: &Cx) {
        // A structurally valid control record is independent of AudioUp's
        // retirement; asserting its exact bytes proves real surviving I/O.
        let mut bytes = [0; control::GRANTED_BYTES];
        control::encode_granted(
            authority::grant(),
            &mut bytes,
            &authority::L,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        let until = network::clock(cx) + 1_000_000;
        self.host
            .send(
                cx,
                Route::Stream(self.host_control.outbound),
                &bytes,
                until,
                || true,
            )
            .unwrap();
        let mut got = false;
        while !got {
            assert!(network::clock(cx) < until);
            self.drive(cx).await;
            self.client
                .receive_ready(
                    cx,
                    || true,
                    |route| route == Route::Stream(self.client_control.inbound),
                    |_, received| {
                        assert_eq!(received, bytes);
                        got = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert!(!self.host.is_closed() && !self.client.is_closed());
    }
}
