//! Real sender/receiver composition; no caller-written file records in this path.
use super::native_support::{self, Running, clock, runtime};
use asupersync::cx::Cx;
use fr_files::{
    quic::HostReceiver,
    receive::Publication,
    sender::{Error, Outcome, Policy, Receipt, Sender, Stage},
};
use std::{
    fs::{self, File},
    time::{Duration, Instant},
};

async fn step(
    sender: &mut Sender<'_>,
    host: &mut HostReceiver,
    link: &mut native_support::Link,
    cx: &Cx,
) {
    sender.service(&mut link.c, || true).unwrap();
    host.service(&mut link.h, || true).unwrap();
    link.drive(cx).await;
    host.service(&mut link.h, || true).unwrap();
    sender.service(&mut link.c, || true).unwrap();
}
async fn result(
    sender: &mut Sender<'_>,
    host: &mut HostReceiver,
    link: &mut native_support::Link,
    cx: &Cx,
) -> Receipt {
    let until = clock(cx) + 1_500_000;
    loop {
        assert!(clock(cx) < until, "sender stalled: {:?}", sender.stage());
        step(sender, host, link, cx).await;
        if let Some(result) = sender.take_result() {
            return result;
        }
    }
}
fn cleanup(sender: &mut Sender<'_>) -> Receipt {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        assert!(Instant::now() < until, "source cleanup did not finish");
        if let Some(receipt) = sender.take_result() {
            return receipt;
        }
        std::thread::yield_now();
    }
}
#[test]
fn selected_descriptor_streams_two_files_and_empty_file_with_host_proofs() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("local-source");
        let data: Vec<u8> = (0..120_007)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        fs::write(&path, &data).unwrap();
        let (link, host) = (&mut f.link, &mut f.host);
        let mut sender =
            Sender::new(cx.clone(), &link.c, &mut f.client, Policy::default()).unwrap();
        for (id, name) in [(1, "first.bin"), (2, "second.bin")] {
            assert_eq!(
                sender
                    .begin(&link.c, File::open(&path).unwrap(), name)
                    .unwrap(),
                id
            );
            assert_eq!(
                sender.begin(&link.c, File::open(&path).unwrap(), "third.bin"),
                Err(Error::Busy)
            );
            let receipt = result(&mut sender, host, link, &cx).await;
            assert_eq!(
                receipt,
                Receipt {
                    id,
                    outcome: Outcome::HostPublished {
                        bytes: data.len() as u64,
                        publication: Publication::Durable
                    }
                }
            );
            assert_eq!(fs::read(f.path.join(name)).unwrap(), data);
            assert_eq!(sender.stage(), Stage::Idle);
        }
        fs::write(&path, b"").unwrap();
        assert_eq!(
            sender
                .begin(&link.c, File::open(&path).unwrap(), "empty")
                .unwrap(),
            3
        );
        assert_eq!(
            result(&mut sender, host, link, &cx).await.outcome,
            Outcome::HostPublished {
                bytes: 0,
                publication: Publication::Durable
            }
        );
        sender.cancel(&mut link.c).unwrap();
        drop(sender);
        f.input_live(&cx);
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}
#[test]
fn replacing_selected_path_does_not_redirect_the_source_handle() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"chosen").unwrap();
        let file = File::open(&path).unwrap();
        fs::rename(&path, f.path.join("old-source")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let (link, host) = (&mut f.link, &mut f.host);
        let mut sender =
            Sender::new(cx.clone(), &link.c, &mut f.client, Policy::default()).unwrap();
        sender.begin(&link.c, file, "delivered").unwrap();
        let receipt = result(&mut sender, host, link, &cx).await;
        assert!(matches!(
            receipt.outcome,
            Outcome::HostPublished { bytes: 6, .. }
        ));
        assert_eq!(fs::read(f.path.join("delivered")).unwrap(), b"chosen");
        sender.cancel(&mut link.c).unwrap();
    });
}
#[test]
fn source_mutation_after_manifest_is_not_a_publication_or_automatic_retry() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, vec![1; 20_000]).unwrap();
        let (link, host) = (&mut f.link, &mut f.host);
        let mut sender =
            Sender::new(cx.clone(), &link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&link.c, File::open(&path).unwrap(), "received")
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        while sender.stage() != Stage::AwaitingAcceptance {
            assert!(clock(&cx) < until);
            sender.service(&mut link.c, || true).unwrap();
            link.drive(&cx).await;
        }
        fs::write(&path, vec![2; 20_000]).unwrap();
        loop {
            assert!(clock(&cx) < until);
            let _ = sender.service(&mut link.c, || true);
            let _ = host.service(&mut link.h, || true);
            link.drive(&cx).await;
            if sender.result().is_some() {
                break;
            }
        }
        assert_eq!(
            cleanup(&mut sender).outcome,
            Outcome::InterruptedBeforePublication(Error::SourceChanged)
        );
        assert_eq!(
            sender.begin(&link.c, File::open(&path).unwrap(), "retry"),
            Err(Error::Closed)
        );
        assert!(!f.path.join("received").exists());
        drop(sender);
        f.input_live(&cx);
    });
}
#[test]
fn missing_proof_after_completion_is_unknown_even_when_host_committed() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"committed").unwrap();
        let (link, host) = (&mut f.link, &mut f.host);
        let mut sender =
            Sender::new(cx.clone(), &link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&link.c, File::open(&path).unwrap(), "delivered")
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        while sender.stage() != Stage::AwaitingProof {
            assert!(clock(&cx) < until);
            sender.service(&mut link.c, || true).unwrap();
            host.service(&mut link.h, || true).unwrap();
            link.drive(&cx).await;
        }
        while !f.path.join("delivered").exists() {
            assert!(clock(&cx) < until);
            host.service(&mut link.h, || true).unwrap();
            link.drive(&cx).await;
        }
        // Deliberately do not dispatch the proof to the sender before closing.
        sender.cancel(&mut link.c).unwrap();
        assert_eq!(cleanup(&mut sender).outcome, Outcome::PublicationUnknown);
        assert_eq!(fs::read(f.path.join("delivered")).unwrap(), b"committed");
        drop(sender);
        f.input_live(&cx);
    });
}
#[test]
fn cancellation_before_offer_and_invalid_local_selection_do_not_send_data() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("private-local-file");
        fs::write(&path, b"private-payload").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        for name in ["../escape", "a/b", ".fr-part-source", "NUL"] {
            assert_eq!(
                sender.begin(&f.link.c, File::open(&path).unwrap(), name),
                Err(Error::Name)
            );
        }
        sender
            .begin(&f.link.c, File::open(&path).unwrap(), "remote")
            .unwrap();
        let debug = format!("{sender:?}");
        assert!(!debug.contains("private-local-file"));
        assert!(!debug.contains("private-payload"));
        sender.cancel(&mut f.link.c).unwrap();
        assert_eq!(
            cleanup(&mut sender).outcome,
            Outcome::InterruptedBeforePublication(Error::Cancelled)
        );
        assert!(!f.path.join("remote").exists());
        drop(sender);
        f.input_live(&cx);
    });
}
#[test]
fn oversized_and_nonregular_sources_refuse_on_the_disk_worker() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for directory in [false, true] {
            let mut f = Running::new(&cx).await;
            let path = f.path.join("source");
            fs::write(&path, b"oversized").unwrap();
            let policy = Policy {
                max_file_bytes: 3,
                ..Policy::default()
            };
            let mut sender = Sender::new(cx.clone(), &f.link.c, &mut f.client, policy).unwrap();
            let file = File::open(if directory { &f.path } else { &path }).unwrap();
            sender.begin(&f.link.c, file, "remote").unwrap();
            let until = clock(&cx) + 1_000_000;
            while sender.result().is_none() {
                assert!(clock(&cx) < until);
                let _ = sender.service(&mut f.link.c, || true);
                std::thread::yield_now();
            }
            assert_eq!(
                cleanup(&mut sender).outcome,
                Outcome::InterruptedBeforePublication(Error::Source)
            );
            assert!(!f.path.join("remote").exists());
        }
    });
}

