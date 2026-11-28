//! Original control socket: negotiation, renewal, clock and receiver credit.
//! Source discovery progresses beside the retained reader and writer futures.
use super::*;
use asupersync::io::split_owned;
use fr_wire::{authority, clock, receiver_pressure};
#[cfg(test)]
mod tests;

#[allow(clippy::too_many_lines)] // Original admission, consent and capture custody transfer in one ordered startup.
pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    ingress: &BrowserIngress,
    stream: &mut S,
    req: &http::HttpRequest,
    metadata: &FetchMetadata<'_>,
    peer: Lease,
    cx: Cx,
    sni: Option<&str>,
) -> io::Result<()> {
    let service = ingress.desktop.as_ref().ok_or_else(failed)?;
    let started = host_now(&cx).map_err(|_| failed())?.as_micros();
    let first = bounded_startup(
        first_message_auth_duration().min(ingress.startup_budget()),
        async {
            if !ingress
                .validate_ws_upgrade(
                    stream,
                    req,
                    metadata,
                    sni,
                    fr_transport::wss::WssChannelRole::Control,
                )
                .await?
            {
                return Ok(None);
            }
            let frame = websocket::read_ws_frame(stream, browser::MAX_RECORD, true)
                .await
                .map_err(|_| failed())?;
            let websocket::WsFrame::Binary(bytes) = frame else {
                return Err(failed());
            };
            let hello = browser::decode_hello(&bytes, browser::MAX_RECORD).map_err(|_| failed())?;
            Ok(Some(hello))
        },
    )
    .await?;
    let Some(hello) = first else {
        return Ok(());
    };
    if hello.offer.role != Role::Observe {
        return refuse(stream, Reason::PermissionDenied).await;
    }
    let now = host_now(&cx).map_err(|_| failed())?;
    let id = match ingress.policy.with_nonce_manager(|manager| {
        manager.consume_nonce(
            hello.nonce(),
            peer.addresses().peer.ip(),
            BrowserSessionRole::Observer,
            now,
        )
    })? {
        Ok(id) => id,
        Err(_) => return refuse(stream, Reason::PermissionDenied).await,
    };
    let reservation = match service.reserve() {
        Ok(reservation) => reservation,
        Err(_) => return refuse(stream, Reason::ResourceLimit).await,
    };
    let offer = match service.offer(ingress.config.limits) {
        Ok(offer) => offer,
        Err(reason) => return refuse(stream, reason).await,
    };
    let host = match browser::intersect(&offer, &hello.offer) {
        Ok(host) => host,
        Err(_) => return refuse(stream, Reason::RequiredCapability).await,
    };
    let binding = ControlBinding {
        id: 1,
        host_boot: ingress.config.host_boot,
        os_session: ingress.config.os_session,
        remote_session: id,
    };
    let maximum = browser::MAX_RECORD.min(host.limits.max_control_message_bytes() as usize);
    let until = started
        .checked_add(
            u64::from(ingress.config.limits.max_handshake_duration_ms()).saturating_mul(1000),
        )
        .ok_or_else(expired)?;
    let left = until.checked_sub(now.as_micros()).ok_or_else(expired)?;
    let negotiated = bounded_startup(Duration::from_micros(left), async {
        let bytes = encode_message(&Message::HostCapabilities(host.clone()), maximum)?;
        websocket::write_server_binary(stream, &bytes).await?;
        let websocket::WsFrame::Binary(bytes) = websocket::read_ws_frame(stream, maximum, true)
            .await
            .map_err(|_| failed())?
        else {
            return Err(failed());
        };
        let Message::SelectedConfiguration(selection) =
            browser::decode(&bytes, maximum, 0).map_err(|_| failed())?
        else {
            return Err(failed());
        };
        browser::check_selection(&selection, &host).map_err(|_| failed())?;
        Ok(selection)
    })
    .await?;
    let selection = negotiated;
    let (byte_allowance, picture_allowance) = match credit_limits(&ingress.config, &selection) {
        Ok(allowance) => allowance,
        Err(reason) => return refuse(stream, reason).await,
    };
    let initial_until_us = started
        .checked_add(u64::from(selection.limits.max_handshake_duration_ms()) * 1000)
        .ok_or_else(expired)?;
    let startup_until_us = if service.require_approval {
        if !browser::local_approval_required(&selection) {
            return refuse(stream, Reason::RequiredCapability).await;
        }
        started
            .checked_add(browser::APPROVAL_STARTUP_BUDGET_US)
            .ok_or_else(expired)?
    } else {
        initial_until_us
    };
    let approval = if service.require_approval {
        let pending = crate::session_startup::PendingApproval::browser(
            cx.clone(),
            &peer,
            binding,
            &selection,
            startup_until_us,
        )
        .map_err(|_| failed())?;
        // Announce this original request inside the original negotiation bound.
        // Neither receiving selection nor writing the notice starts a new timer.
        let now = host_now(&cx).map_err(|_| failed())?.as_micros();
        let left = initial_until_us
            .checked_sub(now)
            .filter(|left| *left != 0)
            .ok_or_else(expired)?;
        let notice = encode_message(
            &Message::ApprovalRequired {
                request: binding.remote_session,
                deadline_us: startup_until_us,
                role: Role::Observe,
            },
            maximum,
        )?;
        bounded_startup(
            Duration::from_micros(left),
            websocket::write_server_binary(stream, &notice),
        )
        .await?;
        Some(pending)
    } else {
        None
    };
    let now = host_now(&cx).map_err(|_| failed())?.as_micros();
    let left = startup_until_us
        .checked_sub(now)
        .filter(|left| *left != 0)
        .ok_or_else(expired)?;
    let prepared = (service.factory)(Request {
        peer: peer.clone(),
        cx: cx.clone(),
        binding,
        selection: selection.clone(),
        approval,
    });
    let authorized = bounded_startup(
        Duration::from_micros(left),
        authorize_while_connected(stream, &peer, &cx, prepared),
    )
    .await;
    let authorized = match authorized {
        Ok(authorized) => authorized,
        Err(error) if service.require_approval && error.kind() == io::ErrorKind::TimedOut => {
            return refuse(stream, Reason::ApprovalExpired).await;
        }
        Err(error) => return Err(error),
    };
    let authorized = match authorized {
        Ok(authorized) => authorized,
        Err(reason) => return refuse(stream, reason).await,
    };
    let Authorized {
        control,
        launch,
        select,
        cleanup,
        supervise,
    } = authorized;
    let mut authorization = AuthorizationGuard(Some(control.clone()));
    if control.claim_browser_renewal(id, &peer).is_err() {
        control.revoke();
        return Err(failed());
    }
    let (launch, retirement) = launch.retain_cleanup().map_err(|_| failed())?;
    let now = control.check().map_err(|_| failed())?.as_micros();
    if now >= startup_until_us {
        return Err(expired());
    }
    let session = Arc::new(Session {
        binding,
        selection,
        byte_allowance,
        picture_allowance,
        peer: peer.clone(),
        control,
        cleanup,
        supervisor: Mutex::new(Some(supervise)),
        state: Mutex::new(State {
            accepted: false,
            descriptor: None,
            grant: None,
            binding_accepted: false,
            video_attached: false,
            running: false,
            pending_challenge: false,
            next_challenge_us: now,
            credit: ReceiverCreditWindow::new(1, byte_allowance, u64::from(picture_allowance))
                .map_err(|_| failed())?,
            outgoing: VecDeque::new(),
        }),
        prepared: Mutex::new(None),
        retirement: Mutex::new(Some(retirement)),
        closed: AtomicBool::new(false),
        startup_until_us,
        reaped: AtomicBool::new(false),
        parked: Mutex::new([None, None]),
        writing: [AtomicBool::new(false), AtomicBool::new(false)],
    });
    // Install original cleanup custody before the first native preparation poll.
    reservation.install(Arc::clone(&session))?;
    let guard = SessionGuard(Arc::clone(&session));
    authorization.0.take();
    let result = active(ingress, stream, peer, Arc::clone(&session), launch, select).await;
    drop(guard); // fence before worker/transport cleanup
    let cleanup_result = service.retire_session(&session, &session.cleanup).await;
    result.and(cleanup_result)
}

