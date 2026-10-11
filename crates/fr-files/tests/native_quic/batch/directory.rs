//! Mixed selections use one authenticated lane, source slot and receipt domain.
use super::*;
use std::os::unix::fs::symlink;

#[test]
fn mixed_file_and_directory_batch_publishes_pinned_trees_in_selection_order() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let file = f.path.join("selected-file");
        let tree = f.path.join("selected-tree");
        let empty = f.path.join("selected-empty");
        fs::write(&file, b"single").unwrap();
        fs::create_dir_all(tree.join("nested")).unwrap();
        fs::create_dir(&empty).unwrap();
        let data = vec![0x39; 80_007];
        fs::write(tree.join("nested/data"), &data).unwrap();
        fs::write(tree.join("zero"), []).unwrap();
        let mut chosen = selection(&file, &["first"]);
        chosen
            .push_directory(File::open(&tree).unwrap(), "tree")
            .unwrap();
        chosen
            .push_directory(File::open(&empty).unwrap(), "empty")
            .unwrap();
        chosen.push(File::open(&file).unwrap(), "last").unwrap();
        // No reopening a queued tree via its old path after another file runs.
        fs::rename(&tree, f.path.join("original-tree")).unwrap();
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("substitute"), b"not selected").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(&f.link.c, chosen, Duration::from_secs(2))
            .unwrap();
        assert_eq!(sender.batch_report().unwrap().started, 0);
        let report = finish(&mut sender, &mut f.host, &mut f.link, &cx).await;
        assert_eq!(
            (report.total, report.started, report.not_started()),
            (4, 4, 0)
        );
        assert_eq!(report.stop, None);
        for (index, (receipt, bytes)) in report.receipts().zip([6, 80_007, 0, 6]).enumerate() {
            assert_eq!(receipt.id, index as u64 + 1);
            assert_eq!(
                receipt.outcome,
                Outcome::HostPublished {
                    bytes,
                    publication: Publication::Durable,
                }
            );
        }
        assert_eq!(fs::read(f.path.join("first")).unwrap(), b"single");
        assert_eq!(fs::read(f.path.join("last")).unwrap(), b"single");
        assert_eq!(fs::read(f.path.join("tree/nested/data")).unwrap(), data);
        assert_eq!(fs::read(f.path.join("tree/zero")).unwrap(), [] as [u8; 0]);
        assert!(!f.path.join("tree/substitute").exists());
        assert!(f.path.join("empty").is_dir());
        assert_eq!(fs::read_dir(f.path.join("empty")).unwrap().count(), 0);
        assert_eq!(sender.take_batch_report(), Some(report));
        assert_eq!(sender.stage(), Stage::Idle);
        drop(sender);
        f.input_live(&cx);
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}

#[test]
fn unsafe_queued_tree_stops_the_batch_without_touching_earlier_publication() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let file = f.path.join("selected-file");
        let tree = f.path.join("selected-tree");
        fs::write(&file, b"keep").unwrap();
        fs::create_dir(&tree).unwrap();
        symlink(&file, tree.join("escape")).unwrap();
        let mut chosen = selection(&file, &["first"]);
        chosen
            .push_directory(File::open(&tree).unwrap(), "unsafe")
            .unwrap();
        chosen.push(File::open(&file).unwrap(), "never").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(&f.link.c, chosen, Duration::from_secs(2))
            .unwrap();
        let report = finish(&mut sender, &mut f.host, &mut f.link, &cx).await;
        assert_eq!(
            (
                report.started,
                report.not_started(),
                report.receipts().len()
            ),
            (2, 1, 2)
        );
        assert_eq!(report.stop, Some(Stop::Local(Error::Source)));
        let mut receipts = report.receipts();
        assert!(matches!(
            receipts.next().unwrap().outcome,
            Outcome::HostPublished {
                bytes: 4,
                publication: Publication::Durable
            }
        ));
        assert_eq!(
            receipts.next().unwrap().outcome,
            Outcome::InterruptedBeforePublication(Error::Source)
        );
        assert_eq!(fs::read(f.path.join("first")).unwrap(), b"keep");
        assert!(!f.path.join("unsafe").exists());
        assert!(!f.path.join("never").exists());
        drop(sender);
        f.input_live(&cx);
    });
}

#[test]
fn directory_selection_shares_bounds_and_cannot_spawn_without_authority() {
    let path = Path::new("/dev/null"); // Deliberately not a tree: queuing does no I/O.
    let mut chosen = selection(path, &["same"]);
    for name in ["same", "../escape", ".fr-part-private", ""] {
        assert_eq!(
            chosen.push_directory(File::open(path).unwrap(), name),
            Err(Error::Name)
        );
        assert_eq!(chosen.len(), 1);
    }
    for index in 1..MAX_FILES {
        chosen
            .push_directory(File::open(path).unwrap(), &format!("tree-{index}"))
            .unwrap();
    }
    assert_eq!(
        chosen.push_directory(File::open(path).unwrap(), "extra"),
        Err(Error::Limits)
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(&f.link.c, chosen, Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            sender.service(&mut f.link.c, || false),
            Err(Error::Cancelled)
        );
        let report = cleanup(&mut sender);
        assert_eq!(
            (
                report.started,
                report.not_started(),
                report.receipts().len()
            ),
            (0, MAX_FILES, 0)
        );
        assert_eq!(report.stop, Some(Stop::Local(Error::Cancelled)));
        assert!(sender.cleanup_finished());
        assert!(f.host.progress().is_none());
        drop(sender);
        f.input_live(&cx);
    });
}
