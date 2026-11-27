//! Original folder authority must reach the independent ATP source worker.
//! These cases use real source descriptors and UDP/TLS attachment, then stop
//! servicing the job, sender and network while its source is still owned.
use super::*;
use fr_client::input::StopReason;
use fr_files::sender::Outcome;

#[derive(Clone, Copy)]
enum Fence {
    Job,
    Permission,
    Input,
}

#[test]
fn stopping_a_folder_job_finishes_its_source_without_another_network_turn() {
    for upload in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            fence_source(&cx, upload, Fence::Job, false).await;
        });
    }
}

#[test]
fn revoking_source_consent_finishes_the_worker_without_revoking_input() {
    for upload in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            fence_source(&cx, upload, Fence::Permission, false).await;
        });
    }
}

#[test]
fn stopping_original_input_finishes_the_folder_source_without_network_service() {
    for upload in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            fence_source(&cx, upload, Fence::Input, false).await;
        });
    }
}

#[test]
fn source_consent_revocation_keeps_the_original_published_prefix() {
    for upload in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            fence_source(&cx, upload, Fence::Permission, true).await;
        });
    }
}

async fn fence_source(cx: &Cx, upload: bool, fence: Fence, prefix: bool) {
    let source = Directory::new();
    let destination = Directory::new();
    if prefix {
        fs::write(source.0.join("a-confirmed"), b"confirmed first version").unwrap();
    }
    // A real bounded file keeps independent preparation observable. Once its
    // manifest is prepared, the same source waits for the original sender's
    // first read request; it must observe authority in either state.
    fs::write(source.0.join("z-pending"), vec![0x53; 4 * 1024 * 1024]).unwrap();
    let mut running = if upload {
        Running::upload(cx, &source.0, &destination.0, 41, true, true).await
    } else {
        Running::new(cx, &source.0, &destination.0, 41).await
    };
    let id = if prefix { 2 } else { 1 };
    wait_for_preparation(&mut running, cx, id).await;
    let publications = u32::from(prefix);
    assert_eq!(running.host.status().confirmed_publications, publications);
    assert_eq!(running.client.status().confirmed_publications, publications);
    assert!(!running.sender.cleanup_finished());
    let original_receipt = running.receiver.last_result();
    let original_files = destination.files();
    assert_eq!(original_files.len(), publications as usize);
    assert!(!destination.0.join("z-pending").exists());

    match fence {
        Fence::Job => running.host.stop(),
        Fence::Permission => running.source_permission.revoke(),
        Fence::Input if upload => running._client_input.stop(StopReason::Disconnected),
        Fence::Input => running.input.revoke(),
    }
    // Do not stop/service the Sender or drive either connection: that would
    // hide a missing authority handoff by supplying the network owner's stop.
    // The existing source loop polls its command at most every 10 ms.
    let until = Instant::now() + Duration::from_millis(500);
    while !running.sender.cleanup_finished() {
        assert!(
            Instant::now() < until,
            "the original disk source retained revoked folder authority"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(cx.checkpoint().is_ok(), "the session scope stays live");
    assert_eq!(running.sender.stage(), Stage::Preparing);
    assert_eq!(running.sender.progress().unwrap().id, id);
    assert_eq!(running.sender.progress().unwrap().queued_bytes, 0);
    assert_eq!(running.receiver.last_result(), original_receipt);
    assert_eq!(destination.files(), original_files);
    assert_eq!(running.host.status().confirmed_publications, publications);
    if !matches!(fence, Fence::Input) {
        running.input_live(cx);
        assert!(running._client_input.stopped().is_none());
    }

    // Original cleanup records the interrupted source without losing an
    // earlier confirmed publication or acquiring a replacement file owner.
    running.retire();
    assert!(running.host.status().cleanup_complete);
    assert!(running.client.status().cleanup_complete);
    assert_eq!(running.host.status().confirmed_publications, publications);
    assert_eq!(running.client.status().confirmed_publications, publications);
    assert_eq!(running.host.status().rounds_settled, 0);
    let report = running.sender.batch_report().unwrap();
    assert!(report.complete);
    let started = usize::try_from(id).unwrap();
    assert_eq!(report.started, started);
    assert_eq!(report.receipts().len(), started);
    assert_eq!(
        report
            .receipts()
            .filter(|receipt| matches!(
                receipt.outcome,
                Outcome::HostPublished { .. } | Outcome::ControllerPublished { .. }
            ))
            .count(),
        publications as usize
    );
    assert_eq!(destination.files(), original_files);
    running.link.control_progress(cx).await;
}

async fn wait_for_preparation(running: &mut Running, cx: &Cx, id: u64) {
    let until = clock(cx) + 2_000_000;
    while running.sender.stage() != Stage::Preparing
        || running
            .sender
            .progress()
            .is_none_or(|progress| progress.id != id)
    {
        assert!(clock(cx) < until, "independent source did not start");
        let (source, destination) = running.step(cx).await;
        source.unwrap();
        destination.unwrap();
    }
}