#[test]
fn queued_record_expiry_retires_only_files_and_does_not_refresh_its_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"bounded").unwrap();
        let policy = Policy {
            record_lifetime: Duration::from_millis(10),
            ..Policy::default()
        };
        let mut sender = Sender::new(cx.clone(), &f.link.c, &mut f.client, policy).unwrap();
        sender
            .begin(&f.link.c, File::open(&path).unwrap(), "remote")
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        while sender.stage() != Stage::AwaitingAcceptance {
            assert!(clock(&cx) < until);
            sender.service(&mut f.link.c, || true).unwrap();
            std::thread::yield_now();
        }
        // No native drive: the original queued offer has not reached the peer.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(sender.service(&mut f.link.c, || true), Err(Error::Expired));
        assert_eq!(
            cleanup(&mut sender).outcome,
            Outcome::InterruptedBeforePublication(Error::Expired)
        );
        assert!(f.link.c.tick(&cx, || true).is_ok());
        assert!(!f.link.c.is_closed());
        drop(sender);
        f.input_live(&cx);
    });
}

fn framed(kind: asupersync::net::atp::protocol::FrameType, payload: Vec<u8>) -> Vec<u8> {
    use asupersync::net::atp::protocol::{Frame, ProtocolVersion};
    Frame::new(ProtocolVersion::CURRENT, kind, payload)
        .unwrap()
        .to_wire_bytes()
        .unwrap()
}
fn encoded(
    body: fr_wire::files::Body<'_>,
    context: fr_wire::files::Context,
    limits: fr_wire::files::Limits,
) -> Vec<u8> {
    let mut bytes = vec![0; limits.record_bytes()];
    let n = fr_wire::files::encode(
        fr_wire::files::Message { id: 1, body },
        context,
        limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
#[test]
fn hostile_acceptance_cannot_change_size_root_or_request_profile_before_source_bytes() {
    runtime().block_on(async {
        use fr_wire::files::{self,Body};
        use asupersync::net::atp::protocol::FrameType;
        for variant in 0..4 {
            let cx=Cx::current().unwrap();let mut f=Running::new(&cx).await;
            let path=f.path.join("source");fs::write(&path,b"private").unwrap();
            let expected=native_support::manifest("remote",b"private");
            let ctx=f.client.incoming();let limits=f.client.limits();let route=f.host_files;
            let mut sender=Sender::new(cx.clone(),&f.link.c,&mut f.client,Policy::default()).unwrap();
            sender.begin(&f.link.c,File::open(&path).unwrap(),"remote").unwrap();
            let until=clock(&cx)+1_000_000;
            while sender.stage()!=Stage::AwaitingAcceptance {
                assert!(clock(&cx)<until);sender.service(&mut f.link.c,||true).unwrap();
                f.link.drive(&cx).await;
            }
            let request=serde_json::json!({"mode":if variant==1{"delta"}else{"full_object"},
                "sender_merkle_root_hex":if variant==2{"0".repeat(64)}else{expected.merkle_root_hex},
                "missing_bytes":7,"shared_chunks":0,"stale_chunks":0,"missing_chunks":[],"fallback_reason":"portable_full_object"});
            let frame=framed(if variant==3{FrameType::Proof}else{FrameType::ObjectRequest},serde_json::to_vec(&request).unwrap());
            let bytes=encoded(Body::Accept{profile:files::ATP_PORTABLE_FULL,size:if variant==0{8}else{7},
                bytes_per_second:8_000_000,chunk_bytes:100,concurrent_transfers:1,atp:&frame},ctx,limits);
            f.link.h.send(&cx,fr_transport::quic::Route::Stream(route),&bytes,until,||true).unwrap();
            while sender.result().is_none() {
                assert!(clock(&cx)<until);f.link.drive(&cx).await;
                let _=sender.service(&mut f.link.c,||true);
            }
            assert_eq!(sender.progress().unwrap().queued_bytes,0);
            assert_eq!(cleanup(&mut sender).outcome,Outcome::InterruptedBeforePublication(Error::Protocol));
            assert!(!f.path.join("remote").exists());drop(sender);f.input_live(&cx);
        }
    });
}
#[test]
fn contradictory_success_proof_never_becomes_a_published_receipt() {
    runtime().block_on(async {
        use asupersync::net::atp::{protocol::FrameType, transport_tcp::ReceiveReceipt};
        use fr_wire::files::{Body, Disposition, Reason};
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"content").unwrap();
        let context = f.client.incoming();
        let limits = f.client.limits();
        let route = f.host_files;
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&f.link.c, File::open(&path).unwrap(), "remote")
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        while sender.stage() != Stage::AwaitingProof {
            assert!(clock(&cx) < until);
            sender.service(&mut f.link.c, || true).unwrap();
            f.host.service(&mut f.link.h, || true).unwrap();
            f.link.drive(&cx).await;
        }
        let proof = ReceiveReceipt {
            committed: true,
            bytes_received: 7,
            files: 1,
            sha_ok: false,
            merkle_ok: true,
            symbols_accepted: 0,
            feedback_rounds: 0,
            decode_count: 0,
            decode_micros: 0,
            reason: None,
            committed_paths: vec![],
        };
        let frame = framed(FrameType::Proof, serde_json::to_vec(&proof).unwrap());
        let bytes = encoded(
            Body::Complete {
                disposition: Disposition::PublishedDurable,
                reason: Reason::None,
                published_bytes: 7,
                atp: &frame,
            },
            context,
            limits,
        );
        f.link
            .h
            .send(
                &cx,
                fr_transport::quic::Route::Stream(route),
                &bytes,
                until,
                || true,
            )
            .unwrap();
        while sender.result().is_none() {
            assert!(clock(&cx) < until);
            f.link.drive(&cx).await;
            let _ = sender.service(&mut f.link.c, || true);
        }
        assert_eq!(cleanup(&mut sender).outcome, Outcome::PublicationUnknown);
    });
}
#[test]
fn actual_host_conflict_is_a_refusal_and_cannot_overwrite_or_resubmit() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"incoming").unwrap();
        fs::write(f.path.join("existing"), b"untouched").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&f.link.c, File::open(&path).unwrap(), "existing")
            .unwrap();
        let receipt = result(&mut sender, &mut f.host, &mut f.link, &cx).await;
        assert_eq!(
            receipt.outcome,
            Outcome::HostRefused(fr_wire::files::Reason::Conflict)
        );
        assert_eq!(fs::read(f.path.join("existing")).unwrap(), b"untouched");
        assert_eq!(
            sender.begin(&f.link.c, File::open(&path).unwrap(), "retry"),
            Err(Error::Closed)
        );
    });
}
