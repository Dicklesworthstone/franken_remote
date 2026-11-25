//! Recursive jobs use the same real UDP/TLS and original authority fixtures.
use super::*;

fn trees(path: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|entry| entry.is_dir())
        .collect();
    paths.sort();
    paths
}

#[test]
fn recursive_jobs_publish_atomic_subtrees_and_keep_each_changed_version_in_both_directions() {
    for (upload, duplex) in [(false, false), (true, false), (true, true)] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let source = Directory::new();
            let destination = Directory::new();
            fs::create_dir_all(source.0.join("project/nested/empty")).unwrap();
            fs::write(source.0.join("project/nested/data"), b"first").unwrap();
            fs::write(source.0.join("project/empty-file"), b"").unwrap();
            fs::write(source.0.join("note"), b"ordinary file").unwrap();
            fs::create_dir(destination.0.join("project")).unwrap();
            fs::write(destination.0.join("project/local"), b"local contents").unwrap();
            let mut running = Running::configured_mode(
                &cx,
                &source.0,
                &destination.0,
                (41, true),
                upload,
                duplex,
                true,
            )
            .await;
            running.settled(&cx, 1).await;
            assert_eq!(running.host.status().confirmed_publications, 2);
            assert_eq!(running.host.status().confirmed_conflicts, 1);
            assert_eq!(
                fs::read(destination.0.join("project/local")).unwrap(),
                b"local contents"
            );
            assert_eq!(
                fs::read(destination.0.join("note")).unwrap(),
                b"ordinary file"
            );
            let first = trees(&destination.0)
                .into_iter()
                .find(|path| path.join("nested/data").is_file())
                .unwrap();
            assert_eq!(fs::read(first.join("nested/data")).unwrap(), b"first");
            assert!(first.join("nested/empty").is_dir());
            assert!(first.join("empty-file").is_file());

            // Editing a descendant does not require its ancestor timestamp to
            // move. The next scan must still independently hash the whole tree.
            fs::write(source.0.join("project/nested/data"), b"later").unwrap();
            running.settled(&cx, 2).await;
            assert_eq!(running.host.status().confirmed_publications, 3);
            assert_eq!(running.host.status().confirmed_conflicts, 2);
            let copies = trees(&destination.0);
            assert_eq!(copies.len(), 3);
            assert_eq!(fs::read(first.join("nested/data")).unwrap(), b"first");
            assert!(
                copies
                    .iter()
                    .any(|path| fs::read(path.join("nested/data")).ok().as_deref()
                        == Some(b"later".as_slice()))
            );

            // Independent receiver edits do not cause an acknowledged source
            // version to be replayed under a fresh conflict name.
            fs::write(first.join("nested/data"), b"edited locally").unwrap();
            running.settled(&cx, 3).await;
            assert_eq!(running.host.status().confirmed_publications, 3);
            assert_eq!(trees(&destination.0).len(), 3);
            assert_eq!(
                fs::read(first.join("nested/data")).unwrap(),
                b"edited locally"
            );
            running.retire();
            running.input_live(&cx);
            running.link.control_progress(&cx).await;
        });
    }
}

#[test]
fn changed_descendant_after_scan_refuses_before_any_tree_offer() {
    for upload in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let source = Directory::new();
            let destination = Directory::new();
            fs::create_dir_all(source.0.join("project/nested")).unwrap();
            fs::write(source.0.join("project/nested/file"), b"before").unwrap();
            let mut running = Running::configured_mode(
                &cx,
                &source.0,
                &destination.0,
                (41, true),
                upload,
                false,
                true,
            )
            .await;
            let until = clock(&cx) + 2_000_000;
            while running.host.status().state.phase != SyncPhase::Transferring {
                assert!(clock(&cx) < until);
                let (source, destination) = running.step(&cx).await;
                source.unwrap();
                destination.unwrap();
            }
            assert_eq!(running.sender.stage(), Stage::Idle);
            fs::write(source.0.join("project/nested/file"), b"after!").unwrap();
            loop {
                assert!(clock(&cx) < until);
                let (result, _) = running.step(&cx).await;
                if let Err(error) = result {
                    assert_eq!(
                        error,
                        LiveError::Send(fr_files::sender::Error::SourceChanged)
                    );
                    break;
                }
            }
            assert_eq!(running.host.status().confirmed_publications, 0);
            assert!(running.receiver.last_result().is_none());
            assert!(fs::read_dir(&destination.0).unwrap().next().is_none());
            running.retire();
            running.input_live(&cx);
        });
    }
}

