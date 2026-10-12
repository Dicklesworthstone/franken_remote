//! Production audio service with injected transport admission and a recording
//! output. These prove queue/epoch behavior, not QUIC, Opus or audible playback.
use super::*;
use std::{cell::Cell, collections::VecDeque};

#[derive(Default)]
struct Script {
    turns: VecDeque<Vec<Vec<u8>>>,
    services: usize,
    admitted: usize,
    fail: bool,
    revoke: Option<Rc<Cell<bool>>>,
}
struct Output {
    recording: Recording,
    script: Rc<RefCell<Script>>,
}
impl AudioOutput for Output {
    fn configure(&mut self, binding: u32, offer: AudioConfiguration) -> Result<(), OutputRefused> {
        self.recording.configure(binding, offer)
    }
    fn service(
        &mut self,
        live: &mut dyn FnMut() -> bool,
        acknowledge: &mut dyn FnMut(&[u8]) -> Result<(), OutputRefused>,
    ) -> Result<(), OutputRefused> {
        if !live() {
            return Err(OutputRefused);
        }
        let mut script = self.script.borrow_mut();
        script.services += 1;
        for record in script.turns.pop_front().unwrap_or_default() {
            // Deliberately swallow callback failure: the session must still
            // fence a malformed/duplicate reply and not admit the first one.
            let _ = acknowledge(&record);
        }
        if let Some(flag) = &script.revoke {
            flag.set(false);
        }
        if script.fail {
            Err(OutputRefused)
        } else {
            Ok(())
        }
    }
    fn configuration_admitted(
        &mut self,
        binding: u32,
        offer: AudioConfiguration,
    ) -> Result<(), OutputRefused> {
        assert_eq!(binding, BINDING);
        assert_eq!(self.recording.0.borrow().configured.last(), Some(&offer));
        self.script.borrow_mut().admitted += 1;
        Ok(())
    }
    fn receive(
        &mut self,
        bytes: &[u8],
        live: &mut dyn FnMut() -> bool,
    ) -> Result<(), OutputRefused> {
        self.recording.receive(bytes, live)
    }
    fn ended(&mut self, end: AudioEnd) {
        self.recording.ended(end);
    }
    fn reset(&mut self) {
        self.recording.reset();
    }
}
fn ack(generation: u64) -> Vec<u8> {
    let offer = offer(generation);
    let mut record = vec![0; wire::AUDIO_CONFIGURED_RECORD_BYTES];
    wire::encode_configured(
        &wire::AudioConfigured {
            direction: offer.direction,
            generation: offer.generation,
            accepted: true,
            actual_channels: offer.channels,
            actual_sample_rate: offer.sample_rate,
            actual_frame_duration_ms: offer.frame_duration_ms,
        },
        BINDING,
        &mut record,
    )
    .unwrap();
    record
}
fn configured() -> (ViewerAudio, Rc<RefCell<Calls>>, Rc<RefCell<Script>>) {
    let (mut audio, calls) = owner();
    let script = Rc::new(RefCell::new(Script {
        turns: VecDeque::from([vec![ack(1)]]),
        ..Script::default()
    }));
    audio.output = Some(Box::new(Output {
        recording: Recording(calls.clone()),
        script: script.clone(),
    }));
    calls.borrow_mut().dropped = 0; // Ignore replacement of the unused fixture output.
    audio.receive(control(), &configuration(1), &mut live());
    (audio, calls, script)
}
fn turn(audio: &mut ViewerAudio, now: u64, admit: bool) -> Vec<(Vec<u8>, u64)> {
    let mut attempts = Vec::new();
    audio
        .service_reply(now, &mut live(), &mut |route, bytes, until, permitted| {
            assert_eq!(route, Route::Stream(lanes().replies));
            assert!(permitted());
            attempts.push((bytes.to_vec(), until));
            if admit {
                Ok(())
            } else {
                Err(quic::Error::Backpressure)
            }
        })
        .unwrap();
    attempts
}

