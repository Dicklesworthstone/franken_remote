//! Real original startup/input/clock owners and role-8 UDP/TLS attachment.
//! The native source is an explicit scheduling fixture. These cases do not
//! claim physical recording, a host virtual device or visible production UI.
use super::super::microphone::{
    AttachError, Error as MicError, NativeMicrophone, Retirement, State as MicState, Status,
};
use super::*;
use fr_client::audio::uplink::{self, Sender};
use fr_core::audio::{AudioChannels, MicPermission, MicTalkMode};
use fr_transport::quic::{ChannelRequest, QuicRecords, audio::AudioUpChannel};
use fr_wire::attachment::Ticket;
use std::{path::Path, sync::atomic::AtomicUsize};

#[derive(Default)]
struct Calls {
    services: AtomicUsize,
    stopped: AtomicBool,
}
struct Native {
    lane: AudioUpChannel,
    sender: Sender,
    cx: Cx,
    gate: Arc<AtomicBool>,
    calls: Arc<Calls>,
    retired: bool,
}
impl NativeMicrophone for Native {
    fn operation_check(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let gate = self.gate.clone();
        Arc::new(move || gate.load(Ordering::Acquire))
    }
    fn owns_inbound(&self, route: Route) -> bool {
        self.lane.owns_inbound(route)
    }
    fn permits_io(&self) -> bool {
        self.gate.load(Ordering::Acquire)
    }
    fn maintain(&mut self, q: &mut QuicRecords) -> Result<(), MicError> {
        if self.retired {
            if !self.lane.is_retired(q).map_err(MicError::Transport)? {
                return Err(MicError::WrongBinding);
            }
            Ok(())
        } else {
            self.lane.check(q).map_err(MicError::Transport)
        }
    }
    fn service(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut dyn FnMut() -> bool,
    ) -> Result<(), MicError> {
        self.calls.services.fetch_add(1, Ordering::Relaxed);
        q.tick(&self.cx, authorize).map_err(MicError::Transport)
    }
    fn set_permission(&mut self, permission: MicPermission) -> Result<(), MicError> {
        self.sender.set_permission(permission);
        Ok(())
    }
    fn begin_talk(&mut self, mode: MicTalkMode) -> Result<(), MicError> {
        self.sender
            .begin_talk(
                mode,
                ClientInstant(now(&self.cx).map_err(|_| MicError::Clock)?),
            )
            .map(|_| ())
            .map_err(MicError::Consent)
    }
    fn mute(&mut self) -> Result<(), MicError> {
        self.sender.mute();
        Ok(())
    }
    fn stop(&mut self) {
        self.sender.stop();
        self.calls.stopped.store(true, Ordering::Release);
    }
    fn retire(&mut self, q: &mut QuicRecords) -> Result<(), MicError> {
        self.stop();
        // This fixture admits no audio bytes. Its exact completed lane can
        // retire without releasing any queued/protected capture custody.
        self.gate.store(true, Ordering::Release);
        if !self.retired && !q.is_closed() {
            self.lane.retire(q, &self.cx).map_err(MicError::Transport)?;
        }
        self.retired = true;
        Ok(())
    }
    fn status(&self) -> Status {
        Status {
            state: match self.sender.status() {
                uplink::Status::Muted => MicState::Muted,
                uplink::Status::Configuring | uplink::Status::AwaitingHost => {
                    MicState::AwaitingHost
                }
                uplink::Status::Capturing => MicState::Talking,
                uplink::Status::Stopped => MicState::Stopped,
            },
            permission: self.sender.permission(),
            reason: None,
            retirement: Retirement::NotStarted,
        }
    }
}
fn launch() -> crate::microphone_process::Launch {
    crate::microphone_process::Launch::new(
        Path::new("/nonexistent/fr-microphone-worker"),
        Path::new("/nonexistent/fr-opus-worker"),
        Path::new("/nonexistent/private-pipewire"),
    )
    .unwrap()
}
async fn setup(c: &Cx, h: &Cx, combined: bool) -> Fixture {
    Box::pin(fixture_with_clipboard(
        c,
        h,
        caps(),
        false,
        false,
        true,
        if combined {
            ClipboardMode::MicrophoneCombined
        } else {
            ClipboardMode::Microphone
        },
    ))
    .await
}
fn expect(state: &mut Fixture, timeout: Duration) -> (Arc<AtomicBool>, Arc<Calls>) {
    let gate = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(Calls::default());
    let retained = (gate.clone(), calls.clone());
    let original = state.viewer.input.binding();
    state
        .viewer
        .expect_microphone(timeout, move |q, lane, input, selection, cx| {
            assert_eq!(
                input.binding(),
                original,
                "factory replaced original controller"
            );
            if let Err(error) = lane.check(q) {
                return Err(AttachError::new(MicError::Transport(error), lane));
            }
            let profile = uplink::Profile {
                channels: AudioChannels::Mono,
                frame_duration_ms: 20,
                max_packet_bytes: 1000,
                max_record_bytes: fr_wire::audio::AUDIO_PACKET_OVERHEAD + 1000,
                jitter_target_ms: 20,
            };
            let sender = match input.attach_microphone(
                lane.parent(),
                lane.channel(),
                selection,
                true,
                profile,
                ClientInstant(now(cx).unwrap()),
            ) {
                Ok(sender) => sender,
                Err(error) => return Err(AttachError::new(MicError::Consent(error), lane)),
            };
            assert_eq!(sender.lease(), original.lease);
            Ok(Box::new(Native {
                lane,
                sender,
                cx: cx.clone(),
                gate,
                calls,
                retired: false,
            }))
        })
        .unwrap();
    retained
}
async fn join(state: &mut Fixture, c: &Cx, h: &Cx) -> (Arc<AtomicBool>, Arc<Calls>) {
    let effects = expect(state, Duration::from_secs(1));
    let mut binding = state.viewer.media.as_ref().unwrap().binding();
    binding.parent.id = 73;
    state
        .host
        .offer_microphone(
            ChannelRequest {
                binding,
                ticket: Ticket(73_000),
                timeout: Duration::from_secs(1),
            },
            true,
            launch(),
        )
        .unwrap();
    let mut n = 80_000;
    let mut t = 90_000;
    let until = now(c).unwrap() + 500_000;
    loop {
        assert!(
            now(c).unwrap() < until,
            "role-8 did not complete on ordinary drives"
        );
        // Host clock is already session-owned; no raw transport loan is
        // taken after its microphone has claimed the original connection.
        let (host, viewer) = Box::pin(support::both(
            state.host.drive(
                Duration::from_millis(1),
                || nonce(&mut n),
                || {
                    t += 1;
                    Some(InputTicketId::from_raw(t))
                },
                block,
            ),
            state.viewer.drive(Duration::from_millis(1), |_| {}, block),
        ))
        .await;
        host.unwrap();
        viewer.unwrap();
        if state.viewer.microphone_status().unwrap().state == MicState::Muted
            && !state.host.microphone_negotiating()
            && !state.viewer.clipboard_negotiating()
            && !state.host.clipboard_negotiating()
            && !state.viewer.file_send_negotiating()
            && !state.host.file_receive_negotiating()
        {
            break;
        }
    }
    assert!(
        state.host.microphone_retirement().is_none(),
        "attachment started a native host source"
    );
    assert!(!h.is_cancel_requested());
    effects
}
#[test]
fn original_running_pair_keeps_microphone_muted_until_local_permission_and_talk() {
    run(|c, h| async move {
        let mut state = setup(&c, &h, false).await;
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (_, calls) = join(&mut state, &c, &h).await;
                assert!(calls.services.load(Ordering::Relaxed) > 0);
                let initial = state.viewer.microphone_status().unwrap();
                assert_eq!(initial.permission, MicPermission::NotRequested);
                assert_eq!(initial.retirement, Retirement::NotStarted);
                assert_eq!(
                    state
                        .viewer
                        .begin_microphone_talk(MicTalkMode::OpenMic { active: true }),
                    Err(MicError::Consent(uplink::Error::Permission))
                );
                state
                    .viewer
                    .set_microphone_permission(MicPermission::Granted)
                    .unwrap();
                assert_eq!(
                    state.viewer.microphone_status().unwrap().state,
                    MicState::Muted
                );
                state
                    .viewer
                    .begin_microphone_talk(MicTalkMode::OpenMic { active: true })
                    .unwrap();
                assert_eq!(
                    state.viewer.microphone_status().unwrap().state,
                    MicState::AwaitingHost
                );
                state.viewer.mute_microphone().unwrap();
                assert_eq!(
                    state.viewer.microphone_status().unwrap().state,
                    MicState::Muted
                );
                state.viewer.retire_microphone().unwrap();
                state.host.retire_microphone().unwrap();
                assert!(calls.stopped.load(Ordering::Acquire));
                assert_eq!(
                    state.viewer.microphone_status().unwrap().state,
                    MicState::Stopped
                );
                assert_eq!(
                    state
                        .viewer
                        .expect_microphone(Duration::from_secs(1), |_, _, _, _, _| {
                            panic!("reused original microphone scope")
                        }),
                    Err(MicError::AlreadyAttached)
                );
                let _ = state.viewer.action(key(true)).unwrap();
                let mut n = 100_000;
                let mut t = 110_000;
                let until = now(&c).unwrap() + 500_000;
                while state.viewer.pending_actions() != 0 {
                    assert!(now(&c).unwrap() < until);
                    let (host, viewer) = turn(&mut state, &c, &h, &mut n, &mut t).await;
                    host.unwrap();
                    viewer.unwrap();
                }
                assert_eq!(state.effects.lock().unwrap().keys, [true]);
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}
#[test]
fn clipboard_files_and_microphone_exchanges_share_the_original_parent_without_cross_consumption() {
    run(|c, h| async move {
        let mut state = setup(&c, &h, true).await;
        let destination = super::files::Disk::new();
        state
            .viewer
            .expect_clipboard(Duration::from_secs(1), true)
            .unwrap();
        state
            .viewer
            .expect_files(
                991,
                fr_files::session::Permission::new(true),
                fr_files::sender::Policy::default(),
                Duration::from_secs(1),
            )
            .unwrap();
        let binding = state.viewer.media.as_ref().unwrap().binding();
        let request = |id| ChannelRequest {
            binding: decoder::Binding {
                parent: fr_wire::negotiation::ControlBinding {
                    id,
                    ..binding.parent
                },
                ..binding
            },
            ticket: Ticket(u128::from(id) + 80_000),
            timeout: Duration::from_secs(1),
        };
        // Queue both sibling offers first: the microphone acceptor must leave
        // their exact descriptors for their original owners. Their acceptors
        // must likewise leave the later role-8 offer and ticket untouched.
        state
            .host
            .offer_files(request(12), 991, destination.config())
            .unwrap();
        state.host.offer_clipboard(request(14), true).unwrap();
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                join(&mut state, &c, &h).await;
                assert!(state.viewer.take_clipboard_worker().is_some());
                assert!(state.host.take_clipboard_worker().is_some());
                assert_eq!(
                    state.viewer.file_stage(),
                    Some(fr_files::sender::Stage::Idle)
                );
                assert!(std::fs::read_dir(&destination.0).unwrap().next().is_none());
                assert!(!state.viewer.is_closed());
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}
#[test]
fn expectation_deadline_fences_even_a_bare_transport_loan_before_the_host_offer() {
    run(|c, h| async move {
        let mut state = setup(&c, &h, false).await;
        expect(&mut state, Duration::from_millis(1));
        asupersync::time::sleep(h.now(), Duration::from_millis(5)).await;
        assert_eq!(
            state.viewer.session.transport.tick(&c, || true),
            Err(quic::Error::Unauthorized),
        );
        assert!(state.viewer.session.transport.is_closed());
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn retained_native_guard_and_unpolled_drive_fence_the_original_running_owner() {
    for bypass in [false, true] {
        run(move |c, h| async move {
            let mut state = setup(&c, &h, false).await;
            let driver = state.driver.take().unwrap();
            let ((), done) = Box::pin(support::both(
                async {
                    let (gate, calls) = join(&mut state, &c, &h).await;
                    if bypass {
                        gate.store(false, Ordering::Release);
                        assert_eq!(
                            state.viewer.session.transport.tick(&c, || true),
                            Err(quic::Error::Unauthorized),
                        );
                        assert!(state.viewer.pointer(DesktopPoint { x: 2, y: 2 }).is_err());
                    } else {
                        drop(state.viewer.drive(Duration::ZERO, |_| {}, block));
                    }
                    assert!(state.viewer.is_closed());
                    assert!(calls.stopped.load(Ordering::Acquire));
                    assert_eq!(
                        state.viewer.microphone_status().unwrap().state,
                        MicState::Stopped
                    );
                    state.host.close();
                },
                driver,
            ))
            .await;
            assert!(done.handoff_safe());
        });
    }
}
#[test]
fn microphone_factory_is_never_called_without_positive_selection() {
    run(|c, h| async move {
        let mut state = fixture(&c, &h).await;
        assert_eq!(
            state
                .viewer
                .expect_microphone(Duration::from_secs(1), |_, _, _, _, _| {
                    panic!("unselected microphone factory called")
                }),
            Err(MicError::NotSelected)
        );
        assert!(state.viewer.microphone_status().is_none());
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
