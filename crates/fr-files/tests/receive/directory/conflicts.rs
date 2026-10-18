//! Whole-tree publication: never merge into or replace an existing destination.
use super::*;
use fr_files::receive::ConflictPolicy;
use std::sync::{Arc, Barrier};

fn conflict_directories(scratch: &Scratch) -> Vec<PathBuf> {
    let mut entries: Vec<_> = fs::read_dir(&scratch.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains(".fr-conflict-")
        })
        .collect();
    entries.sort();
    entries
}

#[test]
fn keep_both_publishes_the_whole_tree_without_merging_or_replacing() {
    let scratch = Scratch::new();
    let root = scratch
        .open()
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    fs::create_dir(scratch.0.join("project")).unwrap();
    fs::write(scratch.0.join("project/precious"), b"original").unwrap();
    let mut pending = root
        .begin_directory(manifest(&[("src/a", b"abc"), ("src/deep/empty", b"")]))
        .unwrap();
    pending.write_entry(0, 0, b"abc").unwrap();
    pending.verify().unwrap();
    assert_eq!(conflict_directories(&scratch), Vec::<PathBuf>::new());
    assert_eq!(pending.publish().unwrap(), Publication::Durable);
    assert_eq!(
        fs::read(scratch.0.join("project/precious")).unwrap(),
        b"original"
    );
    assert_eq!(fs::read_dir(scratch.0.join("project")).unwrap().count(), 1);
    let copies = conflict_directories(&scratch);
    assert_eq!(copies.len(), 1);
    assert_eq!(fs::read(copies[0].join("src/a")).unwrap(), b"abc");
    assert_eq!(fs::read(copies[0].join("src/deep/empty")).unwrap(), b"");
    assert!(!copies[0].join("precious").exists());
    assert_eq!(scratch.entries(), 2);
}

#[test]
fn keep_both_preserves_file_directory_symlink_and_hardlink_arrivals() {
    for kind in 0..4 {
        let scratch = Scratch::new();
        let outside = Scratch::new();
        fs::write(outside.0.join("precious"), b"original").unwrap();
        let mut pending = scratch
            .open()
            .with_conflict_policy(ConflictPolicy::KeepBoth)
            .begin_directory(manifest(&[("a", b"abc")]))
            .unwrap();
        pending.write_entry(0, 0, b"abc").unwrap();
        pending.verify().unwrap();
        let dest = scratch.0.join("project");
        // Occupy the destination only AFTER content and the ATP tree root were verified.
        match kind {
            0 => {
                fs::create_dir(&dest).unwrap();
                fs::write(dest.join("precious"), b"original").unwrap();
            }
            1 => fs::write(&dest, b"original").unwrap(),
            2 => symlink("missing", &dest).unwrap(),
            _ => fs::hard_link(outside.0.join("precious"), &dest).unwrap(),
        }
        pending.publish().unwrap();
        match kind {
            0 => assert_eq!(fs::read(dest.join("precious")).unwrap(), b"original"),
            2 => assert_eq!(fs::read_link(dest).unwrap(), PathBuf::from("missing")),
            _ => assert_eq!(fs::read(dest).unwrap(), b"original"),
        }
        assert_eq!(fs::read(outside.0.join("precious")).unwrap(), b"original");
        let copies = conflict_directories(&scratch);
        assert_eq!(copies.len(), 1);
        assert_eq!(fs::read(copies[0].join("a")).unwrap(), b"abc");
        assert_eq!(scratch.entries(), 2);
    }
}

#[test]
fn concurrent_duplicate_trees_publish_as_separate_complete_objects() {
    const COUNT: u32 = 8;
    let scratch = Scratch::new();
    let root = scratch
        .open_with_limits(Limits {
            max_file_bytes: 1024,
            max_reserved_bytes: 8192,
            max_transfers: COUNT,
        })
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    let mut jobs = Vec::new();
    for id in 0..COUNT {
        let bytes = id.to_be_bytes();
        let mut pending = root
            .begin_directory(manifest(&[("src/id", &bytes), ("empty", b"")]))
            .unwrap();
        pending.write_entry(0, 0, &bytes).unwrap();
        pending.verify().unwrap();
        jobs.push(pending);
    }
    assert!(!scratch.0.join("project").exists());
    assert_eq!(conflict_directories(&scratch), Vec::<PathBuf>::new());
    let gate = Arc::new(Barrier::new(COUNT as usize));
    let threads: Vec<_> = jobs
        .into_iter()
        .map(|pending| {
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                pending.publish()
            })
        })
        .collect();
    // Join EVERY publisher before any assertion can unwind and remove scratch.
    let results: Vec<_> = threads
        .into_iter()
        .map(std::thread::JoinHandle::join)
        .collect();
    for result in results {
        assert_eq!(result.unwrap().unwrap(), Publication::Durable);
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir(&scratch.0).unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.file_name().to_str().unwrap().starts_with(".fr-part-"));
        let id = fs::read(entry.path().join("src/id")).unwrap();
        ids.push(u32::from_be_bytes(id.try_into().unwrap()));
        assert_eq!(fs::read(entry.path().join("empty")).unwrap(), b"");
    }
    ids.sort_unstable();
    assert_eq!(ids, (0..COUNT).collect::<Vec<_>>());
    assert_eq!(conflict_directories(&scratch).len(), COUNT as usize - 1);
}

#[test]
fn corrupt_or_unverified_keep_both_trees_never_acquire_a_conflict_name() {
    let scratch = Scratch::new();
    let root = scratch
        .open()
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    fs::write(scratch.0.join("project"), b"original").unwrap();
    for stage in 0..3 {
        let mut pending = root.begin_directory(manifest(&[("a", b"abc")])).unwrap();
        match stage {
            0 => {}
            1 => assert_eq!(pending.verify(), Err(Error::Incomplete)),
            _ => {
                pending.write_entry(0, 0, b"bad").unwrap();
                assert_eq!(pending.verify(), Err(Error::Integrity));
            }
        }
        assert_eq!(pending.publish(), Err(Error::Retired));
        assert_eq!(conflict_directories(&scratch), Vec::<PathBuf>::new());
        assert_eq!(scratch.entries(), 1);
        assert_eq!(fs::read(scratch.0.join("project")).unwrap(), b"original");
    }
    root.begin_directory(manifest(&[]))
        .unwrap()
        .cancel()
        .unwrap();
    assert_eq!(scratch.entries(), 1);
}
