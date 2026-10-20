//! The independent session cleanup context must preserve the control transport.
use super::*;
use asupersync::types::{Budget, CancelKind};
use std::path::{Path, PathBuf};

fn staged_handles(root: &Path) -> Vec<PathBuf> {
    let root = fs::canonicalize(root).unwrap();
    fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| {
            let descriptor = entry.unwrap().path();
            let target = fs::read_link(&descriptor).ok()?;
            (target.parent() == Some(root.as_path())
                && fs::metadata(&descriptor).is_ok_and(|metadata| metadata.is_file()))
            .then_some(descriptor)
        })
        .collect()
}

#[test]
fn cancelled_disk_owner_retires_on_live_cleanup_context_without_closing_parent() {
    let runtime = runtime();
    let operation = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Running::new(&operation).await;
        begin(&mut f, &operation, b"complete").await;
        chunks(&mut f, &operation, b"part").await;
        // Both named staging and O_TMPFILE must retain the real partial inode.
        let staged = staged_handles(&f.path);
        assert_eq!(staged.len(), 1);
        assert_eq!(fs::read(&staged[0]).unwrap(), b"part");
        assert!(!f.path.join("received.bin").exists());
        operation.cancel_fast(CancelKind::User);
        assert!(operation.is_cancel_requested());
        assert!(cleanup.checkpoint().is_ok());
        f.host.retire_on(&mut f.link.h, &cleanup).unwrap();
        assert_eq!(f.host.state(), State::Retired);
        clean(&mut f);
        assert_eq!(f.host.try_finish_cleanup(), Some(Ok(())));
        assert_eq!(staged_handles(&f.path), [] as [PathBuf; 0]);
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 0);
        f.link.h.tick(&cleanup, || true).unwrap();
        f.link.c.tick(&cleanup, || true).unwrap();
        f.input_live(&cleanup);
        assert!(!f.link.h.is_closed());
        assert!(!f.link.c.is_closed());
    });
}
