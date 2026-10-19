//! The independent session cleanup context must preserve the control transport.
use super::*;
use asupersync::types::{Budget, CancelKind};

#[test]
fn cancelled_disk_owner_retires_on_live_cleanup_context_without_closing_parent() {
    let runtime = runtime();
    let operation = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Running::new(&operation).await;
        begin(&mut f, &operation, b"complete").await;
        chunks(&mut f, &operation, b"part").await;
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 1);
        operation.cancel_fast(CancelKind::User);
        assert!(operation.is_cancel_requested());
        assert!(cleanup.checkpoint().is_ok());
        f.host.retire_on(&mut f.link.h, &cleanup).unwrap();
        assert_eq!(f.host.state(), State::Retired);
        clean(&mut f);
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 0);
        f.link.h.tick(&cleanup, || true).unwrap();
        f.link.c.tick(&cleanup, || true).unwrap();
        f.input_live(&cleanup);
        assert!(!f.link.h.is_closed());
        assert!(!f.link.c.is_closed());
    });
}
