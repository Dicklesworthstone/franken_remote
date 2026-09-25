//! Client-only fixture: no fabricated host observation/control is needed.
use super::*;
pub(crate) struct Client {
    c: Cx,
    pub viewer: ViewerSession,
    _selected: SelectedDisplay,
    media: NegotiatedMedia,
    receiver: ReceivePipeline,
    reply: StreamRoute,
    configuration: bool,
    configured: bool,
    first: Option<u64>,
    acknowledged: bool,
    pub frames: Vec<u64>,
}
impl Client {
    pub async fn start(c: Cx, viewer: Viewer) -> Box<Self> {
        Box::pin(Self::start_when(c, viewer, || true)).await
    }
    // Delay the display request without starting/restarting its independent
    // deadline. Keep the original observation session renewing during setup.
    pub async fn start_when(c: Cx, mut viewer: Viewer, ready: impl Fn() -> bool) -> Box<Self> {
        while !viewer.is_complete() {
            viewer.drive(Duration::from_millis(1)).await.unwrap();
        }
        let mut viewer = viewer.finish().unwrap();
        while !ready() {
            viewer.drive(Duration::from_millis(1), block).await.unwrap();
        }
        let selection = viewer.metadata().selection.clone();
        assert_eq!(selection.role, Role::Observe);
        let selected = select(&mut viewer).await;
        let cfg = channel(&mut viewer, &c, MediaRole::Configuration).await;
        let recovery = channel(&mut viewer, &c, MediaRole::Recovery).await;
        let video = channel(&mut viewer, &c, MediaRole::Video).await;
        let q = viewer.io().unwrap().0;
        let media = NegotiatedMedia::new(q, &selection, &cfg, &recovery, &video).unwrap();
        let reply = cfg.completed_on(q).unwrap().outbound;
        let config = media.receiver_config(q, ReceivePolicy::default()).unwrap();
        let receiver =
            ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap())
                .unwrap();
        Box::new(Self {
            c,
            viewer,
            _selected: selected,
            media,
            receiver,
            reply,
            configuration: false,
            configured: false,
            first: None,
            acknowledged: false,
            frames: vec![],
        })
    }
    pub async fn ready(&mut self) {
        let until = now(&self.c).unwrap() + 1_500_000;
        while !self.acknowledged {
            assert!(now(&self.c).unwrap() < until);
            self.turn().await;
        }
        for _ in 0..3 {
            self.turn().await;
        }
    }
    pub(crate) async fn turn(&mut self) {
        let message = if self.configuration && !self.configured {
            Some(decoder::Message::Configured)
        } else if !self.acknowledged {
            self.first.map(|frame| decoder::Message::FirstDecoded {
                frame,
                decoder_micros: now(&self.c).unwrap(),
            })
        } else {
            None
        };
        if let Some(message) = message {
            let mut bytes = [0; 512];
            let n = decoder::encode(
                message,
                self.media.binding(),
                self.media.limits().protocol(),
                &mut bytes,
                InputDirection::ViewerToHost,
                InputDelivery::Reliable,
            )
            .unwrap();
            match self.viewer.io().unwrap().0.send(
                &self.c,
                Route::Stream(self.reply),
                &bytes[..n],
                now(&self.c).unwrap() + 500_000,
                || true,
            ) {
                Ok(()) => {
                    if message == decoder::Message::Configured {
                        self.configured = true;
                        self.receiver
                            .decoder_configured(now(&self.c).unwrap())
                            .unwrap();
                    } else {
                        self.acknowledged = true;
                    }
                }
                Err(quic::Error::Backpressure) => {}
                other => panic!("decoder receipt: {other:?}"),
            }
        }
        self.viewer
            .drive(Duration::from_millis(1), |route, bytes| {
                if matches!(route, Route::Stream(r) if r.messages == Messages::MediaConfiguration) {
                    assert!(!self.configuration);
                    assert!(matches!(
                        decoder::decode(
                            bytes,
                            self.media.binding(),
                            self.media.limits().protocol(),
                            InputDirection::HostToViewer,
                            InputDelivery::Reliable
                        )
                        .unwrap(),
                        decoder::Message::Configuration(_)
                    ));
                    self.configuration = true;
                    Ok(Disposition::Consumed)
                } else {
                    Ok(Disposition::Blocked)
                }
            })
            .await
            .unwrap();
        self.media
            .receive_ready(
                &self.c,
                self.viewer.io().unwrap().0,
                || true,
                |channel, bytes| {
                    self.receiver
                        .receive(channel, bytes, now(&self.c).unwrap())
                        .unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        while let Some(picture) = self.receiver.take_decodable(now(&self.c).unwrap()).unwrap() {
            let frame = self
                .receiver
                .complete_decode(&picture, now(&self.c).unwrap())
                .unwrap()
                .descriptor()
                .frame;
            self.first.get_or_insert(frame);
            self.frames.push(frame);
        }
        asupersync::runtime::yield_now().await;
    }
}
