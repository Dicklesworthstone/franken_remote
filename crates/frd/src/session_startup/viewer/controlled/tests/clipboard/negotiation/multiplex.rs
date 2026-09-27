//! One controlling connection, actual QUIC/ATP/disk workers and both clipboard
//! directions. Initial desktop evidence, input effects and OS clipboard contents
//! use the existing explicit fixtures. No manual attachment pump or wider limits.
use super::super::super::files::Disk;
use super::*;
use fr_files::{receive::Publication as FilePublication, sender::Outcome, session::Permission};
use std::fs;

// Keep the original owners and the full transfer/retirement sequence visible.
#[allow(clippy::too_many_lines)]
fn exercise(files_first: bool, consent: bool) {
    run(move |c, h| async move {
        let mut state = Box::pin(fixture_with_clipboard(
            &c,
            &h,
            caps(),
            false,
            false,
            false,
            ClipboardMode::Combined,
        ))
        .await;
        let dest = Disk::new();
        let source = Disk::new();
        let input_lease = state.viewer.input.binding().lease;
        let mut clip_request = request(&state);
        clip_request.binding.parent.id = if files_first { 14 } else { 12 };
        let files_request = ChannelRequest {
            binding: decoder::Binding {
                parent: fr_wire::negotiation::ControlBinding {
                    id: if files_first { 12 } else { 14 },
                    ..clip_request.binding.parent
                },
                ..clip_request.binding
            },
            ticket: attachment::Ticket(8_014),
            timeout: Duration::from_secs(2),
        };
        // Reverse local registration and host-offer order. Both acceptors must
        // leave the sibling's records in the one original control parser.
        if !files_first {
            state
                .viewer
                .expect_clipboard(Duration::from_secs(2), consent)
                .unwrap();
        }
        state
            .viewer
            .expect_file_drop(
                Permission::new(true),
                fr_files::sender::Policy::default(),
                Duration::from_secs(2),
            )
            .unwrap();
        if files_first {
            state
                .viewer
                .expect_clipboard(Duration::from_secs(2), consent)
                .unwrap();
        }
        if files_first {
            state
                .host
                .offer_file_drop(files_request, dest.config())
                .unwrap();
        }
        state.host.offer_clipboard(clip_request, true).unwrap();
        if !files_first {
            state
                .host
                .offer_file_drop(files_request, dest.config())
                .unwrap();
        }
        assert!(state.viewer.file_send_negotiating() && state.viewer.clipboard_negotiating());
        assert!(state.host.file_receive_negotiating() && state.host.clipboard_negotiating());
        assert!(state.host.take_clipboard_worker().is_none());
        assert!(state.viewer.take_clipboard_worker().is_none());
        assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(async {
            let (mut n, mut t) = (80_000, 90_000);
            let until = now(&c).unwrap() + 2_000_000;
            while state.viewer.file_send_negotiating() || state.host.file_receive_negotiating()
                || state.viewer.clipboard_negotiating() || state.host.clipboard_negotiating() {
                assert!(now(&c).unwrap() < until, "combined setup did not finish");
                drive(&mut state, &c, &h, &mut n, &mut t).await;
            }
            let os = [Arc::new(Mutex::new(Os::default())), Arc::new(Mutex::new(Os::default()))];
            let mut workers = Vec::new();
            if consent {
                workers.push(open(state.host.take_clipboard_worker().unwrap(), &os[0]));
                workers.push(open(state.viewer.take_clipboard_worker().unwrap(), &os[1]));
            } else {
                assert!(state.host.take_clipboard_worker().is_none());
                assert!(state.viewer.take_clipboard_worker().is_none());
                assert_eq!(state.viewer.clipboard_reason(), Some(ClipboardError::ConsentRequired));
            }
            let _ = state.viewer.action(key(true)).unwrap();
            // This is not a throughput qualification. Both large files share
            // one test budget within the unchanged twelve-second harness;
            // record, setup, input-ticket and lease deadlines stay untouched.
            let transfers_until = now(&c).unwrap() + 8_000_000;
            for (id, from, text) in [(1, 0, "host λ clipboard"), (2, 1, "viewer 👋 clipboard")] {
                let bytes: Vec<u8> = (0..600_019).map(|index| u8::try_from(index % 251).unwrap()).collect();
                assert_eq!(state.viewer.send_file(source.source(&bytes), &format!("received-{id}")).unwrap(), id);
                if consent { copy(&os[from], text); }
                let until = transfers_until;
                let mut copied = !consent;
                let mut serviced_during_transfer = false;
                loop {
                    assert!(now(&c).unwrap() < until, "concurrent transfer {id} stalled: copied={copied}, stage={:?}, result={:?}, failure={:?}, cleaned={}, actions={}, clipboard={:?}/{:?}", state.viewer.file_stage(), state.viewer.file_result(), state.viewer.file_failure(), state.viewer.file_cleanup_finished(), state.viewer.pending_actions(), state.host.clipboard_reason(), state.viewer.clipboard_reason());
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    if !copied {
                        let receipt = if from == 0 { state.viewer.take_clipboard_received() } else { state.host.take_clipboard_received() }.unwrap();
                        if let Some(receipt) = receipt {
                            assert!(matches!(receipt, Received::Consumed(Some(r)) if r.publication == Publication::SubmittedToOs));
                            assert_eq!(os[1-from].lock().unwrap().text.as_deref(), Some(text));
                            copied = true;
                        }
                    }
                    if copied && state.viewer.pending_actions() == 0 && state.viewer.file_result().is_none() {
                        serviced_during_transfer = true;
                    }
                    if copied && state.viewer.file_result().is_some() && state.viewer.file_cleanup_finished() && state.viewer.pending_actions() == 0 { break; }
                }
                assert!(serviced_during_transfer, "input and clipboard must progress before file completion");
                assert_eq!(state.viewer.file_result().unwrap().outcome, Outcome::HostPublished { bytes: bytes.len() as u64, publication: FilePublication::Durable });
                assert_eq!(fs::read(dest.0.join(format!("received-{id}"))).unwrap(), bytes);
                assert_eq!(state.viewer.input.binding().lease, input_lease);
                assert_eq!(state.viewer.take_file_result().unwrap().id, id);
                assert!(state.viewer.file_result().is_none());
            }
            if consent {
                assert_eq!(os[0].lock().unwrap().published.len(), 1, "no clipboard echo");
                assert_eq!(os[1].lock().unwrap().published.len(), 1, "no clipboard echo");
                state.host.retire_clipboard().unwrap();
                for _ in 0..10 { drive(&mut state, &c, &h, &mut n, &mut t).await; }
                assert!(state.host.clipboard_retired() && state.viewer.clipboard_retired());
            }
            // Retiring/refusing one optional lane leaves the other and input
            // alive, without a new lease, connection or replayed selection.
            assert_eq!(state.viewer.send_file(source.source(b"after clipboard"), "after").unwrap(), 3);
            let _ = state.viewer.action(key(false)).unwrap();
            let until = now(&c).unwrap() + 1_000_000;
            while state.viewer.file_result().is_none() || !state.viewer.file_cleanup_finished() || state.viewer.pending_actions() != 0 {
                assert!(now(&c).unwrap() < until);
                drive(&mut state, &c, &h, &mut n, &mut t).await;
            }
            assert_eq!(fs::read(dest.0.join("after")).unwrap(), b"after clipboard");
            assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
            let receipt = state.viewer.file_result();
            state.host.close(); state.viewer.close();
            assert_eq!(state.viewer.file_result(), receipt, "closure does not undo publication");
            for worker in &mut workers { worker.stop(); }
        }, driver)).await;
        assert!(shutdown.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}
#[test]
fn clipboard_first_transfers_both_directions_beside_files_and_input() {
    exercise(false, true);
}
#[test]
fn files_first_transfers_both_directions_beside_clipboard_and_input() {
    exercise(true, true);
}
#[test]
fn declining_clipboard_preserves_files_and_the_original_controller() {
    exercise(false, false);
    exercise(true, false);
}
