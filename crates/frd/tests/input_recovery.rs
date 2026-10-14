#![cfg(target_os = "linux")]
//! Real UDP/TLS and production attachment/record drivers, with explicit local
//! selection and authorization fixtures. No native input or HEVC claim.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod network;

use asupersync::{cx::Cx, types::Budget};
use fr_core::{ids::*, input::*, limits::ProtocolLimits};
use fr_transport::quic::{
    self, ChannelRequest, ChannelScope, ControlRoutes, Disposition, MediaChannel, Messages, Policy,
    Priority, QuicRecords, Route, StreamRoute,
};
use fr_wire::{
    Kind,
    attachment::{self, MediaRole, Ticket},
    decoder::Binding,
    input::{InputDelivery, InputDirection, encode_input},
    negotiation::{Capability, ControlBinding, Role, Selection},
};
use frd::{
    input_quic::{Error, NegotiatedInput},
    media_quic::NegotiatedMedia,
};
use std::{future::Future, time::Duration};

const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;

fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn binding(id: u32, recovery: RecoveryGeneration) -> Binding {
    Binding {
        parent: ControlBinding { id, ..parent() },
        display: 9,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn selection(recovery: bool) -> Selection {
    let mut capabilities: Vec<_> = [
        fr_wire::decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
        attachment::INPUT_CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    if recovery {
        capabilities.push(Capability {
            name: fr_wire::recovery_request::CAPABILITY.into(),
            version: fr_wire::recovery_request::VERSION,
            required: true,
        });
    }
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Selection {
        version: 0,
        profile: 1,
        profile_version: 0,
        role: Role::RequestControl,
        limits: LIMITS,
        capabilities,
    }
}
fn now(cx: &Cx) -> u64 {
    network::clock(cx)
}
fn run<F, Fut>(f: F)
where
    F: FnOnce(Cx) -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(Box::pin(async move {
        asupersync::time::timeout(cx.now(), Duration::from_secs(12), Box::pin(f(cx.clone())))
            .await
            .unwrap();
    }));
}
struct Pair {
    client: QuicRecords,
    host: QuicRecords,
    client_control: ControlRoutes,
    host_control: ControlRoutes,
    selection: Selection,
}
impl Pair {
    async fn new(cx: &Cx, recovery: bool) -> Self {
        let (client, host) = network::native_pair(cx, "localhost", quic::ALPN).await;
        let (mut client, mut host) = (client.unwrap(), host.unwrap());
        let inbound = StreamRoute {
            stream: client.connection_mut().open_uni_stream(cx).unwrap(),
            binding: parent().id,
            messages: Messages::SessionControl,
            priority: Priority::Critical,
            maximum: 1024,
            outbound: false,
        };
        let outbound = StreamRoute {
            stream: host.connection_mut().open_uni_stream(cx).unwrap(),
            outbound: true,
            ..inbound
        };
        let host_control = ControlRoutes { inbound, outbound };
        let client_control = ControlRoutes {
            outbound: StreamRoute {
                outbound: true,
                ..inbound
            },
            inbound: StreamRoute {
                outbound: false,
                ..outbound
            },
        };
        let host =
            QuicRecords::new(host, cx, &[inbound, outbound], &[], Policy::default()).unwrap();
        let client = QuicRecords::new(
            client,
            cx,
            &[client_control.inbound, client_control.outbound],
            &[],
            Policy::default(),
        )
        .unwrap();
        Self {
            client,
            host,
            client_control,
            host_control,
            selection: selection(recovery),
        }
    }
    async fn drive(&mut self, cx: &Cx) {
        let (a, b) = Box::pin(network::both(
            self.client.drive(cx, Duration::from_millis(1), || true),
            self.host.drive(cx, Duration::from_millis(1), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    async fn attach(
        &mut self,
        cx: &Cx,
        role: MediaRole,
        binding: Binding,
    ) -> (MediaChannel, MediaChannel) {
        let mut host = self
            .host
            .offer_media_role(
                cx,
                ChannelScope {
                    control: self.host_control,
                    parent: parent(),
                    selection: &self.selection,
                },
                ChannelRequest {
                    binding,
                    ticket: Ticket(u128::from(binding.parent.id) + 100),
                    timeout: Duration::from_secs(2),
                },
                role,
                || true,
            )
            .unwrap();
        let mut client: Option<MediaChannel> = None;
        let until = now(cx) + 2_000_000;
        loop {
            assert!(now(cx) < until, "attachment timed out");
            host.transmit(&mut self.host, cx, || true).unwrap();
            if let Some(channel) = &mut client {
                channel.transmit(&mut self.client, cx, || true).unwrap();
            }
            self.drive(cx).await;
            let mut offer = None;
            let control = self.client_control.inbound;
            self.client
                .receive_ready(
                    cx,
                    || true,
                    |route| client.is_none() && route == Route::Stream(control),
                    |_, bytes| {
                        let kind = (Kind::StreamBinding as u16).to_be_bytes();
                        assert_eq!(bytes.get(6..8), Some(kind.as_slice()));
                        offer = Some(bytes.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(offer) = offer {
                client = Some(
                    self.client
                        .accept_media_channel(
                            cx,
                            ChannelScope {
                                control: self.client_control,
                                parent: parent(),
                                selection: &self.selection,
                            },
                            &offer,
                            Duration::from_secs(2),
                            || true,
                        )
                        .unwrap(),
                );
            }
            host.dispatch(&mut self.host, cx, || true).unwrap();
            if let Some(channel) = &mut client {
                channel.dispatch(&mut self.client, cx, || true).unwrap();
                let a = host.finish(&mut self.host, cx, || true).unwrap();
                let b = channel.finish(&mut self.client, cx, || true).unwrap();
                if a.is_some() && b.is_some() {
                    return (host, client.take().unwrap());
                }
            }
        }
    }
    async fn media(
        &mut self,
        cx: &Cx,
        configuration: Binding,
        recovery_id: u32,
        video_id: u32,
    ) -> Media {
        let (_, configuration_channel) = self
            .attach(cx, MediaRole::Configuration, configuration)
            .await;
        let (_, recovery) = self
            .attach(
                cx,
                MediaRole::Recovery,
                Binding {
                    parent: ControlBinding {
                        id: recovery_id,
                        ..configuration.parent
                    },
                    ..configuration
                },
            )
            .await;
        let (_, video) = self
            .attach(
                cx,
                MediaRole::Video,
                Binding {
                    parent: ControlBinding {
                        id: video_id,
                        ..configuration.parent
                    },
                    ..configuration
                },
            )
            .await;
        let owner = NegotiatedMedia::new(
            &self.client,
            &self.selection,
            &configuration_channel,
            &recovery,
            &video,
        )
        .unwrap();
        Media {
            configuration: configuration_channel,
            recovery,
            video,
            owner,
        }
    }
}
struct Media {
    configuration: MediaChannel,
    recovery: MediaChannel,
    video: MediaChannel,
    owner: NegotiatedMedia,
}
struct Lane {
    pair: Pair,
    host_configuration: MediaChannel,
    client_configuration: MediaChannel,
    host_input: MediaChannel,
    input: NegotiatedInput,
}
async fn lane(cx: &Cx, recovery: bool) -> Lane {
    let mut pair = Pair::new(cx, recovery).await;
    let (host_configuration, client_configuration) = pair
        .attach(
            cx,
            MediaRole::Configuration,
            binding(8, RecoveryGeneration::INITIAL),
        )
        .await;
    let (host_input, client_input) = pair
        .attach(
            cx,
            MediaRole::Input,
            binding(10, RecoveryGeneration::INITIAL),
        )
        .await;
    let input = NegotiatedInput::new(
        &pair.client,
        &pair.selection,
        &client_configuration,
        client_input,
    )
    .unwrap();
    Lane {
        pair,
        host_configuration,
        client_configuration,
        host_input,
        input,
    }
}
fn credentials(recovery: RecoveryGeneration) -> InputCredentials {
    let b = binding(10, recovery);
    InputCredentials {
        session: parent().remote_session,
        lease: InputLeaseId::from_raw(19),
        ticket: InputTicketId::from_raw(23),
        view: InputView {
            geometry: b.geometry,
            viewport: b.viewport,
            configuration: b.configuration,
            recovery,
        },
    }
}
fn record(recovery: RecoveryGeneration, pointer: bool) -> Vec<u8> {
    let request = InputRequest {
        credentials: credentials(recovery),
        sequence: 0,
        event: if pointer {
            InputEvent::Pointer {
                position: DesktopPoint { x: 1, y: 2 },
            }
        } else {
            InputEvent::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Press,
            }
        },
    };
    let mut bytes = [0; 128];
    let size = encode_input(
        request,
        &mut bytes,
        &LIMITS,
        10,
        InputDirection::ViewerToHost,
        if pointer {
            InputDelivery::Datagram
        } else {
            InputDelivery::Reliable
        },
    )
    .unwrap();
    bytes[..size].to_vec()
}
fn assert_sends_fenced(f: &mut Lane, cx: &Cx, recovery: RecoveryGeneration) {
    for pointer in [false, true] {
        assert_eq!(
            f.input.send(
                cx,
                &mut f.pair.client,
                &record(recovery, pointer),
                now(cx) + 1_000_000,
                || true,
            ),
            Err(Error::InvalidRoutes)
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn recovered_input_uses_the_original_stream_and_rejects_old_generation_actions() {
    run(|cx| async move {
        let mut f = lane(&cx, true).await;
        let routes = f.input.viewer_routes(&f.pair.client).unwrap();
        f.input.begin_recovery(&f.pair.client).unwrap();
        assert_sends_fenced(&mut f, &cx, RecoveryGeneration::INITIAL);
        f.client_configuration
            .retire_media(&mut f.pair.client, &cx)
            .unwrap();
        f.host_configuration
            .retire_media(&mut f.pair.host, &cx)
            .unwrap();
        assert_eq!(f.input.viewer_routes(&f.pair.client).unwrap(), routes);
        assert_eq!(
            f.input.viewer_scope(&f.pair.client),
            Err(Error::InvalidRoutes)
        );
        let next = RecoveryGeneration::INITIAL.next().unwrap();
        let mut media = f.pair.media(&cx, binding(20, next), 30, 31).await;
        f.input
            .follow_recovery(&f.pair.client, &media.owner)
            .unwrap();
        assert_eq!(f.input.channel_binding(), 10);
        assert_eq!(f.input.viewer_routes(&f.pair.client).unwrap(), routes);
        assert_eq!(
            f.input.viewer_scope(&f.pair.client).unwrap(),
            (parent(), binding(20, next))
        );
        assert_sends_fenced(&mut f, &cx, RecoveryGeneration::INITIAL);
        let until = now(&cx) + 1_000_000;
        let current = record(next, false);
        loop {
            assert!(now(&cx) < until, "recovered input admission timed out");
            match f
                .input
                .send(&cx, &mut f.pair.client, &current, until, || true)
            {
                Ok(()) => break,
                Err(Error::Transport(quic::Error::Backpressure)) => f.pair.drive(&cx).await,
                error => panic!("recovered input admission: {error:?}"),
            }
        }
        // Confirm delivery on the ORIGINAL host input route. This does not
        // claim a native operation: no input agent exists in this fixture.
        let inbound = f.host_input.completed_on(&f.pair.host).unwrap().inbound;
        let mut received = 0;
        while received == 0 {
            assert!(now(&cx) < until, "recovered input delivery timed out");
            f.pair.drive(&cx).await;
            f.pair
                .host
                .receive_ready(
                    &cx,
                    || true,
                    |r| r == Route::Stream(inbound),
                    |_, bytes| {
                        assert_eq!(bytes, current.as_slice());
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert_eq!(received, 1);
        media
            .configuration
            .retire_media(&mut f.pair.client, &cx)
            .unwrap();
        assert_eq!(f.input.viewer_routes(&f.pair.client), Err(Error::Closed));
    });
}

#[test]
fn failed_follow_keeps_the_recovery_fence_and_can_accept_the_same_generation_later() {
    run(|cx| async move {
        let mut f = lane(&cx, true).await;
        let routes = f.input.viewer_routes(&f.pair.client).unwrap();
        let next = RecoveryGeneration::INITIAL.next().unwrap();
        f.input.begin_recovery(&f.pair.client).unwrap();
        let mut media = f.pair.media(&cx, binding(20, next), 30, 31).await;
        media
            .configuration
            .retire_media(&mut f.pair.client, &cx)
            .unwrap();
        assert!(
            f.input
                .follow_recovery(&f.pair.client, &media.owner)
                .is_err()
        );
        assert_eq!(f.input.viewer_routes(&f.pair.client).unwrap(), routes);
        assert_eq!(
            f.input.viewer_scope(&f.pair.client),
            Err(Error::InvalidRoutes)
        );
        assert_sends_fenced(&mut f, &cx, next);
        let (_host, client) = f
            .pair
            .attach(&cx, MediaRole::Configuration, binding(40, next))
            .await;
        // Reuse the still-live same-generation recovery/video pair, never
        // allocate another input lane or reset the original ordering domain.
        let replacement = NegotiatedMedia::new(
            &f.pair.client,
            &f.pair.selection,
            &client,
            &media.recovery,
            &media.video,
        )
        .unwrap();
        f.input
            .follow_recovery(&f.pair.client, &replacement)
            .unwrap();
        assert_eq!(
            f.input.viewer_scope(&f.pair.client).unwrap(),
            (parent(), binding(40, next))
        );
        assert_eq!(f.input.viewer_routes(&f.pair.client).unwrap(), routes);
    });
}

#[test]
fn unnegotiated_recovery_cannot_enter_or_install_a_successor() {
    run(|cx| async move {
        let mut f = lane(&cx, false).await;
        let original = f.input.viewer_scope(&f.pair.client).unwrap();
        let next = RecoveryGeneration::INITIAL.next().unwrap();
        assert_eq!(
            f.input.begin_recovery(&f.pair.client),
            Err(Error::InvalidRoutes)
        );
        assert_eq!(f.input.viewer_scope(&f.pair.client).unwrap(), original);
        let media = f.pair.media(&cx, binding(20, next), 30, 31).await;
        assert_eq!(
            f.input.follow_recovery(&f.pair.client, &media.owner),
            Err(Error::InvalidRoutes)
        );
        assert_sends_fenced(&mut f, &cx, next);
        // Refusing unnegotiated recovery must not bypass ordinary lifetimes.
        f.client_configuration
            .retire_media(&mut f.pair.client, &cx)
            .unwrap();
        assert_eq!(f.input.viewer_routes(&f.pair.client), Err(Error::Closed));
    });
}

#[test]
fn recovery_never_allows_a_foreign_connection_or_ordinary_configuration_loss() {
    run(|cx| async move {
        let mut f = lane(&cx, true).await;
        assert!(f.input.viewer_routes(&f.pair.host).is_err());
        assert!(f.input.begin_recovery(&f.pair.host).is_err());
        // A failed foreign entry did not change this lane's lifetime checks.
        // Capability selection alone is not entry into the recovery interval.
        f.client_configuration
            .retire_media(&mut f.pair.client, &cx)
            .unwrap();
        assert_eq!(f.input.viewer_routes(&f.pair.client), Err(Error::Closed));
        f.input.begin_recovery(&f.pair.client).unwrap();
        assert!(f.input.viewer_routes(&f.pair.client).is_ok());
        assert!(f.input.viewer_routes(&f.pair.host).is_err());
        f.pair.client.close();
        assert!(f.input.viewer_routes(&f.pair.client).is_err());
    });
}

#[test]
fn recovery_retains_feedback_delivery_without_reopening_input() {
    run(|cx| async move {
        let mut f = lane(&cx, true).await;
        f.input.begin_recovery(&f.pair.client).unwrap();
        f.client_configuration
            .retire_media(&mut f.pair.client, &cx)
            .unwrap();
        f.host_configuration
            .retire_media(&mut f.pair.host, &cx)
            .unwrap();
        let issued = now(&cx);
        let until = issued + 1_000_000;
        let mut bytes = [0; fr_wire::input_ticket::INPUT_TICKET_BYTES];
        let len = fr_wire::input_ticket::encode(
            fr_wire::input_ticket::Ticket {
                credentials: credentials(RecoveryGeneration::INITIAL),
                sequence: 0,
                issued_at_us: issued,
                expires_at_us: until,
            },
            &mut bytes,
            &LIMITS,
            10,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        let feedback = f.host_input.completed_on(&f.pair.host).unwrap().outbound;
        loop {
            assert!(now(&cx) < until, "feedback admission timed out");
            match f
                .pair
                .host
                .send(&cx, Route::Stream(feedback), &bytes[..len], until, || true)
            {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => f.pair.drive(&cx).await,
                error => panic!("feedback admission: {error:?}"),
            }
        }
        let mut received = 0;
        while received == 0 {
            assert!(now(&cx) < until, "feedback delivery timed out");
            f.pair.drive(&cx).await;
            f.input
                .receive_ready(
                    &cx,
                    &mut f.pair.client,
                    || true,
                    |record| {
                        assert_eq!(record, &bytes[..len]);
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert_eq!(received, 1);
        // Delivery of feedback is NOT resumption of input or a new grant.
        assert_sends_fenced(&mut f, &cx, RecoveryGeneration::INITIAL);
        assert_eq!(
            f.input.viewer_scope(&f.pair.client),
            Err(Error::InvalidRoutes)
        );
    });
}

#[test]
fn a_foreign_media_owner_cannot_rebind_identical_numeric_routes() {
    run(|cx| async move {
        let mut local = lane(&cx, true).await;
        let mut foreign = lane(&cx, true).await;
        let next = RecoveryGeneration::INITIAL.next().unwrap();
        let a = local.pair.media(&cx, binding(20, next), 30, 31).await;
        let b = foreign.pair.media(&cx, binding(20, next), 30, 31).await;
        assert_eq!(a.owner.binding(), b.owner.binding());
        assert_eq!(
            a.owner.progress_route(&local.pair.client).unwrap(),
            b.owner.progress_route(&foreign.pair.client).unwrap()
        );
        let routes = local.input.viewer_routes(&local.pair.client).unwrap();
        local.input.begin_recovery(&local.pair.client).unwrap();
        assert_eq!(
            local.input.check_recovery(&local.pair.client, &b.owner),
            Err(Error::InvalidRoutes)
        );
        assert_eq!(
            local.input.follow_recovery(&local.pair.client, &b.owner),
            Err(Error::InvalidRoutes)
        );
        assert_eq!(
            local.input.viewer_routes(&local.pair.client).unwrap(),
            routes
        );
        assert_sends_fenced(&mut local, &cx, next);
        local
            .input
            .follow_recovery(&local.pair.client, &a.owner)
            .unwrap();
        assert_eq!(
            local.input.viewer_scope(&local.pair.client).unwrap(),
            (parent(), binding(20, next))
        );
    });
}

#[test]
fn preflight_neither_resumes_input_nor_consumes_the_successor() {
    run(|cx| async move {
        let mut f = lane(&cx, true).await;
        let next = RecoveryGeneration::INITIAL.next().unwrap();
        let media = f.pair.media(&cx, binding(20, next), 30, 31).await;
        f.input.begin_recovery(&f.pair.client).unwrap();
        for _ in 0..2 {
            f.input
                .check_recovery(&f.pair.client, &media.owner)
                .unwrap();
            assert_eq!(
                f.input.viewer_scope(&f.pair.client),
                Err(Error::InvalidRoutes)
            );
            assert_sends_fenced(&mut f, &cx, next);
        }
        f.input
            .follow_recovery(&f.pair.client, &media.owner)
            .unwrap();
        assert_eq!(
            f.input.viewer_scope(&f.pair.client).unwrap(),
            (parent(), binding(20, next))
        );
        // Installing the successor consumed this transition, not the input lane.
        assert_eq!(
            f.input.check_recovery(&f.pair.client, &media.owner),
            Err(Error::InvalidRoutes)
        );
    });
}

#[test]
fn a_live_configuration_does_not_hide_retired_replacement_video() {
    run(|cx| async move {
        let mut f = lane(&cx, true).await;
        let next = RecoveryGeneration::INITIAL.next().unwrap();
        let mut media = f.pair.media(&cx, binding(20, next), 30, 31).await;
        let routes = f.input.viewer_routes(&f.pair.client).unwrap();
        f.input.begin_recovery(&f.pair.client).unwrap();
        media.video.retire_media(&mut f.pair.client, &cx).unwrap();
        assert!(media.configuration.completed_on(&f.pair.client).is_ok());
        assert_eq!(
            f.input.check_recovery(&f.pair.client, &media.owner),
            Err(Error::InvalidRoutes)
        );
        assert_eq!(
            f.input.follow_recovery(&f.pair.client, &media.owner),
            Err(Error::InvalidRoutes)
        );
        assert_eq!(f.input.viewer_routes(&f.pair.client).unwrap(), routes);
        assert_sends_fenced(&mut f, &cx, next);
    });
}
