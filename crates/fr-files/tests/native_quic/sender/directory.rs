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
                1 => fs::create_dir(source.join("empty-child")).unwrap(),
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
        for change_tree in [false, true] {
            let cx = Cx::current().unwrap();
            let mut f = Running::new(&cx).await;
            let source = f.path.join("source");
            fs::create_dir(&source).unwrap();
            fs::write(source.join("file"), vec![1; 20_000]).unwrap();
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
            if change_tree {
                fs::write(source.join("added"), b"new").unwrap();
            } else {
                fs::write(source.join("file"), vec![2; 20_000]).unwrap();
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
