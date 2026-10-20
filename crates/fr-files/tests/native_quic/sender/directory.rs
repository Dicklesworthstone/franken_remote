use super::*;
use std::os::unix::fs::symlink;

#[test]
fn nested_directory_crosses_real_tls_streams_then_legacy_file_uses_same_lane() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("selected");
        fs::create_dir_all(source.join("src/deep")).unwrap();
        let data: Vec<u8> = (0..120_007u32)
            .map(|n| u8::try_from(n % 251).unwrap())
            .collect();
        fs::write(source.join("README"), b"project").unwrap();
        fs::write(source.join("src/deep/data"), &data).unwrap();
        fs::write(source.join("src/empty"), b"").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        assert_eq!(
            sender
                .begin_directory(&f.link.c, File::open(&source).unwrap(), "delivered")
                .unwrap(),
            1
        );
        let receipt = result(&mut sender, &mut f.host, &mut f.link, &cx).await;
        assert_eq!(
            receipt.outcome,
            Outcome::HostPublished {
                bytes: data.len() as u64 + 7,
                publication: Publication::Durable
            }
        );
        assert_eq!(
            fs::read(f.path.join("delivered/src/deep/data")).unwrap(),
            data
        );
        assert_eq!(
            fs::read(f.path.join("delivered/README")).unwrap(),
            b"project"
        );
        assert_eq!(
            fs::metadata(f.path.join("delivered/src/empty"))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            sender
                .begin(
                    &f.link.c,
                    File::open(source.join("README")).unwrap(),
                    "single"
                )
                .unwrap(),
            2
        );
        assert!(matches!(
            result(&mut sender, &mut f.host, &mut f.link, &cx)
                .await
                .outcome,
            Outcome::HostPublished { bytes: 7, .. }
        ));
        sender.cancel(&mut f.link.c).unwrap();
        drop(sender);
        f.input_live(&cx);
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}
#[test]
fn empty_selected_directory_publishes_zero_file_proof() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("source");
        fs::create_dir(&source).unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_directory(&f.link.c, File::open(&source).unwrap(), "empty")
            .unwrap();
        assert_eq!(
            result(&mut sender, &mut f.host, &mut f.link, &cx)
                .await
                .outcome,
            Outcome::HostPublished {
                bytes: 0,
                publication: Publication::Durable
            }
        );
        assert!(f.path.join("empty").is_dir());
        assert_eq!(fs::read_dir(f.path.join("empty")).unwrap().count(), 0);
    });
}
#[test]
fn nested_empty_leaves_publish_with_files_and_without_any_content_bytes() {
    runtime().block_on(async {
        for content in [false, true] {
            let cx = Cx::current().unwrap();
            let mut f = Running::new(&cx).await;
            let source = f.path.join("source");
            fs::create_dir_all(source.join("a-empty")).unwrap();
            fs::create_dir_all(source.join("branch/deep/empty")).unwrap();
            fs::create_dir_all(source.join("z-empty")).unwrap();
            if content {
                fs::write(source.join("b-file"), b"payload").unwrap();
                fs::write(source.join("branch/zero-file"), b"").unwrap();
            }
            let mut sender =
                Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
            sender
                .begin_directory(&f.link.c, File::open(&source).unwrap(), "delivered")
                .unwrap();
            assert_eq!(
                result(&mut sender, &mut f.host, &mut f.link, &cx)
                    .await
                    .outcome,
                Outcome::HostPublished {
                    bytes: if content { 7 } else { 0 },
                    publication: Publication::Durable
                }
            );
            for leaf in ["a-empty", "branch/deep/empty", "z-empty"] {
                let published = f.path.join("delivered").join(leaf);
                assert!(published.is_dir());
                assert_eq!(fs::read_dir(published).unwrap().count(), 0);
            }
            if content {
                assert_eq!(
                    fs::read(f.path.join("delivered/b-file")).unwrap(),
                    b"payload"
                );
                assert!(f.path.join("delivered/branch/zero-file").is_file());
            }
            drop(sender);
            f.input_live(&cx);
            assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
        }
    });
}
#[test]
fn structural_tree_never_accepts_a_content_only_profile_downgrade() {
    runtime().block_on(async {
        use asupersync::net::atp::{
            protocol::FrameType,
            transport_common::{StagedEntryReceive, flat_merkle_root_from_digests},
        };
        use fr_wire::files::{self, Body};
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("source");
        fs::create_dir_all(source.join("empty")).unwrap();
        let context = f.client.incoming();
        let limits = f.client.limits();
        let route = f.host_files;
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_directory(&f.link.c, File::open(&source).unwrap(), "remote")
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        while sender.stage() != Stage::AwaitingAcceptance {
            assert!(clock(&cx) < until);
            sender.service(&mut f.link.c, || true).unwrap();
            f.link.drive(&cx).await;
        }
        let (digest, _, _) =
            StagedEntryReceive::new(std::path::PathBuf::new()).finalize("empty".into());
        let request = serde_json::json!({
            "mode": "full_object",
            "sender_merkle_root_hex": flat_merkle_root_from_digests(&[digest]),
            "missing_bytes": 0,
            "shared_chunks": 0,
            "stale_chunks": 0,
            "missing_chunks": [],
            "fallback_reason": "portable_full_object"
        });
        let frame = framed(
            FrameType::ObjectRequest,
            serde_json::to_vec(&request).unwrap(),
        );
        let bytes = encoded(
            Body::Accept {
                profile: files::ATP_PORTABLE_DIRECTORY_FULL,
                size: 0,
                bytes_per_second: 8_000_000,
                chunk_bytes: 100,
                concurrent_transfers: 1,
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
        assert_eq!(sender.progress().unwrap().queued_bytes, 0);
        assert_eq!(
            cleanup(&mut sender).outcome,
            Outcome::InterruptedBeforePublication(Error::Protocol)
        );
        assert!(!f.path.join("remote").exists());
        assert!(f.host.progress().is_none());
        drop(sender);
        f.input_live(&cx);
    });
}
#[test]
fn directory_selection_never_follows_a_replaced_root_path() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"chosen").unwrap();
        let selected = File::open(&source).unwrap();
        fs::rename(&source, f.path.join("old")).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"replacement").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_directory(&f.link.c, selected, "delivered")
            .unwrap();
        assert!(matches!(
            result(&mut sender, &mut f.host, &mut f.link, &cx)
                .await
                .outcome,
            Outcome::HostPublished { bytes: 6, .. }
        ));
        assert_eq!(fs::read(f.path.join("delivered/file")).unwrap(), b"chosen");
    });
}
#[test]
fn unsupported_tree_entries_are_refused_before_an_offer_or_remote_write() {
    runtime().block_on(async {
        for case in 0..6 {
            let cx = Cx::current().unwrap();
            let mut f = Running::new(&cx).await;
            let source = f.path.join("source");
            fs::create_dir(&source).unwrap();
            match case {
                0 => symlink("../outside", source.join("link")).unwrap(),
                1 => {
                    for i in 0..65 {
                        fs::create_dir(source.join(format!("empty-{i}"))).unwrap();
                    }
                }
                2 => {
                    for i in 0..65 {
                        fs::write(source.join(format!("file-{i}")), b"").unwrap();
                    }
                }
                3 => {
                    fs::write(source.join("a"), b"a").unwrap();
                    fs::write(source.join("A"), b"A").unwrap();
                }
                4 => {
                    rustix::fs::mknodat(
                        rustix::fs::CWD,
                        source.join("fifo"),
                        rustix::fs::FileType::Fifo,
                        rustix::fs::Mode::RUSR,
                        0,
                    )
                    .unwrap();
                }
                _ => fs::write(source.join("NUL"), b"a").unwrap(),
            }
            let mut sender =
                Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
            sender
                .begin_directory(&f.link.c, File::open(&source).unwrap(), "remote")
                .unwrap();
            let until = clock(&cx) + 1_000_000;
            while sender.result().is_none() {
                assert!(clock(&cx) < until);
                let _ = sender.service(&mut f.link.c, || true);
                std::thread::yield_now();
            }
            assert!(matches!(
                cleanup(&mut sender).outcome,
                Outcome::InterruptedBeforePublication(Error::Source | Error::Limits | Error::Name)
            ));
            assert!(!f.path.join("remote").exists());
            assert!(f.host.progress().is_none());
            drop(sender);
            f.input_live(&cx);
        }
    });
}
#[test]
fn tree_or_child_mutation_after_manifest_never_publishes_partial_directory() {
    runtime().block_on(async {
        for change in 0..4 {
            let cx = Cx::current().unwrap();
            let mut f = Running::new(&cx).await;
            let source = f.path.join("source");
            fs::create_dir(&source).unwrap();
            fs::write(source.join("file"), vec![1; 20_000]).unwrap();
            if change >= 2 {
                fs::create_dir_all(source.join("nested/empty")).unwrap();
            }
            let mut sender =
                Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
            sender
                .begin_directory(&f.link.c, File::open(&source).unwrap(), "remote")
                .unwrap();
            let until = clock(&cx) + 1_000_000;
            while sender.stage() != Stage::AwaitingAcceptance {
                assert!(clock(&cx) < until);
                sender.service(&mut f.link.c, || true).unwrap();
                f.link.drive(&cx).await;
            }
            match change {
                0 => fs::write(source.join("file"), vec![2; 20_000]).unwrap(),
                1 => fs::write(source.join("added"), b"new").unwrap(),
                2 => fs::write(source.join("nested/empty/added"), b"new").unwrap(),
                _ => {
                    fs::rename(source.join("nested/empty"), source.join("nested/original"))
                        .unwrap();
                    fs::create_dir(source.join("nested/empty")).unwrap();
                }
            }
            while sender.result().is_none() {
                assert!(clock(&cx) < until);
                let _ = sender.service(&mut f.link.c, || true);
                let _ = f.host.service(&mut f.link.h, || true);
                f.link.drive(&cx).await;
            }
            assert_eq!(
                cleanup(&mut sender).outcome,
                Outcome::InterruptedBeforePublication(Error::SourceChanged)
            );
            assert!(!f.path.join("remote").exists());
            drop(sender);
            f.input_live(&cx);
        }
    });
}
#[test]
fn directory_conflict_keeps_both_local_selection_and_existing_remote_tree() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"new").unwrap();
        fs::create_dir(f.path.join("remote")).unwrap();
        fs::write(f.path.join("remote/precious"), b"keep").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_directory(&f.link.c, File::open(&source).unwrap(), "remote")
            .unwrap();
        assert_eq!(
            result(&mut sender, &mut f.host, &mut f.link, &cx)
                .await
                .outcome,
            Outcome::HostRefused(fr_wire::files::Reason::Conflict)
        );
        assert_eq!(fs::read(f.path.join("remote/precious")).unwrap(), b"keep");
        assert!(!f.path.join("remote/file").exists());
        assert_eq!(fs::read(source.join("file")).unwrap(), b"new");
    });
}
#[test]
fn lost_directory_proof_is_unknown_publication_never_safe_to_retry() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"committed").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_directory(&f.link.c, File::open(&source).unwrap(), "remote")
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        while sender.stage() != Stage::AwaitingProof {
            assert!(clock(&cx) < until);
            sender.service(&mut f.link.c, || true).unwrap();
            f.host.service(&mut f.link.h, || true).unwrap();
            f.link.drive(&cx).await;
        }
        while !f.path.join("remote").exists() {
            assert!(clock(&cx) < until);
            f.host.service(&mut f.link.h, || true).unwrap();
            f.link.drive(&cx).await;
        }
        sender.cancel(&mut f.link.c).unwrap();
        assert_eq!(cleanup(&mut sender).outcome, Outcome::PublicationUnknown);
        assert_eq!(fs::read(f.path.join("remote/file")).unwrap(), b"committed");
        drop(sender);
        f.input_live(&cx);
    });
}