/// Before `SessionOpened` there is no permitted application message. Observe
/// EOF/early bytes while a real local prompt waits; a disconnected request
/// cannot leave a consumable positive decision or start native capture later.
async fn authorize_while_connected<S: AsyncRead + Unpin>(
    stream: &mut S,
    peer: &Lease,
    cx: &Cx,
    work: impl Future<Output = Result<Authorized, Reason>>,
) -> io::Result<Result<Authorized, Reason>> {
    authorize_guarded(
        stream,
        || {
            cx.checkpoint().map_err(|_| failed())?;
            peer.observe().map(|_| ()).map_err(|_| failed())
        },
        work,
    )
    .await
}

async fn authorize_guarded<S: AsyncRead + Unpin>(
    stream: &mut S,
    check: impl Fn() -> io::Result<()>,
    work: impl Future<Output = Result<Authorized, Reason>>,
) -> io::Result<Result<Authorized, Reason>> {
    let mut work = pin!(work);
    poll_fn(|task| {
        check_waiting_transport(stream, &check, task)?;
        let result = work.as_mut().poll(task);
        if let Err(error) = check_waiting_transport(stream, &check, task) {
            if let Poll::Ready(Ok(authorized)) = &result {
                authorized.control.revoke();
            }
            return Poll::Ready(Err(error));
        }
        result.map(Ok)
    })
    .await
}

