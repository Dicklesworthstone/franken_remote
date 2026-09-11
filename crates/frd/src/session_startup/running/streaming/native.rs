//! Explicit native lane: full production startup, actual software HEVC, process
//! supervision, persistent sessions, UDP/TLS and X11 pixel readback. Only initial
//! tailnet identity/approval use the existing private session test fixture.
use super::{tests::*, *};
use crate::media::decoder_startup;
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

struct Display {
    server: Child,
    helper: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    name: String,
}
impl Display {
    fn start() -> Self {
        let mut server = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut number = String::new();
        BufReader::new(server.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        let name = format!(":{}", number.trim().parse::<u32>().unwrap());
        let mut helper = Command::new("python3")
            .args(["-u", "-c", include_str!("x11_fixture.py"), &name])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = helper.stdin.take().unwrap();
        let output = BufReader::new(helper.stdout.take().unwrap());
        let mut this = Self {
            server,
            helper,
            input,
            output,
            name,
        };
        assert_eq!(this.line(), "ready");
        this
    }
    fn line(&mut self) -> String {
        let mut line = String::new();
        self.output.by_ref().take(32).read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "native pixel fixture closed");
        line.trim().to_owned()
    }
    fn paint(&mut self, pixel: u32) {
        writeln!(self.input, "paint {pixel}").unwrap();
        self.input.flush().unwrap();
        assert_eq!(self.line(), "painted");
    }
    fn pixel(&mut self) -> u32 {
        writeln!(self.input, "read").unwrap();
        self.input.flush().unwrap();
        self.line().parse().unwrap()
    }
    fn assert_pixel(&mut self, expected: u32) {
        let actual = self.pixel();
        for shift in [0, 8, 16] {
            assert!(
                ((actual >> shift) & 255).abs_diff((expected >> shift) & 255) <= 8,
                "actual X11 pixel {actual:06x} != {expected:06x}"
            );
        }
    }
    fn launch(&self, binary: &Path, role: Role, epoch: u128) -> Launch {
        Launch::new(binary, &self.name, None, role, epoch).unwrap()
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.helper.kill();
        let _ = self.helper.wait();
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}
async fn turn(host: &mut HostSession, viewer: &mut ViewerSession, n: &mut u128) {
    let (a, b) = Box::pin(support::both(
        host.drive(Duration::from_millis(1), || nonce(n), block),
        viewer.drive(Duration::from_millis(1), block),
    ))
    .await;
    a.unwrap();
    b.unwrap();
}

struct NativePair {
    streaming: StreamingHost,
    viewer: ViewerSession,
    v_media: NegotiatedMedia,
    decoder: decoder_startup::Viewer,
    control: ObservationControl,
    source_display: Display,
    target_display: Display,
    initial_frame: fr_media::access_unit::FrameId,
    source_pid: u32,
    n: u128,
}
#[allow(clippy::too_many_lines)]
async fn native_pair(c: &Cx, h: &Cx, image: &Path, target_image: &Path) -> NativePair {
    use crate::session_startup::running::controlled::tests::attach;
    let mut source_display = Display::start();
    let mut target_display = Display::start();
    let (mut host, mut viewer) = pair_initialized(c, h, capabilities(), |_| {}).await;
    let (hc, vc) = attach(&mut host, &mut viewer, c, h, MediaRole::Configuration, 18).await;
    let (hr, vr) = attach(&mut host, &mut viewer, c, h, MediaRole::Recovery, 19).await;
    let (hv, vv) = attach(&mut host, &mut viewer, c, h, MediaRole::Video, 20).await;
    let selection = host.selection().clone();
    let h_media = NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap();
    let v_media = NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv).unwrap();
    let control = host.observation().unwrap();
    let mut source = CaptureSource::start(
        &control,
        source_display.launch(image, Role::Capture, 41),
        configuration(),
    )
    .await
    .unwrap();
    let first_color = 0x0050_3060;
    source_display.paint(first_color);
    let initial = source.capture_if_changed(&control, true).await.unwrap();
    assert!(initial.encoded().unwrap().is_idr());
    let initial_frame = initial.frame();
    let setup = h_media
        .decoder_setup(host.io().unwrap().0, Duration::from_secs(2))
        .unwrap();
    let mut startup = decoder_startup::Host::new(
        control.clone(),
        host.io().unwrap().0,
        setup,
        configuration(),
        initial,
    )
    .unwrap();
    let config_route = vc.completed_on(viewer.io().unwrap().0).unwrap().inbound;
    let mut n = 3000;
    let mut submitted = false;
    let bytes = loop {
        if !submitted {
            submitted = startup.transmit(host.io().unwrap().0).unwrap();
        }
        turn(&mut host, &mut viewer, &mut n).await;
        let mut result = None;
        viewer
            .io()
            .unwrap()
            .0
            .receive_ready(
                c,
                || true,
                |r| r == Route::Stream(config_route),
                |_, b| {
                    assert!(result.is_none());
                    result = Some(b.to_vec());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(bytes) = result {
            break bytes;
        }
    };
    let setup = v_media
        .decoder_setup(viewer.io().unwrap().0, Duration::from_secs(2))
        .unwrap();
    let receive = v_media
        .receiver_config(viewer.io().unwrap().0, ReceivePolicy::default())
        .unwrap();
    let mut decoder = decoder_startup::Viewer::start(
        c.clone(),
        viewer.io().unwrap().0,
        setup,
        &bytes,
        target_display.launch(target_image, Role::Present, 42),
        receive,
    )
    .await
    .unwrap();
    loop {
        if decoder.transmit(viewer.io().unwrap().0).unwrap() {
            break;
        }
        turn(&mut host, &mut viewer, &mut n).await;
    }
    let recovery = loop {
        turn(&mut host, &mut viewer, &mut n).await;
        startup.dispatch(host.io().unwrap().0).unwrap();
        if let Some(update) = startup.take_recovery().unwrap() {
            break update;
        }
    };
    let mut sender = h_media
        .sender(host.io().unwrap().0, control.clone(), SendPolicy::default())
        .unwrap();
    sender.enqueue_capture(recovery).unwrap();
    loop {
        sender
            .transmit(h, host.io().unwrap().0, Lane::Original)
            .unwrap();
        turn(&mut host, &mut viewer, &mut n).await;
        v_media
            .receive_ready(
                c,
                viewer.io().unwrap().0,
                || true,
                |channel, b| {
                    decoder.receive_media(channel, b).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(receipt) = decoder.present_first().await.unwrap() {
            assert_eq!(receipt.frame, initial_frame);
            break;
        }
    }
    target_display.assert_pixel(first_color);
    loop {
        if decoder.transmit(viewer.io().unwrap().0).unwrap() {
            break;
        }
        turn(&mut host, &mut viewer, &mut n).await;
    }
    while !startup.is_complete() {
        turn(&mut host, &mut viewer, &mut n).await;
        startup.dispatch(host.io().unwrap().0).unwrap();
    }
    let source_pid = source.worker_id().unwrap();
    let stream = Stream::new(
        startup,
        source,
        sender,
        host.io().unwrap().0,
        Policy::default(),
    )
    .unwrap();
    assert_eq!(stream.worker_id(), Some(source_pid));
    let streaming = host.into_streaming(stream).unwrap();
    NativePair {
        streaming,
        viewer,
        v_media,
        decoder,
        control,
        source_display,
        target_display,
        initial_frame,
        source_pid,
        n,
    }
}

// frd cannot depend on fr-native (the native input adapter already depends on
// frd). CI builds the real worker separately and explicitly runs this lane.
// Preserve the complete wire-to-pixel trace in one sequential integration scenario.
#[allow(clippy::too_many_lines)]
#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb; never treated as a normal unit-test pass"]
fn actual_hevc_streams_after_network_startup_and_stays_idle_between_updates() {
    let image = std::env::var_os("FR_NATIVE_TEST_WORKER").expect("build fr-media-worker first");
    let image = std::path::PathBuf::from(image).canonicalize().unwrap();
    assert!(image.is_file());
    run(|c, h| async move {
        let NativePair {
            mut streaming,
            mut viewer,
            v_media,
            decoder,
            control,
            mut source_display,
            mut target_display,
            initial_frame,
            source_pid,
            mut n,
        } = Box::pin(native_pair(&c, &h, &image, &image)).await;
        let (mut presenter, mut receiver) = decoder.finish().unwrap();
        let stop = control.clone();
        let (result, frame_count) = Box::pin(support::both(
            streaming.serve(|| nonce(&mut n), || None, block),
            async {
                // Each idle interval really reads/compares the source while
                // encoding no dummy video. Seven updates span a renewal boundary.
                let mut count = 0;
                let mut lost = std::collections::BTreeSet::new();
                let mut expected_fragments = 0;
                let mut repairs = 0;
                let mut pending: Option<(Vec<u8>, u64, u64)> = None;
                let mut reply = vec![0; v_media.limits().record_bytes()];
                let route = Route::Stream(v_media.repair_stream(viewer.io().unwrap().0).unwrap());
                for (index, color) in [
                    0x0020_4060,
                    0x0060_4020,
                    0x0020_5050,
                    0x0070_5040,
                    0x0030_6050,
                    0x0060_4050,
                    0x0050_3070,
                ]
                .into_iter()
                .enumerate()
                {
                    source_display.paint(color);
                    let until = now(&c).unwrap() + 700_000;
                    let mut matched = false;
                    while now(&c).unwrap() < until {
                        viewer.drive(Duration::from_millis(1), block).await.unwrap();
                        v_media
                            .receive_ready(
                                &c,
                                viewer.io().unwrap().0,
                                || true,
                                |channel, b| {
                                    if index == 6 && channel == fr_wire::Channel::Video {
                                        let record = fr_wire::Record::decode(
                                            b,
                                            &v_media.limits(),
                                            v_media.bindings().for_channel(channel),
                                            channel,
                                        )
                                        .unwrap();
                                        let fragment =
                                            fr_wire::decode_fragment(record, &v_media.limits())
                                                .unwrap();
                                        expected_fragments =
                                            fragment.descriptor.fragment_count().unwrap();
                                        // Drop the first actual delivery of EVERY fragment;
                                        // only a subsequent repair can fill the whole picture.
                                        if lost.insert(fragment.index) {
                                            return Ok(Disposition::Consumed);
                                        }
                                    }
                                    receiver.receive(channel, b, now(&c).unwrap()).unwrap();
                                    Ok(Disposition::Consumed)
                                },
                            )
                            .unwrap();
                        if index == 6
                            && pending.is_none()
                            && let Some(offer) =
                                receiver.repair_offer(now(&c).unwrap(), &mut reply).unwrap()
                        {
                            pending = Some((
                                reply[..offer.bytes].to_vec(),
                                offer.reference_deadline_us.min(now(&c).unwrap() + 80_000),
                                offer.frame,
                            ));
                        }
                        if pending
                            .as_ref()
                            .is_some_and(|(_, _, frame)| !receiver.repair_needed(*frame))
                        {
                            pending = None;
                        }
                        if let Some((bytes, deadline, _)) = &pending {
                            match viewer
                                .io()
                                .unwrap()
                                .0
                                .send(&c, route, bytes, *deadline, || true)
                            {
                                Ok(()) => {
                                    repairs += 1;
                                    pending = None;
                                }
                                Err(fr_transport::quic::Error::Backpressure) => {}
                                other => panic!("repair not admitted: {other:?}"),
                            }
                        }
                        if let Some(receipt) =
                            presenter.present_next(&c, &mut receiver).await.unwrap()
                        {
                            assert!(receipt.frame.as_raw() > initial_frame.as_raw());
                            target_display.assert_pixel(color);
                            matched = true;
                            count += 1;
                        }
                    }
                    assert!(matched, "real dependent HEVC picture never arrived");
                    target_display.assert_pixel(color);
                    assert!(
                        control.check().is_ok(),
                        "codec work starved observation renewal"
                    );
                }
                assert!(repairs > 0 && !lost.is_empty());
                assert_eq!(lost.len(), usize::try_from(expected_fragments).unwrap());
                stop.revoke();
                count
            },
        ))
        .await;
        assert!(result.is_err());
        assert_eq!(frame_count, 7);
        assert_eq!(streaming.statistics().encoded_updates, 7);
        assert!(streaming.statistics().unchanged_observations >= 20);
        assert!(streaming.statistics().repair_requests > 0);
        assert_eq!(
            streaming.worker_id(),
            Some(source_pid),
            "continuous service replaced the source"
        );
        streaming
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        presenter.abort();
        presenter
            .reap(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

// The proxy delays requests, not completion evidence: the actual native worker
// still owns HEVC decoding and X11 submission. All children share the supervised
// worker process group; cancellation must kill and reap the proxy too.
fn decoder_proxy(image: &Path, delay: u64) -> std::path::PathBuf {
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let file = std::env::temp_dir().join(format!(
        "fr-decoder-proxy-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let script = include_str!("decoder_proxy.py")
        .replace("@IMAGE@", &format!("{:?}", image.to_str().unwrap()))
        .replace("@DELAY@", &delay.to_string());
    std::fs::write(&file, script).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
    file
}
fn worker_image() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var_os("FR_NATIVE_TEST_WORKER").expect("build fr-media-worker first"),
    )
    .canonicalize()
    .unwrap()
}

#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn actual_hevc_streams_continuous_viewer_keeps_decoder_and_renews_during_idle() {
    let image = worker_image();
    run(|c, h| async move {
        let NativePair {
            mut streaming,
            viewer,
            v_media,
            decoder,
            control,
            mut source_display,
            mut target_display,
            initial_frame: _,
            source_pid,
            mut n,
        } = Box::pin(native_pair(&c, &h, &image, &image)).await;
        let decoder_pid = decoder.worker_id();
        let mut receiving = viewer.into_streaming(v_media, decoder).unwrap();
        let stop = receiving.control();
        let colors = [0x0020_4060, 0x0060_4020, 0x0020_5050, 0x0050_3070];
        let mut index = 0;
        let mut pictures = 0;
        let mut until = 0;
        let (host_result, viewer_result) = Box::pin(support::both(
            streaming.serve(|| nonce(&mut n), || None, block),
            receiving.serve(
                |input, event| {
                    assert!(input.is_none());
                    if let Some(event) = event {
                        assert_eq!(event.stage, media::PresentationStage::SubmittedToCompositor);
                        target_display.assert_pixel(colors[index - 1]);
                        pictures += 1;
                    }
                    let stamp = now(&c).unwrap();
                    if stamp >= until {
                        assert_eq!(pictures, index, "idle must not encode extra pictures");
                        assert!(
                            control.check().is_ok(),
                            "native viewer starved observation renewal"
                        );
                        if index == colors.len() {
                            control.revoke();
                            stop.stop();
                            return Ok(());
                        }
                        source_display.paint(colors[index]);
                        index += 1;
                        until = stamp + 850_000;
                    }
                    Ok(())
                },
                |_| {},
                block,
            ),
        ))
        .await;
        assert!(host_result.is_err() && viewer_result.is_err());
        assert_eq!(pictures, colors.len());
        assert_eq!(receiving.statistics().decoded, 4);
        assert!(receiving.statistics().network_turns > 20);
        assert_eq!(receiving.worker_id(), decoder_pid);
        assert_eq!(streaming.worker_id(), Some(source_pid));
        assert_eq!(streaming.statistics().encoded_updates, 4);
        assert!(streaming.statistics().unchanged_observations > 10);
        receiving
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        streaming
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn actual_hevc_streams_slow_decoder_does_not_block_viewer_service() {
    let image = worker_image();
    let proxy = decoder_proxy(&image, 120);
    run(|c, h| async move {
        let NativePair {
            mut streaming,
            viewer,
            v_media,
            decoder,
            control,
            mut source_display,
            mut target_display,
            mut n,
            ..
        } = Box::pin(native_pair(&c, &h, &image, &proxy)).await;
        let mut receiving = viewer.into_streaming(v_media, decoder).unwrap();
        let stop = receiving.control();
        let start = now(&c).unwrap();
        let mut turns = 0;
        let mut completed = false;
        source_display.paint(0x0020_4060);
        let (a, b) = Box::pin(support::both(
            streaming.serve(|| nonce(&mut n), || None, block),
            receiving.serve(
                |_, event| {
                    turns += 1;
                    assert!(
                        now(&c).unwrap() < start + 1_000_000,
                        "slow decoder never completed"
                    );
                    if event.is_some() {
                        assert!(now(&c).unwrap() >= start + 120_000);
                        assert!(turns >= 5, "native await blocked UI/network maintenance");
                        target_display.assert_pixel(0x0020_4060);
                        completed = true;
                        control.revoke();
                        stop.stop();
                    }
                    Ok(())
                },
                |_| {},
                block,
            ),
        ))
        .await;
        assert!(a.is_err() && b.is_err() && completed);
        assert_eq!(receiving.statistics().decoded, 1);
        assert!(receiving.statistics().network_turns >= 5);
        receiving
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        streaming
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn actual_hevc_streams_stalled_viewer_decoder_can_be_stopped_and_reaped() {
    let image = worker_image();
    let proxy = decoder_proxy(&image, 60_000);
    run(|c, h| async move {
        let NativePair {
            mut streaming,
            viewer,
            v_media,
            decoder,
            control,
            mut source_display,
            target_display: _target_display,
            mut n,
            ..
        } = Box::pin(native_pair(&c, &h, &image, &proxy)).await;
        let mut receiving = viewer.into_streaming(v_media, decoder).unwrap();
        let stop = receiving.control();
        let until = now(&c).unwrap() + 80_000;
        let mut turns = 0;
        source_display.paint(0x0020_4060);
        let (a, b) = Box::pin(support::both(
            streaming.serve(|| nonce(&mut n), || None, block),
            receiving.serve(
                |_, event| {
                    assert!(event.is_none());
                    turns += 1;
                    if now(&c).unwrap() >= until {
                        control.revoke();
                        stop.stop();
                    }
                    Ok(())
                },
                |_| {},
                block,
            ),
        ))
        .await;
        assert!(a.is_err() && b.is_err());
        assert!(turns >= 3);
        assert_eq!(receiving.statistics().decoded, 0);
        assert_eq!(receiving.budget_usage().bytes, 0);
        receiving
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        streaming
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn actual_hevc_streams_unpolled_viewer_serve_is_terminal() {
    let image = worker_image();
    run(|c, h| async move {
        let NativePair {
            mut streaming,
            viewer,
            v_media,
            decoder,
            control,
            source_display: _source_display,
            target_display: _target_display,
            ..
        } = Box::pin(native_pair(&c, &h, &image, &image)).await;
        let mut receiving = viewer.into_streaming(v_media, decoder).unwrap();
        drop(receiving.serve(|_, _| panic!("unpolled service called UI"), |_| {}, block));
        assert_eq!(
            receiving.statistics(),
            crate::session_startup::ViewerStatistics::default()
        );
        assert_eq!(receiving.budget_usage().bytes, 0);
        assert!(receiving.serve(|_, _| Ok(()), |_| {}, block).await.is_err());
        receiving
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        control.revoke();
        streaming
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
