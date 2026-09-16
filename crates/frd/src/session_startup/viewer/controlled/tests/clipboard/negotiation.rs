//! Actual running owners negotiate the fifth pair; no manually attached routes.
//! Native OS contents/consent/IDs are fixtures, as in the adjacent session tests.
use super::*;
use crate::clipboard_quic::Error as ClipboardError;
use fr_transport::quic::ChannelRequest;

// Erase the large end-to-end future before entering the shared runtime harness;
// this keeps its code-generation footprint bounded as scenario coverage grows.
fn run<F, Fut>(body: F)
where
    F: FnOnce(Cx, Cx) -> Fut,
    Fut: Future<Output = ()> + 'static,
{
    super::run(
        move |client, host| -> std::pin::Pin<Box<dyn Future<Output = ()>>> {
            Box::pin(body(client, host))
        },
    );
}

async fn setup(c: &Cx, h: &Cx) -> Fixture {
    Box::pin(fixture_with_clipboard(
        c,
        h,
        caps(),
        false,
        false,
        false,
        ClipboardMode::Negotiate,
    ))
    .await
}
fn request(state: &Fixture) -> ChannelRequest {
    let b = state.viewer.media.binding();
    ChannelRequest {
        binding: decoder::Binding {
            parent: fr_wire::negotiation::ControlBinding { id: 12, ..b.parent },
            ..b
        },
        ticket: attachment::Ticket(9_999),
        timeout: Duration::from_secs(2),
    }
}
fn begin(state: &mut Fixture, host_consent: bool, viewer_consent: bool) {
    state
        .viewer
        .expect_clipboard(Duration::from_secs(2), viewer_consent)
        .unwrap();
    state
        .host
        .offer_clipboard(request(state), host_consent)
        .unwrap();
    assert!(state.viewer.clipboard_negotiating() && state.host.clipboard_negotiating());
    assert!(state.viewer.take_clipboard_worker().is_none());
    assert!(state.host.take_clipboard_worker().is_none());
    assert!(state.clipboard_channels.is_none());
}
fn not_handshake(bytes: &[u8]) {
    let kind = u16::from_be_bytes([bytes[6], bytes[7]]);
    assert!(
        !(0x18..=0x1c).contains(&kind) && kind != 0x54,
        "handshake leaked to application callback"
    );
}
async fn drive(
    state: &mut Fixture,
    client: &Cx,
    host: &Cx,
    nonce_id: &mut u128,
    ticket_id: &mut u128,
) {
    if now(host).unwrap() >= state.last_announce + 50_000 {
        announce(
            &mut state.host,
            &state.host_media,
            &state.observation,
            host,
            state.descriptor,
            false,
        );
        state.last_announce = now(host).unwrap();
    }
    if let Some(clock) = &mut state.host_clock {
        clock.receive(state.host.io().unwrap().0, block).unwrap();
        clock.service(state.host.io().unwrap().0).unwrap();
    }
    let (host_result,viewer_result)=Box::pin(support::both(
        state.host.drive(Duration::from_millis(1),||nonce(nonce_id),||{*ticket_id+=1;Some(InputTicketId::from_raw(*ticket_id))},|_,bytes|{not_handshake(bytes);Ok(Disposition::Blocked)}),
        state.viewer.drive(Duration::from_millis(1),|_|{},|route,bytes|{
            not_handshake(bytes);
            if matches!(route,Route::Stream(stream) if stream.messages==quic::Messages::Exact(Kind::Progress as u16)) {
                state.receiver.receive(Channel::MediaConfig,bytes,now(client).unwrap()).map_err(|_|())?;Ok(Disposition::Consumed)
            }else{Ok(Disposition::Blocked)}
        })
    )).await;
    host_result.unwrap();
    viewer_result.unwrap();
}
async fn finish(state: &mut Fixture, c: &Cx, h: &Cx, n: &mut u128, t: &mut u128) {
    let until = now(c).unwrap() + 1_000_000;
    while state.host.clipboard_negotiating() || state.viewer.clipboard_negotiating() {
        assert!(
            now(c).unwrap() < until,
            "regular drive did not finish optional negotiation"
        );
        drive(state, c, h, n, t).await;
    }
}
#[test]
fn automatic_startup_transfers_both_directions_and_keeps_input_running() {
    run(|c, h| async move {
        let mut s = setup(&c, &h).await;
        begin(&mut s, true, true);
        let driver = s.driver.take().unwrap();
        let ((),done)=Box::pin(support::both(async{
            let (mut n,mut t)=(80_000,90_000);
            let _ = s.viewer.action(key(true)).unwrap();
            finish(&mut s,&c,&h,&mut n,&mut t).await;
            let hs=s.host.take_clipboard_worker().unwrap();
            let vs=s.viewer.take_clipboard_worker().unwrap();
            assert!(s.host.take_clipboard_worker().is_none() && s.viewer.take_clipboard_worker().is_none());
            assert_eq!(s.host.offer_clipboard(request(&s),true),Err(ClipboardError::AlreadyAttached));
            assert_eq!(s.viewer.expect_clipboard(Duration::from_secs(2),true),Err(ClipboardError::AlreadyAttached));
            let os=[Arc::new(Mutex::new(Os::default())),Arc::new(Mutex::new(Os::default()))];
            let mut workers=[open(hs,&os[0]),open(vs,&os[1])];
            for (from,text) in [(0,"negotiated host copy"),(1,"viewer λ 👋"),(0,"")] {
                copy(&os[from],text);
                let until=now(&c).unwrap()+1_000_000;
                loop {
                    assert!(now(&c).unwrap()<until,"automatic clipboard did not arrive");
                    drive(&mut s,&c,&h,&mut n,&mut t).await;
                    let receipt=if from==0{s.viewer.take_clipboard_received()}else{s.host.take_clipboard_received()}.unwrap();
                    if let Some(receipt)=receipt {
                        assert!(matches!(receipt,Received::Consumed(Some(r)) if r.publication==Publication::SubmittedToOs));
                        assert_eq!(os[1-from].lock().unwrap().text.as_deref(),Some(text));break;
                    }
                }
                for _ in 0..5 {drive(&mut s,&c,&h,&mut n,&mut t).await;}
            }
            assert_eq!(s.effects.lock().unwrap().keys,[true]);
            assert_eq!(os[0].lock().unwrap().published.len(),1);
            assert_eq!(os[1].lock().unwrap().published.len(),2);
            s.host.retire_clipboard().unwrap();
            for _ in 0..10 {drive(&mut s,&c,&h,&mut n,&mut t).await;}
            assert!(s.host.clipboard_retired() && s.viewer.clipboard_retired());
            let _ = s.viewer.action(key(false)).unwrap();
            for _ in 0..10 {drive(&mut s,&c,&h,&mut n,&mut t).await;}
            assert_eq!(s.effects.lock().unwrap().keys,[true,false]);
            s.viewer.close();s.host.close();
            for worker in &mut workers {worker.stop();}
        },driver)).await;
        assert!(done.handoff_safe());
    });
}
#[test]
fn either_local_consent_declines_before_native_open_without_disconnect() {
    for (host, viewer) in [(false, true), (true, false), (false, false)] {
        run(move |c, h| async move {
            let mut s = setup(&c, &h).await;
            begin(&mut s, host, viewer);
            let driver = s.driver.take().unwrap();
            let ((), done) = Box::pin(support::both(
                async {
                    let (mut n, mut t) = (80_000, 90_000);
                    finish(&mut s, &c, &h, &mut n, &mut t).await;
                    assert_eq!(
                        s.host.clipboard_reason(),
                        Some(ClipboardError::ConsentRequired)
                    );
                    assert_eq!(
                        s.viewer.clipboard_reason(),
                        Some(ClipboardError::ConsentRequired)
                    );
                    assert!(
                        s.host.take_clipboard_worker().is_none()
                            && s.viewer.take_clipboard_worker().is_none()
                    );
                    assert!(
                        s.host.clipboard_switches().is_none()
                            && s.viewer.clipboard_switches().is_none()
                    );
                    assert!(s.host.clipboard_retired() && s.viewer.clipboard_retired());
                    assert_eq!(
                        s.host.offer_clipboard(request(&s), true),
                        Err(ClipboardError::AlreadyAttached)
                    );
                    assert_eq!(
                        s.viewer.expect_clipboard(Duration::from_secs(2), true),
                        Err(ClipboardError::AlreadyAttached)
                    );
                    for _ in 0..10 {
                        drive(&mut s, &c, &h, &mut n, &mut t).await;
                    }
                    let _ = s.viewer.action(key(true)).unwrap();
                    for _ in 0..10 {
                        drive(&mut s, &c, &h, &mut n, &mut t).await;
                    }
                    assert_eq!(s.effects.lock().unwrap().keys, [true]);
                    assert!(!s.viewer.is_closed() && !s.host.control().is_stopped());
                    s.viewer.close();
                    s.host.close();
                },
                driver,
            ))
            .await;
            assert!(done.handoff_safe());
        });
    }
}
#[test]
fn capability_and_timeout_refusal_leave_existing_input_usable() {
    run(|c, h| async move {
        let mut s = fixture(&c, &h).await;
        assert_eq!(
            s.host.offer_clipboard(request(&s), true),
            Err(ClipboardError::NotNegotiated)
        );
        assert_eq!(
            s.viewer.expect_clipboard(Duration::from_secs(2), true),
            Err(ClipboardError::NotNegotiated)
        );
        assert!(!s.host.clipboard_negotiating() && !s.viewer.clipboard_negotiating());
        let driver = s.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let _ = s.viewer.action(key(true)).unwrap();
                for _ in 0..10 {
                    drive(&mut s, &c, &h, &mut n, &mut t).await;
                }
                assert_eq!(s.effects.lock().unwrap().keys, [true]);
                s.viewer.close();
                s.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
    run(|c, h| async move {
        let mut s = setup(&c, &h).await;
        for timeout in [Duration::ZERO, Duration::from_secs(3)] {
            let mut r = request(&s);
            r.timeout = timeout;
            assert_eq!(s.host.offer_clipboard(r, true), Err(ClipboardError::Limit));
            assert_eq!(
                s.viewer.expect_clipboard(timeout, true),
                Err(ClipboardError::Limit)
            );
            assert!(!s.host.clipboard_negotiating() && !s.viewer.clipboard_negotiating());
        }
        begin(&mut s, true, true);
        s.viewer.close();
        s.host.close();
        assert!(s.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn unpolled_negotiation_drive_never_delivers_worker_and_pending_retirement_fences() {
    for host in [false, true] {
        run(move |c, h| async move {
            let mut s = setup(&c, &h).await;
            begin(&mut s, true, true);
            if host {
                drop(s.host.drive(
                    Duration::from_millis(1),
                    || Ok(88),
                    || Some(InputTicketId::from_raw(99)),
                    block,
                ));
            } else {
                drop(s.viewer.drive(Duration::from_millis(1), |_| {}, block));
            }
            assert!(
                s.host.take_clipboard_worker().is_none()
                    && s.viewer.take_clipboard_worker().is_none()
            );
            if host {
                assert!(s.host.control().is_stopped());
            } else {
                assert!(s.viewer.is_closed());
            }
            s.viewer.close();
            s.host.close();
            assert!(s.driver.take().unwrap().await.handoff_safe());
        });
    }
    run(|c, h| async move {
        let mut s = setup(&c, &h).await;
        begin(&mut s, true, true);
        assert_eq!(s.viewer.retire_clipboard(), Err(ClipboardError::Cancelled));
        assert_eq!(s.host.retire_clipboard(), Err(ClipboardError::Cancelled));
        assert!(s.viewer.is_closed() && s.host.control().is_stopped());
        assert!(s.driver.take().unwrap().await.handoff_safe());
    });
}

#[test]
fn first_poll_cannot_restart_the_original_negotiation_deadline() {
    for host in [false, true] {
        run(move |c, h| async move {
            let mut state = setup(&c, &h).await;
            state
                .viewer
                .expect_clipboard(Duration::from_millis(30), true)
                .unwrap();
            let mut offer = request(&state);
            offer.timeout = Duration::from_millis(30);
            state.host.offer_clipboard(offer, true).unwrap();
            asupersync::time::sleep(c.now(), Duration::from_millis(50)).await;
            if host {
                let error = state
                    .host
                    .drive(
                        Duration::ZERO,
                        || Ok(80_001),
                        || Some(InputTicketId::from_raw(90_001)),
                        block,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    error,
                    crate::session_startup::Error::Clipboard(ClipboardError::SetupExpired)
                );
                assert!(state.host.control().is_stopped());
            } else {
                let error = state
                    .viewer
                    .drive(Duration::ZERO, |_| {}, block)
                    .await
                    .unwrap_err();
                assert_eq!(error, Error::Clipboard(ClipboardError::SetupExpired));
                assert!(state.viewer.is_closed());
            }
            assert!(state.viewer.take_clipboard_worker().is_none());
            assert!(state.host.take_clipboard_worker().is_none());
            state.host.close();
            state.viewer.close();
            assert!(state.driver.take().unwrap().await.handoff_safe());
        });
    }
}

#[test]
fn uncollected_or_delivered_ready_seed_still_follows_session_cancellation() {
    for host in [false, true] {
        run(move |c, h| async move {
            let mut state = setup(&c, &h).await;
            begin(&mut state, true, true);
            let driver = state.driver.take().unwrap();
            let ((), done) = Box::pin(support::both(
                async {
                    let (mut n, mut t) = (80_000, 90_000);
                    finish(&mut state, &c, &h, &mut n, &mut t).await;
                    let seed = if host {
                        state.host.take_clipboard_worker()
                    } else {
                        state.viewer.take_clipboard_worker()
                    }
                    .unwrap();
                    state.host.close();
                    state.viewer.close();
                    assert!(state.host.take_clipboard_worker().is_none());
                    assert!(state.viewer.take_clipboard_worker().is_none());
                    let opened = std::cell::Cell::new(false);
                    let result = seed.open(|| {
                        opened.set(true);
                        Ok(Native {
                            os: Arc::new(Mutex::new(Os::default())),
                            seen: 0,
                            reading: false,
                        })
                    });
                    assert!(result.is_err());
                    assert!(!opened.get(), "cancelled seed called native factory");
                },
                driver,
            ))
            .await;
            assert!(done.handoff_safe());
        });
    }
}

#[test]
fn newly_negotiated_workers_transfer_full_item_limit_in_both_directions() {
    // Each success case begins with its actual freshly granted host deadline.
    // A copy started near the end of a previous lease is allowed to expire; a
    // later renewal deliberately cannot extend that already captured deadline.
    for from in [0, 1] {
        run(move |c, h| async move {
            let mut state = setup(&c, &h).await;
            begin(&mut state, true, true);
            let driver = state.driver.take().unwrap();
            let ((), done) = Box::pin(support::both(async {
                let (mut n, mut t) = (80_000, 90_000);
                finish(&mut state, &c, &h, &mut n, &mut t).await;
                let os = [Arc::new(Mutex::new(Os::default())), Arc::new(Mutex::new(Os::default()))];
                let mut workers = [open(state.host.take_clipboard_worker().unwrap(), &os[0]),
                    open(state.viewer.take_clipboard_worker().unwrap(), &os[1])];
                let text = "λ".repeat(524_288);
                let mut max_native_records = 0;
                let _ = state.viewer.action(key(true)).unwrap();
                copy(&os[from], &text);
                let until = now(&c).unwrap() + 2_500_000;
                loop {
                    assert!(now(&c).unwrap() < until, "full bounded item failed to transfer; sender={from}; host={:?}; viewer={:?}", state.host.clipboard_reason(), state.viewer.clipboard_reason());
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    for usage in [state.host.io().unwrap().0.usage(), state.viewer.session.io().unwrap().0.usage()] {
                        let bulk = usage.retained_send_records - usage.critical_send_records;
                        max_native_records = max_native_records.max(bulk);
                        assert!(bulk <= 4, "worker handoff exceeded its four-record native bound");
                        assert!(usage.retained_send_upper_bound - usage.critical_send_bytes <= 4 * 8192);
                    }
                    let receipt = if from == 0 { state.viewer.take_clipboard_received() } else { state.host.take_clipboard_received() }.unwrap();
                    if let Some(receipt) = receipt {
                        assert!(matches!(receipt, Received::Consumed(Some(r)) if r.publication == Publication::SubmittedToOs), "terminal clipboard result: {receipt:?}; sender {from}");
                        assert_eq!(os[1-from].lock().unwrap().text.as_deref(), Some(text.as_str()));
                        break;
                    }
                }
                assert!(max_native_records >= 2, "test must exercise pipelined native records");
                let _ = state.viewer.action(key(false)).unwrap();
                for _ in 0..10 { drive(&mut state, &c, &h, &mut n, &mut t).await; }
                assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                state.host.close(); state.viewer.close();
                for worker in &mut workers { worker.stop(); }
            }, driver)).await;
            assert!(done.handoff_safe());
        });
    }
}