fn check_waiting_transport<S: AsyncRead + Unpin>(
    stream: &mut S,
    check: &impl Fn() -> io::Result<()>,
    task: &mut Context<'_>,
) -> io::Result<()> {
    check()?;
    let mut byte = [0];
    let mut buffer = ReadBuf::new(&mut byte);
    match Pin::new(stream).poll_read(task, &mut buffer) {
        Poll::Ready(_) => Err(failed()),
        Poll::Pending => check(),
    }
}

async fn active<S: AsyncRead + AsyncWrite + Unpin>(
    ingress: &BrowserIngress,
    stream: &mut S,
    peer: Lease,
    session: Arc<Session>,
    launch: Launch,
    select: SelectSource,
) -> io::Result<()> {
    let observation_until_us = session
        .control
        .observation_until()
        .map_err(|_| failed())?
        .as_micros();
    let opened = encode_message(
        &Message::SessionOpened {
            binding: session.binding,
            selection: session.selection.clone(),
            observation_until_us,
        },
        browser::MAX_RECORD,
    )?;
    {
        let mut output = GuardedIo::session(&mut *stream, peer.clone(), Arc::clone(&session), 0);
        output.until_us = Some(observation_until_us);
        session
            .run(0, websocket::write_server_binary(&mut output, &opened))
            .await?;
    }
    let (read, write) = split_owned(stream);
    let read = GuardedIo::session(read, peer.clone(), Arc::clone(&session), 0);
    let write = GuardedIo::session(write, peer, Arc::clone(&session), 0);
    let mut reader = pin!(read_control(ingress, read, Arc::clone(&session)));
    let mut writer = pin!(write_control(write, Arc::clone(&session)));
    let mut prepare = pin!(prepare(&session, launch, select));
    let mut prepared = false;
    poll_fn(|task| {
        if let Err(error) = session.poll_check(task, 0) {
            return Poll::Ready(Err(error));
        }
        // Inbound revocation/renewal and outbound challenges remain live while
        // the original native discovery/configuration worker is parked.
        if let Poll::Ready(result) = reader.as_mut().poll(task) {
            return Poll::Ready(result);
        }
        if let Poll::Ready(result) = writer.as_mut().poll(task) {
            return Poll::Ready(result);
        }
        if !prepared {
            match prepare.as_mut().poll(task) {
                Poll::Ready(Ok(())) => prepared = true,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => {}
            }
        }
        Poll::Pending
    })
    .await
}

async fn prepare(session: &Session, launch: Launch, choose: SelectSource) -> io::Result<()> {
    loop {
        let accepted = session.state.lock().map_err(|_| failed())?.accepted;
        if accepted {
            break;
        }
        session.pause(5_000).await?;
    }
    let discovered = crate::media::discovery::DiscoveredSource::start(&session.control, launch)
        .await
        .map_err(|_| failed())?;
    let catalog = discovered.catalog().map_err(|_| failed())?;
    let (choice, configuration) = choose(&catalog).map_err(|_| failed())?;
    let display = catalog.selected(choice).map_err(|_| failed())?;
    let limits = session.selection.limits;
    limits
        .validate_coded_dimensions(configuration.width, configuration.height)
        .map_err(|_| failed())?;
    if configuration.max_access_unit_bytes > limits.max_encoded_access_unit_bytes()
        || !display.is_picture(configuration.width, configuration.height)
        || configuration.fps == 0
    {
        return Err(failed());
    }
    let source = discovered
        .configure_browser(
            &session.control,
            session.binding.remote_session,
            choice,
            configuration,
        )
        .map_err(|_| failed())?
        .await
        .map_err(|_| failed())?;
    let descriptor = browser::attachment::Descriptor {
        binding: decoder::Binding {
            parent: ControlBinding {
                id: 2,
                ..session.binding
            },
            display: display.handle,
            geometry: display.geometry,
            configuration: configuration.generation,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        },
    };
    descriptor.validate(session.binding).map_err(|_| failed())?;
    session.check()?;
    *session.prepared.lock().map_err(|_| failed())? = Some(Prepared {
        source,
        configuration,
    });
    session.state.lock().map_err(|_| failed())?.descriptor = Some(descriptor);
    session.enqueue(
        2,
        encode_attachment(browser::attachment::Message::Binding(descriptor), session)?,
    )
}