#[test]
fn congestion_keeps_one_exact_reply_without_restarting_the_decoder() {
    let (mut audio, calls, script) = configured();
    let expected = vec![(ack(1), 100 + REPLY_SEND_US)];
    assert_eq!(turn(&mut audio, 100, false), expected);
    for now in [101, 50_000, 1_000_000] {
        assert_eq!(turn(&mut audio, now, false), expected);
        assert_eq!(audio.state, State::Configuring(offer(1)));
        audio.receive(datagram(), &packet(1, now, 40), &mut live());
    }
    assert_eq!(calls.borrow().received, [] as [std::vec::Vec<u8>; 0]);
    assert_eq!(calls.borrow().configured, [offer(1)]);
    assert_eq!(calls.borrow().resets, 0);
    assert_eq!(audio.statistics().restarts, 0);
    assert_eq!(script.borrow().admitted, 0);
    assert_eq!(turn(&mut audio, 1_500_000, true), expected);
    assert_eq!(audio.state, State::Active(offer(1)));
    assert!(audio.pending_reply.is_none());
    audio.receive(datagram(), &packet(1, 2_000_000, 40), &mut live());
    assert_eq!(calls.borrow().received.len(), 1);
    assert_eq!(
        turn(&mut audio, 1_500_001, true),
        [] as [(std::vec::Vec<u8>, u64); 0]
    );
    assert_eq!(script.borrow().services, 6);
    assert_eq!(script.borrow().admitted, 1);
}

#[test]
fn pending_ack_expiry_is_audio_only_and_cannot_reopen_or_send_late() {
    for late in [0, 1] {
        let (mut audio, calls, _) = configured();
        turn(&mut audio, 10, false);
        assert_eq!(
            turn(&mut audio, 10 + REPLY_SEND_US + late, true),
            [] as [(std::vec::Vec<u8>, u64); 0]
        );
        assert_eq!(audio.state, State::Ended);
        assert!(audio.pending_reply.is_none() && audio.stop.is_none());
        assert_eq!(calls.borrow().ended, [AudioEnd::Local]);
        assert_eq!(calls.borrow().dropped, 1);
        assert_eq!(audio.statistics().restarts, 0);
        audio.receive(control(), &configuration(2), &mut live());
        assert_eq!(calls.borrow().configured.len(), 1);
        assert_eq!(
            turn(&mut audio, 20 + REPLY_SEND_US, true),
            [] as [(std::vec::Vec<u8>, u64); 0]
        );
    }
}

#[test]
fn stop_congestion_does_not_refresh_its_first_send_deadline() {
    let (mut audio, _, _) = configured();
    audio.fail_locally(AudioGeneration::from_raw(1));
    let expected = vec![(stop(1, AudioStopReason::DeviceChanged), 40 + REPLY_SEND_US)];
    assert_eq!(turn(&mut audio, 40, false), expected);
    assert_eq!(turn(&mut audio, 200, false), expected);
    assert_eq!(
        turn(&mut audio, 40 + REPLY_SEND_US, true),
        [] as [(std::vec::Vec<u8>, u64); 0]
    );
    assert_eq!(audio.state, State::Ended);
    assert!(audio.stop.is_none() && audio.pending_reply.is_none());
    assert_eq!(audio.statistics().restarts, 1);
}

#[test]
fn host_stop_discards_the_unsent_ack_before_any_transport_admission() {
    let (mut audio, calls, _) = configured();
    turn(&mut audio, 1, false);
    audio.receive(
        control(),
        &stop(1, AudioStopReason::HostDisabled),
        &mut live(),
    );
    assert!(audio.pending_reply.is_none());
    assert_eq!(
        turn(&mut audio, 2, true),
        [] as [(std::vec::Vec<u8>, u64); 0]
    );
    assert_eq!(audio.state, State::Ended);
    assert_eq!(
        calls.borrow().ended,
        [AudioEnd::Host(AudioStopReason::HostDisabled)]
    );
}

#[test]
fn output_failure_retires_pending_ack_and_only_new_epoch_can_play() {
    let (mut audio, calls, script) = configured();
    turn(&mut audio, 1, false);
    script.borrow_mut().fail = true;
    assert_eq!(
        turn(&mut audio, 2, true),
        [] as [(std::vec::Vec<u8>, u64); 0]
    );
    assert_eq!(audio.state, State::Idle);
    assert!(audio.pending_reply.is_none());
    assert_eq!(calls.borrow().resets, 1);
    script.borrow_mut().fail = false;
    assert_eq!(
        turn(&mut audio, 3, true)[0].0,
        stop(1, AudioStopReason::DeviceChanged)
    );
    audio.receive(control(), &configuration(2), &mut live());
    script.borrow_mut().turns.push_back(vec![ack(2)]);
    assert_eq!(turn(&mut audio, 4, true)[0].0, ack(2));
    assert_eq!(audio.state, State::Active(offer(2)));
    audio.receive(datagram(), &packet(1, 0, 40), &mut live());
    assert_eq!(calls.borrow().received, [] as [std::vec::Vec<u8>; 0]);
    audio.receive(datagram(), &packet(2, 0, 40), &mut live());
    assert_eq!(calls.borrow().received.len(), 1);
}

