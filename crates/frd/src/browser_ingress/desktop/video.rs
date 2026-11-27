//! One ticket-bound media socket using the existing source and sender engine.
//! The WSS adapter adds credit and reliable writes, not a second media protocol.
use super::*;
use crate::media::{CaptureUpdate, Subscription, decoder_startup};
use asupersync::io::split_owned;
use fr_media::{
    delivery::{MediaBindings, MediaEpoch, PacketOffer},
    hevc::HevcGuard,
};
use fr_wire::{MAX_FRAGMENTS, MAX_REPAIR_RANGES, MediaLimits};
use subtle::ConstantTimeEq;

fn same_grant(left: browser::attachment::Grant, right: browser::attachment::Grant) -> bool {
    left.descriptor == right.descriptor
        && left.deadline_us == right.deadline_us
        && left.byte_allowance == right.byte_allowance
        && left.picture_allowance == right.picture_allowance
        && left.credit_epoch == right.credit_epoch
        && bool::from(left.ticket.0.ct_eq(&right.ticket.0))
}

pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    ingress: &BrowserIngress,
    stream: &mut S,
    req: &http::HttpRequest,
    metadata: &FetchMetadata<'_>,
    peer: Lease,
    sni: Option<&str>,
) -> io::Result<()> {
    let service = ingress.desktop.as_ref().ok_or_else(failed)?;
    // Reading an unauthenticated auxiliary frame never takes capture ownership,
    // revokes a healthy session, or spends that session's one-use ticket.
    let first = bounded_startup(
        first_message_auth_duration().min(ingress.startup_budget()),
        async {
            if !ingress
                .validate_ws_upgrade(
                    stream,
                    req,
                    metadata,
                    sni,
                    fr_transport::wss::WssChannelRole::Video,
                )
                .await?
            {
                return Ok(None);
            }
            let frame =
                websocket::read_ws_frame(stream, browser::attachment::GRANT_RECORD_BYTES, true)
                    .await
                    .map_err(|_| failed())?;
            let websocket::WsFrame::Binary(bytes) = frame else {
                return Err(failed());
            };
            Ok(Some(bytes))
        },
    )
    .await?;
    let Some(bytes) = first else {
        return Ok(());
    };
    let session = service.session()?;
    if !peer.same_peer(&session.peer).map_err(|_| failed())? {
        return Err(failed());
    }
    let expected = session
        .state
        .lock()
        .map_err(|_| failed())?
        .grant
        .ok_or_else(failed)?;
    let browser::attachment::Message::Attach(grant) = browser::attachment::decode(
        &bytes,
        session.binding,
        expected.descriptor.binding.parent.id,
        &session.selection.limits,
        Direction::ViewerToHost,
        Delivery::Reliable,
    )
    .map_err(|_| failed())?
    else {
        return Err(failed());
    };
    let now = session.check()?;
    if !same_grant(grant, expected) || now >= grant.deadline_us {
        return Err(failed());
    }
    {
        let mut state = session.state.lock().map_err(|_| failed())?;
        if state.video_attached
            || !state.binding_accepted
            || state
                .grant
                .is_none_or(|original| !same_grant(original, grant))
        {
            return Err(failed());
        }
        ingress
            .policy
            .consume_auxiliary_ticket(
                Some(&grant.ticket.0),
                session.binding.remote_session,
                AuxiliaryChannelRole::VideoStream,
                peer.addresses().peer.ip(),
                HostInstant::from_micros(now),
            )
            .map_err(|_| failed())?;
        state.video_attached = true;
    }
    let guard = SessionGuard(Arc::clone(&session));
    let prepared = session
        .prepared
        .lock()
        .map_err(|_| failed())?
        .take()
        .ok_or_else(failed)?;
    let result = session
        .run(
            1,
            active(stream, peer, Arc::clone(&session), grant, prepared),
        )
        .await;
    drop(guard);
    result
}