async fn read_control<R: AsyncRead + Unpin>(
    ingress: &BrowserIngress,
    mut read: GuardedIo<R>,
    session: Arc<Session>,
) -> io::Result<()> {
    let mut rate_start = session.check()?;
    let mut count = 0_u32;
    let mut clock_sequence = 0_u64;
    loop {
        let now = session.check()?;
        if now.saturating_sub(rate_start) >= 1_000_000 {
            rate_start = now;
            count = 0;
        }
        count = count.checked_add(1).ok_or_else(failed)?;
        if count
            > session
                .selection
                .limits
                .max_control_requests_per_sec()
                .min(128)
        {
            return Err(failed());
        }
        read.until_us = Some(now.checked_add(3_000_000).ok_or_else(expired)?);
        let frame = websocket::read_ws_frame(&mut read, browser::MAX_RECORD, true)
            .await
            .map_err(|_| failed())?;
        let bytes = match frame {
            websocket::WsFrame::Binary(bytes) => bytes,
            websocket::WsFrame::Close => return Ok(()),
            websocket::WsFrame::Ping(bytes) => {
                session.enqueue(10, bytes)?;
                continue;
            }
            websocket::WsFrame::Pong(_) => {
                session.pause(1_000).await?;
                continue;
            }
        };
        let accepted = session.state.lock().map_err(|_| failed())?.accepted;
        if !accepted {
            if browser::decode(&bytes, browser::MAX_RECORD, session.binding.id)
                .map_err(|_| failed())?
                != (Message::BindingAccepted {
                    binding: session.binding.id,
                })
            {
                return Err(failed());
            }
            session.state.lock().map_err(|_| failed())?.accepted = true;
        } else {
            use fr_wire::Kind;
            match browser::record_kind(&bytes, browser::MAX_RECORD, session.binding.id)
                .map_err(|_| failed())?
            {
                Kind::ChallengeResponse => {
                    let message = authority::decode(
                        &bytes,
                        authority::Binding {
                            channel: session.binding.id,
                            session: session.binding.remote_session,
                        },
                        &session.selection.limits,
                        Direction::ViewerToHost,
                        Delivery::Reliable,
                    )
                    .map_err(|_| failed())?;
                    let authority::Message::Response {
                        scope: authority::Scope::Observation,
                        nonce,
                    } = message
                    else {
                        return Err(failed());
                    };
                    session.control.renew(nonce).map_err(|_| failed())?;
                    session
                        .state
                        .lock()
                        .map_err(|_| failed())?
                        .pending_challenge = false;
                }
                Kind::ClockProbe => {
                    let clock::Message::Probe { sequence } = clock::decode(
                        &bytes,
                        session.binding,
                        &session.selection.limits,
                        Direction::ViewerToHost,
                        Delivery::Reliable,
                    )
                    .map_err(|_| failed())?
                    else {
                        return Err(failed());
                    };
                    if sequence <= clock_sequence {
                        return Err(failed());
                    }
                    clock_sequence = sequence;
                    let mut output = vec![0; clock::REPLY_BYTES];
                    let size = clock::encode(
                        clock::Message::Reply {
                            sequence,
                            host_sample_us: session.check()?,
                        },
                        session.binding,
                        &session.selection.limits,
                        &mut output,
                        Direction::HostToViewer,
                        Delivery::Reliable,
                    )
                    .map_err(|_| failed())?;
                    output.truncate(size);
                    session.enqueue(2, output)?;
                }
                Kind::ReceiverPressure => {
                    let report = receiver_pressure::decode(
                        &bytes,
                        session.binding,
                        &session.selection.limits,
                        Direction::ViewerToHost,
                        Delivery::Reliable,
                    )
                    .map_err(|_| failed())?;
                    let mut state = session.state.lock().map_err(|_| failed())?;
                    if !state.video_attached {
                        return Err(failed());
                    }
                    let binding = state.descriptor.ok_or_else(failed)?.binding.parent.id;
                    state
                        .credit
                        .apply_wire_pressure(
                            binding,
                            receiver_pressure::ChannelRole::Video,
                            &report,
                        )
                        .map_err(|_| failed())?;
                }
                Kind::BindingAccepted => {
                    let browser::attachment::Message::Accepted(binding) =
                        browser::attachment::decode(
                            &bytes,
                            session.binding,
                            session.binding.id,
                            &session.selection.limits,
                            Direction::ViewerToHost,
                            Delivery::Reliable,
                        )
                        .map_err(|_| failed())?
                    else {
                        return Err(failed());
                    };
                    let descriptor = {
                        let state = session.state.lock().map_err(|_| failed())?;
                        if state.binding_accepted {
                            return Err(failed());
                        }
                        let descriptor = state.descriptor.ok_or_else(failed)?;
                        if binding != descriptor.binding.parent.id {
                            return Err(failed());
                        }
                        descriptor
                    };
                    let now = session.control.check().map_err(|_| failed())?;
                    let ticket = ingress
                        .policy
                        .issue_auxiliary_ticket(
                            session.binding.remote_session,
                            AuxiliaryChannelRole::VideoStream,
                            session.peer.addresses().peer.ip(),
                            now,
                        )
                        .map_err(|_| failed())?;
                    let grant = browser::attachment::Grant {
                        descriptor,
                        ticket: browser::attachment::Ticket(*ticket.as_bytes()),
                        deadline_us: now
                            .as_micros()
                            .checked_add(3_000_000)
                            .ok_or_else(expired)?
                            .min(session.startup_until_us),
                        byte_allowance: session.byte_allowance,
                        picture_allowance: session.picture_allowance,
                        credit_epoch: 1,
                    };
                    grant
                        .validate(session.binding, &session.selection.limits)
                        .map_err(|_| failed())?;
                    {
                        let mut state = session.state.lock().map_err(|_| failed())?;
                        state.binding_accepted = true;
                        state.grant = Some(grant);
                    }
                    session.enqueue(
                        2,
                        encode_attachment(browser::attachment::Message::Ticket(grant), &session)?,
                    )?;
                }
                _ => return Err(failed()),
            }
        }
        // Bound one connection's ready work per poll, independently of the
        // per-second request ceiling. Partial frame reads remain in this future.
        session.pause(1_000).await?;
    }
}