#[test]
fn duplicate_native_callbacks_cannot_be_hidden_by_a_successful_return() {
    for same_turn in [true, false] {
        let (mut audio, calls, script) = configured();
        if same_turn {
            script.borrow_mut().turns[0].push(ack(1));
        } else {
            turn(&mut audio, 1, false);
            script.borrow_mut().turns.push_back(vec![ack(1)]);
        }
        assert_eq!(
            turn(&mut audio, 2, true),
            [] as [(std::vec::Vec<u8>, u64); 0]
        );
        assert_eq!(audio.state, State::Idle);
        assert!(audio.pending_reply.is_none());
        assert_eq!(calls.borrow().resets, 1);
    }
}

#[test]
fn malformed_and_foreign_native_replies_never_reach_the_send_boundary() {
    for record in [vec![], ack(2), ack(3), ack(1)[..10].to_vec()] {
        let (mut audio, calls, script) = configured();
        script.borrow_mut().turns = VecDeque::from([vec![record]]);
        assert_eq!(
            turn(&mut audio, 1, true),
            [] as [(std::vec::Vec<u8>, u64); 0]
        );
        assert!(audio.pending_reply.is_none());
        assert_eq!(audio.state, State::Idle);
        assert_eq!(calls.borrow().resets, 1);
    }
}

#[test]
fn a_native_failure_after_custody_never_admits_the_prepared_reply() {
    let (mut audio, _, script) = configured();
    script.borrow_mut().fail = true;
    assert_eq!(
        turn(&mut audio, 1, true),
        [] as [(std::vec::Vec<u8>, u64); 0]
    );
    assert!(audio.pending_reply.is_none());
    assert_eq!(audio.state, State::Idle);
}

#[test]
fn revocation_before_service_or_after_callback_fences_the_original_output() {
    for after_callback in [false, true] {
        let (mut audio, calls, script) = configured();
        let flag = Rc::new(Cell::new(after_callback));
        if after_callback {
            script.borrow_mut().revoke = Some(flag.clone());
        }
        let error = audio.service_reply(1, &mut || flag.get(), &mut |_, _, _, _| {
            panic!("revoked reply reached transport")
        });
        assert_eq!(error, Err(Error::Transport(quic::Error::Unauthorized)));
        assert!(audio.pending_reply.is_none() && audio.output.is_none());
        assert_eq!(script.borrow().services, usize::from(after_callback));
        assert_eq!(calls.borrow().dropped, 1);
    }
}

#[test]
fn permanent_transport_refusal_is_not_a_decoder_restart() {
    let (mut audio, calls, _) = configured();
    let error = audio.service_reply(1, &mut live(), &mut |_, _, _, _| {
        Err(quic::Error::Unauthorized)
    });
    assert_eq!(error, Err(Error::Transport(quic::Error::Unauthorized)));
    assert_eq!(audio.state, State::Ended);
    assert!(audio.pending_reply.is_none());
    assert_eq!(calls.borrow().resets, 0);
}

#[test]
fn reply_deadline_overflow_and_pending_clock_regression_are_terminal() {
    for regression in [false, true] {
        let (mut audio, calls, _) = configured();
        if regression {
            turn(&mut audio, 100, false);
        }
        let at = if regression { 99 } else { u64::MAX - 1 };
        assert_eq!(
            turn(&mut audio, at, true),
            [] as [(std::vec::Vec<u8>, u64); 0]
        );
        assert_eq!(audio.state, State::Ended);
        assert!(audio.pending_reply.is_none() && audio.stop.is_none());
        assert_eq!(calls.borrow().dropped, 1);
        assert_eq!(calls.borrow().resets, 0);
    }
}
