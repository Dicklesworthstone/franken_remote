use super::*;
use asupersync::net::quic_native::StreamId;
use fr_core::ids::*;
use fr_media::delivery::{MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePolicy};
use fr_transport::quic::{Messages, Priority, StreamRoute};
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation,
    encode_progress,
    input::{InputDelivery, InputDirection},
    negotiation::{Capability, Role},
};
fn setup() -> Setup {
    Setup {
        binding: Binding {
            parent: ControlBinding {
                id: 1,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            display: 4,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        },
        limits: ProtocolLimits::ABSOLUTE,
    }
}
fn route() -> Route {
    Route::Stream(StreamRoute {
        stream: StreamId(2),
        binding: 1,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        outbound: true,
        maximum: 65_536,
    })
}
fn receiver() -> ReceivePipeline {
    let cfg = ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    };
    let mut r =
        ReceivePipeline::new(cfg, MediaBudget::new(cfg.limits.protocol()).unwrap()).unwrap();
    r.decoder_configured(0).unwrap();
    let mut bytes = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame: 0,
                total_bytes: 4,
                stride: 4,
                capture_micros: 0,
                reference: None,
            },
            observed_micros: 0,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        3,
        &cfg.limits,
        &mut bytes,
    )
    .unwrap();
    r.receive(Channel::MediaConfig, &bytes[..n], 0).unwrap();
    r
}
fn query(cfg: Setup, seq: u64) -> Vec<u8> {
    let mut b = vec![0; wire::QUERY_BYTES];
    wire::encode(
        wire::Message::Query { sequence: seq },
        cfg.binding,
        &cfg.limits,
        &mut b,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    b
}
fn pending(f: &mut ViewerFeedback, now: u64) -> (Vec<u8>, u64) {
    let (b, until) = f.responder.pending(now).unwrap().unwrap();
    (b.to_vec(), until)
}
fn load(f: &mut ViewerFeedback, now: u64) -> Load {
    let (b, _) = pending(f, now);
    let wire::Message::Reply { load, .. } = wire::decode(
        &b,
        setup().binding,
        &setup().limits,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap() else {
        panic!("reply")
    };
    load
}
#[test]
fn backpressured_metrics_keep_exact_sample_sequence_and_deadline() {
    let r = receiver();
    let cfg = setup();
    let mut f = ViewerFeedback::new(cfg, route(), route()).unwrap();
    f.begin(0).unwrap();
    f.receive(route(), &query(cfg, 1), &r, 10_000).unwrap();
    let original = pending(&mut f, 10_000);
    for (seq, t) in [(2, 20_000), (3, 50_000), (4, 109_999)] {
        f.receive(route(), &query(cfg, seq), &r, t).unwrap();
        assert_eq!(pending(&mut f, t), original);
    }
    assert_eq!(load(&mut f, 109_999).work_us, Some(10_000));
    assert_eq!(original.1, 110_000);
    assert!(f.responder.pending(110_000).unwrap().is_none());
    f.receive(route(), &query(cfg, 4), &r, 110_001).unwrap();
    assert!(
        f.responder.pending(110_001).unwrap().is_none(),
        "retired query must not be resampled"
    );
}
#[test]
fn new_native_work_cannot_hide_recent_slow_completion_as_zero_cost() {
    let r = receiver();
    let mut f = ViewerFeedback::new(setup(), route(), route()).unwrap();
    f.begin(0).unwrap();
    f.complete(100_000).unwrap();
    f.begin(100_000).unwrap();
    let l = f.sample(&r, 100_001).unwrap();
    assert_eq!(l.work_us, Some(100_000));
    assert!(l.decoding);
    f.complete(110_000).unwrap();
    assert_eq!(f.sample(&r, 400_000).unwrap().work_us, None);
}
#[test]
fn only_selected_version_and_original_view_session_can_enable_feedback() {
    let b = setup().binding;
    let mut selected = Selection {
        version: 0,
        profile: 0,
        profile_version: 1,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    };
    assert!(Setup::selected(&selected, b.parent, b).unwrap().is_none());
    selected.capabilities.push(Capability {
        name: wire::CAPABILITY.into(),
        version: 1,
        required: false,
    });
    assert!(Setup::selected(&selected, b.parent, b).unwrap().is_some());
    let mut foreign = b;
    foreign.parent.remote_session = RemoteSessionId::from_raw(77);
    assert!(Setup::selected(&selected, b.parent, foreign).is_err());
    selected.capabilities[0].version = 2;
    assert!(Setup::selected(&selected, b.parent, b).is_err());
}
#[test]
fn feedback_is_advisory_on_only_the_original_route_and_full_view() {
    let r = receiver();
    let cfg = setup();
    let mut f = ViewerFeedback::new(cfg, route(), route()).unwrap();
    let mut host = HostFeedback::new(cfg, route(), route()).unwrap();
    host.requester.prepare(0).unwrap();
    let b = host.requester.pending(0).unwrap().unwrap().0.to_vec();
    host.requester.queued(0).unwrap();
    f.receive(route(), &b, &r, 0).unwrap();
    let response = pending(&mut f, 0).0;
    host.receive(route(), &response, 0).unwrap();
    assert_eq!(host.accepted, 1);
    host.receive(route(), &response, 1).unwrap();
    assert_eq!(host.accepted, 1);
    let mut foreign = cfg;
    foreign.binding.viewport = foreign.binding.viewport.next().unwrap();
    assert!(
        HostFeedback::new(foreign, route(), route())
            .unwrap()
            .receive(route(), &response, 2)
            .is_err()
    );
    let Route::Stream(mut wrong) = route() else {
        unreachable!()
    };
    wrong.binding = 2;
    assert!(host.receive(Route::Stream(wrong), &response, 2).is_err());
}
pub(crate) fn actual_backpressure(
    q: &mut QuicRecords,
    route: Route,
    c: &Cx,
    parent: ControlBinding,
) {
    let mut cfg = setup();
    cfg.binding.parent = parent;
    let mut f = ViewerFeedback::new(cfg, route, route).unwrap();
    let r = receiver();
    let current = c.timer_driver().unwrap().now().as_nanos() / 1000;
    f.begin(current).unwrap();
    f.receive(route, &query(cfg, 1), &r, current + 1).unwrap();
    let original = pending(&mut f, current + 1);
    let mut filled = 0;
    loop {
        match q.send(c, route, &original.0, current + 500_000, || true) {
            Ok(()) => filled += 1,
            Err(quic::Error::Backpressure) => break,
            Err(error) => panic!("enqueue {error:?}"),
        }
        assert!(filled <= 16);
    }
    assert!(filled > 0);
    f.service(q, c, current + 2).unwrap();
    assert_eq!(f.sent, 0);
    assert_eq!(pending(&mut f, current + 2), original);
    assert!(f.responder.pending(original.1).unwrap().is_none());
}