#[test]
fn cancelled_tree_never_becomes_partially_visible_and_keeps_the_published_prefix() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("a-confirmed"), b"confirmed first").unwrap();
        fs::create_dir_all(source.0.join("z-project/nested")).unwrap();
        fs::write(
            source.0.join("z-project/nested/large"),
            vec![0x53; 4 * 1024 * 1024],
        )
        .unwrap();
        let mut running = Running::configured_mode(
            &cx,
            &source.0,
            &destination.0,
            (41, true),
            false,
            false,
            true,
        )
        .await;
        let until = clock(&cx) + 2_000_000;
        while running
            .receiver
            .progress()
            .is_none_or(|p| p.id != 2 || p.staged_bytes == 0)
        {
            assert!(clock(&cx) < until, "tree did not enter private staging");
            let (source, destination) = running.step(&cx).await;
            source.unwrap();
            destination.unwrap();
        }
        assert_eq!(running.host.status().confirmed_publications, 1);
        assert!(!destination.0.join("z-project").exists());
        running.retire();
        assert_eq!(running.host.status().failure, Some(LiveError::Cancelled));
        assert_eq!(running.host.status().confirmed_publications, 1);
        assert_eq!(running.host.status().rounds_settled, 0);
        assert_eq!(
            fs::read(destination.0.join("a-confirmed")).unwrap(),
            b"confirmed first"
        );
        assert_eq!(fs::read_dir(&destination.0).unwrap().count(), 1);
        running.input_live(&cx);
        running.link.control_progress(&cx).await;
    });
}

#[test]
fn local_source_mode_must_match_the_original_negotiated_job_before_scanning() {
    for negotiated_recursive in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let source = Directory::new();
            let destination = Directory::new();
            fs::create_dir(source.0.join("project")).unwrap();
            let mut running = Running::configured_mode(
                &cx,
                &source.0,
                &destination.0,
                (41, negotiated_recursive),
                false,
                false,
                true,
            )
            .await;
            let config = HostConfig::open(&source.0, 41).unwrap();
            let config = if negotiated_recursive {
                config
            } else {
                config.with_recursive()
            };
            running.host = HostJob::new(
                config,
                cx.clone(),
                Authority::from_input(&running.input),
                Permission::new(true),
                &running.link.h,
            )
            .unwrap();
            assert_eq!(
                running
                    .host
                    .service(&mut running.sender, &mut running.link.h, || true),
                Err(LiveError::NotNegotiated),
            );
            assert_eq!(running.host.status().confirmed_publications, 0);
            assert!(running.receiver.last_result().is_none());
            assert!(fs::read_dir(&destination.0).unwrap().next().is_none());
            running.retire();
        });
    }
}

#[test]
fn recursive_attachment_cannot_broaden_flat_destination_consent() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::create_dir(source.0.join("project")).unwrap();
        let mut running = Running::configured_mode(
            &cx,
            &source.0,
            &destination.0,
            (41, true),
            false,
            false,
            true,
        )
        .await;
        let mut flat = ReceiverJob::new(41, running.client.directory()).unwrap();
        assert_eq!(
            flat.service(&mut running.receiver, &mut running.link.c, || true),
            Err(LiveError::NotNegotiated),
        );
        assert!(fs::read_dir(&destination.0).unwrap().next().is_none());
        assert!(running.receiver.last_result().is_none());
        running.retire();
        running.input_live(&cx);
    });
}