async fn active<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    peer: Lease,
    session: Arc<Session>,
    grant: browser::attachment::Grant,
    prepared: Prepared,
) -> io::Result<()> {
    let Prepared {
        mut source,
        configuration,
    } = prepared;
    let binding = grant.descriptor.binding;
    let limits = session.selection.limits;
    let record_bytes =
        browser::MAX_VIDEO_RECORD_BYTES.min(limits.max_control_message_bytes() as usize);
    let media_limits = MediaLimits::new(limits, record_bytes, MAX_FRAGMENTS, MAX_REPAIR_RANGES)
        .map_err(|_| failed())?;
    let mut policy = decoder_startup::host_send_policy();
    policy.max_cached_bytes =
        usize::try_from(limits.per_viewer_compressed_bytes()).map_err(|_| failed())?;
    policy.repair_bytes_per_window = policy.repair_bytes_per_window.min(policy.max_cached_bytes);
    let mut sender = Subscription::new(
        session.control.clone(),
        media_limits,
        MediaBindings::browser_wss(binding.parent.id).map_err(|_| failed())?,
        MediaEpoch {
            configuration: binding.configuration,
            recovery: binding.recovery,
        },
        policy,
    )
    .map_err(|_| failed())?;
    if configuration.max_access_unit_bytes as usize > sender.maximum_capacity() {
        return Err(failed());
    }
    let (read, write) = split_owned(stream);
    let mut read = GuardedIo::session(read, peer.clone(), Arc::clone(&session), 1);
    let mut write = GuardedIo::session(write, peer, Arc::clone(&session), 1);
    write.until_us = Some(grant.deadline_us);
    let attached = encode_attachment(browser::attachment::Message::Attached(grant), &session)?;
    websocket::write_server_binary(&mut write, &attached).await?;

    source
        .check_selected_display(&session.control)
        .await
        .map_err(|_| failed())?;
    let first = source
        .capture_if_changed(&session.control, true)
        .await
        .map_err(|_| failed())?;
    let unit = first.encoded().ok_or_else(failed)?;
    if !unit.is_idr() || unit.config_generation() != binding.configuration {
        return Err(failed());
    }
    let first_frame = unit.frame().as_raw();
    let capture_micros = unit.capture_micros();
    let first_until = capture_micros
        .checked_add(decoder_startup::MAX_STARTUP_US)
        .ok_or_else(expired)?
        .min(session.startup_until_us);
    if session.check()? >= first_until {
        return Err(expired());
    }
    let mut hevc = HevcGuard::new(configuration.codec().map_err(|_| failed())?, limits, 4)
        .map_err(|_| failed())?;
    hevc.validate_length_prefixed(unit.bytes(), true)
        .map_err(|_| failed())?;
    let record = hevc.decoder_record().map_err(|_| failed())?;
    let codec = decoder_startup::codec_identifier(&record).map_err(|_| failed())?;
    let declaration =
        decoder_startup::declaration(configuration, &codec, &record).map_err(|_| failed())?;
    let mut bytes = vec![0; decoder::CONFIGURATION_OVERHEAD + codec.len() + record.bytes().len()];
    let length = decoder::encode(
        decoder::Message::Configuration(declaration),
        binding,
        &limits,
        &mut bytes,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
    .map_err(|_| failed())?;
    write.until_us = Some(first_until);
    websocket::write_server_binary(&mut write, &bytes[..length]).await?;
    // Release the redundant serialized configuration before allocating the one
    // reusable media-record buffer. The guard retains only bounded parameter sets.
    drop(bytes);
    read.until_us = Some(first_until);
    let configured = receive_binary(&mut read, decoder::FIRST_DECODED_BYTES).await?;
    if decoder::decode(
        &configured,
        binding,
        &limits,
        Direction::ViewerToHost,
        Delivery::Reliable,
    )
    .map_err(|_| failed())?
        != decoder::Message::Configured
    {
        return Err(failed());
    }
    let mut packet = vec![0; record_bytes];
    admit_update(&session, &first, true).await?;
    sender.enqueue_capture(first).map_err(|_| failed())?;
    drain(&mut write, &mut sender, &mut packet).await?;
    let decoded = receive_binary(&mut read, decoder::FIRST_DECODED_BYTES).await?;
    let decoder::Message::FirstDecoded { frame, .. } = decoder::decode(
        &decoded,
        binding,
        &limits,
        Direction::ViewerToHost,
        Delivery::Reliable,
    )
    .map_err(|_| failed())?
    else {
        return Err(failed());
    };
    // decoder_micros belongs to the viewer's clock; it cannot be compared with
    // source time or used to extend this original capture-anchored deadline.
    if frame != first_frame || session.check()? >= first_until {
        return Err(expired());
    }
    session.state.lock().map_err(|_| failed())?.running = true;
    read.until_us = None;
    write.until_us = None;

    let mut receive = pin!(watch_video(read));
    let mut publish = pin!(publish(
        &session,
        &mut source,
        configuration,
        &mut hevc,
        &mut sender,
        &mut write,
        &mut packet
    ));
    poll_fn(|task| {
        if let Err(error) = session.poll_check(task, 1) {
            return Poll::Ready(Err(error));
        }
        if let Poll::Ready(result) = receive.as_mut().poll(task) {
            return Poll::Ready(result);
        }
        publish.as_mut().poll(task)
    })
    .await
}

async fn receive_binary<R: AsyncRead + Unpin>(read: &mut R, maximum: usize) -> io::Result<Vec<u8>> {
    match websocket::read_ws_frame(read, maximum, true)
        .await
        .map_err(|_| failed())?
    {
        websocket::WsFrame::Binary(bytes) => Ok(bytes),
        _ => Err(failed()),
    }
}
async fn watch_video<R: AsyncRead + Unpin>(mut read: GuardedIo<R>) -> io::Result<()> {
    // No receiver input, repairs, generation replacement or later decoder
    // acknowledgments are negotiated on this fixed view-only media connection.
    match websocket::read_ws_frame(&mut read, decoder::FIRST_DECODED_BYTES, true)
        .await
        .map_err(|_| failed())?
    {
        websocket::WsFrame::Close => Ok(()),
        _ => Err(failed()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish<W: AsyncWrite + Unpin>(
    session: &Session,
    source: &mut CaptureSource,
    configuration: Configuration,
    hevc: &mut HevcGuard,
    sender: &mut Subscription,
    write: &mut GuardedIo<W>,
    packet: &mut [u8],
) -> io::Result<()> {
    let interval = 1_000_000_u64.div_ceil(u64::from(configuration.fps));
    let mut next = session.check()?.checked_add(interval).ok_or_else(expired)?;
    let mut topology_at = session
        .check()?
        .checked_add(1_000_000)
        .ok_or_else(expired)?;
    loop {
        let now = session.check()?;
        sender.tick().map_err(|_| failed())?;
        // Retain source allocation credit until its exact output moves into the
        // sender. No second capture can race this original worker/subscription.
        if now < next || !sender.stream_credit(configuration.max_access_unit_bytes as usize) {
            session.pause(5_000).await?;
            continue;
        }
        if now >= topology_at {
            source
                .check_selected_display(&session.control)
                .await
                .map_err(|_| failed())?;
            topology_at = session
                .check()?
                .checked_add(1_000_000)
                .ok_or_else(expired)?;
        }
        let update = source
            .capture_if_changed(&session.control, false)
            .await
            .map_err(|_| failed())?;
        if let Some(unit) = update.encoded() {
            if unit.config_generation() != configuration.generation {
                return Err(failed());
            }
            hevc.validate_length_prefixed(unit.bytes(), unit.is_idr())
                .map_err(|_| failed())?;
            // The original guard also rejects changed parameter sets and an
            // impossible reference history before the sender accepts this AU.
        }
        admit_update(session, &update, false).await?;
        // This preserves unforgeable source identity on unchanged observations
        // and never converts skipped captures into freshness evidence.
        sender.enqueue_capture(update).map_err(|_| failed())?;
        drain(write, sender, packet).await?;
        next = session.check()?.checked_add(interval).ok_or_else(expired)?;
    }
}
async fn admit_update(session: &Session, update: &CaptureUpdate, first: bool) -> io::Result<()> {
    let Some(unit) = update.encoded() else {
        return Ok(());
    };
    let length = u32::try_from(unit.bytes().len()).map_err(|_| failed())?;
    let charge = browser::frame_charge(length).map_err(|_| failed())?;
    let policy = decoder_startup::host_send_policy();
    let horizon = if first {
        policy.recovery_horizon_micros
    } else {
        policy.reference_horizon_micros
    };
    let until = unit
        .capture_micros()
        .checked_add(horizon)
        .ok_or_else(expired)?;
    loop {
        let now = session.check()?;
        if now < unit.capture_micros() || now >= until {
            return Err(expired());
        }
        {
            let mut state = session.state.lock().map_err(|_| failed())?;
            if state.credit.can_send(charge) {
                state.credit.try_reserve(charge).map_err(|_| failed())?;
                return Ok(());
            }
        }
        session.pause(5_000).await?;
    }
}
async fn drain<W: AsyncWrite + Unpin>(
    write: &mut GuardedIo<W>,
    sender: &mut Subscription,
    bytes: &mut [u8],
) -> io::Result<()> {
    while let Some(offer) = sender.next_packet(bytes).map_err(|_| failed())? {
        // Re-check canonical SendCache ownership/epoch/dependency/absolute age
        // on EVERY resumed TLS write, in addition to both peer/observation gates.
        write.until_us = Some(offer.send_by_micros());
        let session = Arc::clone(write.session.as_ref().ok_or_else(failed)?);
        let custody = session.begin_write(1)?;
        let mut final_owner = PacketIo {
            stream: &mut *write,
            sender: &mut *sender,
            offer: &offer,
        };
        websocket::write_server_binary(&mut final_owner, &bytes[..offer.byte_len()]).await?;
        custody.flushed();
    }
    write.until_us = None;
    Ok(())
}
struct PacketIo<'a, W> {
    stream: &'a mut W,
    sender: &'a mut Subscription,
    offer: &'a PacketOffer,
}
impl<W: AsyncWrite + Unpin> AsyncWrite for PacketIo<'_, W> {
    fn poll_write(
        self: Pin<&mut Self>,
        task: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.sender.authorize_write(this.offer).is_err() {
            return Poll::Ready(Err(failed()));
        }
        Pin::new(&mut *this.stream).poll_write(task, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.sender.authorize_write(this.offer).is_err() {
            return Poll::Ready(Err(failed()));
        }
        Pin::new(&mut *this.stream).poll_flush(task)
    }
    fn poll_shutdown(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.sender.authorize_write(this.offer).is_err() {
            return Poll::Ready(Err(failed()));
        }
        Pin::new(&mut *this.stream).poll_shutdown(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{runtime::RuntimeBuilder, types::Budget};
    use fr_core::{
        authority::{AuthorityPolicy, SessionAuthority},
        ids::CodecConfigurationGeneration,
    };
    use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};

    fn sender(control: ObservationControl) -> Subscription {
        let limits = browser::limits();
        let media = MediaLimits::new(
            limits,
            browser::MAX_VIDEO_RECORD_BYTES,
            MAX_FRAGMENTS,
            MAX_REPAIR_RANGES,
        )
        .unwrap();
        let mut policy = decoder_startup::host_send_policy();
        policy.max_cached_bytes = limits.per_viewer_compressed_bytes() as usize;
        Subscription::new(
            control,
            media,
            MediaBindings::browser_wss(2).unwrap(),
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            policy,
        )
        .unwrap()
    }
    fn control(cx: Cx) -> ObservationControl {
        let mut authority = SessionAuthority::new(
            RemoteSessionId::from_raw(91),
            AuthorityPolicy::plan_defaults(),
        );
        authority.mark_capabilities_checked().unwrap();
        authority
            .authorize_observation(host_now(&cx).unwrap())
            .unwrap();
        ObservationControl::new(cx, authority).unwrap()
    }
    fn unit(control: &ObservationControl, predicted: bool) -> EncodedAccessUnit {
        // A packetizer fixture only. No claim that these opaque bytes pass
        // HEVC admission, invoke native capture, or reach a browser decoder.
        EncodedAccessUnit::new(
            &browser::limits(),
            FrameId::from_raw(u64::from(predicted)),
            if predicted {
                FrameKind::Predicted {
                    references: FrameId::FIRST,
                }
            } else {
                FrameKind::Idr {
                    recovery: RecoveryGeneration::INITIAL,
                }
            },
            CodecConfigurationGeneration::INITIAL,
            control.check().unwrap().as_micros(),
            vec![7; 128],
        )
        .unwrap()
    }
    struct PendingWriter {
        calls: usize,
    }
    impl AsyncWrite for PendingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().calls += 1;
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn resumed_media_write_rechecks_original_authority_before_touching_transport() {
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .blocking_threads(1, 2)
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let observed = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let original = control(observed);
            let mut sender = sender(original.clone());
            sender.enqueue(unit(&original, false)).unwrap();
            let mut record = vec![0; browser::MAX_VIDEO_RECORD_BYTES];
            let offer = sender.next_packet(&mut record).unwrap().unwrap();
            let mut sink = PendingWriter { calls: 0 };
            {
                let mut gate = PacketIo {
                    stream: &mut sink,
                    sender: &mut sender,
                    offer: &offer,
                };
                let mut task = Context::from_waker(Waker::noop());
                assert!(
                    Pin::new(&mut gate)
                        .poll_write(&mut task, &record[..offer.byte_len()])
                        .is_pending()
                );
                original.revoke();
                assert!(matches!(
                    Pin::new(&mut gate).poll_write(&mut task, &record[..offer.byte_len()]),
                    Poll::Ready(Err(_))
                ));
            }
            assert_eq!(sink.calls, 1);
        });
    }

    #[test]
    fn wss_sender_announces_each_picture_before_same_binding_payload() {
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .blocking_threads(1, 2)
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let observed = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let original = control(observed);
            let mut sender = sender(original.clone());
            let media = MediaLimits::new(
                browser::limits(),
                browser::MAX_VIDEO_RECORD_BYTES,
                MAX_FRAGMENTS,
                MAX_REPAIR_RANGES,
            )
            .unwrap();
            let mut bytes = vec![0; browser::MAX_VIDEO_RECORD_BYTES];
            for predicted in [false, true] {
                sender.enqueue(unit(&original, predicted)).unwrap();
                let progress = sender.next_packet(&mut bytes).unwrap().unwrap();
                assert_eq!(progress.channel(), fr_wire::Channel::MediaConfig);
                let parsed = fr_wire::Record::decode(
                    &bytes[..progress.byte_len()],
                    &media,
                    2,
                    fr_wire::Channel::MediaConfig,
                )
                .unwrap();
                let progress = fr_wire::decode_progress(parsed, &media).unwrap();
                assert_eq!(progress.descriptor.frame, u64::from(predicted));
                assert_eq!(progress.descriptor.reference, predicted.then_some(0));
                let payload = sender.next_packet(&mut bytes).unwrap().unwrap();
                assert_eq!(
                    payload.channel(),
                    if predicted {
                        fr_wire::Channel::Video
                    } else {
                        fr_wire::Channel::Recovery
                    }
                );
                let parsed = fr_wire::Record::decode(
                    &bytes[..payload.byte_len()],
                    &media,
                    2,
                    payload.channel(),
                )
                .unwrap();
                assert_eq!(
                    parsed.kind(),
                    if predicted {
                        fr_wire::Kind::Fragment
                    } else {
                        fr_wire::Kind::Recovery
                    }
                );
                assert!(sender.next_packet(&mut bytes).unwrap().is_none());
            }
        });
    }
}