async fn write_control<W: AsyncWrite + Unpin>(
    mut write: GuardedIo<W>,
    session: Arc<Session>,
) -> io::Result<()> {
    loop {
        let now = session.check()?;
        let challenge_due = {
            let state = session.state.lock().map_err(|_| failed())?;
            state.accepted && !state.pending_challenge && now >= state.next_challenge_us
        };
        if challenge_due {
            let nonce = random_id()?;
            let deadline = session
                .control
                .issue_challenge(nonce)
                .map_err(|_| failed())?;
            let mut bytes = vec![0; authority::MAX_AUTHORITY_BYTES];
            let length = authority::encode(
                authority::Message::Challenge {
                    scope: authority::Scope::Observation,
                    nonce,
                    deadline_micros: deadline.as_micros(),
                },
                authority::Binding {
                    channel: session.binding.id,
                    session: session.binding.remote_session,
                },
                &session.selection.limits,
                &mut bytes,
                Direction::HostToViewer,
                Delivery::Reliable,
            )
            .map_err(|_| failed())?;
            bytes.truncate(length);
            {
                let mut state = session.state.lock().map_err(|_| failed())?;
                state.pending_challenge = true;
                state.next_challenge_us = now.checked_add(1_000_000).ok_or_else(expired)?;
            }
            session.enqueue(2, bytes)?;
        }
        let next = session
            .state
            .lock()
            .map_err(|_| failed())?
            .outgoing
            .pop_front();
        if let Some((opcode, bytes)) = next {
            write.until_us = Some(now.checked_add(1_000_000).ok_or_else(expired)?);
            let custody = session.begin_write(0)?;
            websocket::write_ws_frame(&mut write, opcode, &bytes, None).await?;
            custody.flushed();
        }
        session.pause(5_000).await?;
    }
}
