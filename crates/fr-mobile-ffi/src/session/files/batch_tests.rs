//! Tests of actual pinned selections and original ATP receipt types.
use super::*;
use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
};

struct Files {
    root: PathBuf,
    paths: Vec<PathBuf>,
}
impl Files {
    fn new(count: usize) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fr-batch-upload-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let paths: Vec<_> = (0..count)
            .map(|index| {
                let path = root.join(format!("selected-{index}.txt"));
                fs::write(&path, vec![b'x'; index + 1]).unwrap();
                path
            })
            .collect();
        Self { root, paths }
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir(&self.root);
    }
}

fn receipt(id: u64, outcome: FileSendOutcome) -> FileSendReceipt {
    FileSendReceipt { id, outcome }
}
fn published(id: u64, bytes: u64) -> FileSendReceipt {
    receipt(
        id,
        FileSendOutcome::HostPublished {
            bytes,
            publication: Publication::Durable,
        },
    )
}

#[test]
fn eight_files_are_one_original_selection_with_checked_total_bytes() {
    let files = Files::new(8);
    let upload = Upload::select_many(&files.paths).unwrap();
    assert_eq!(upload.report().batch, 1);
    assert_eq!(upload.report().selected_files, 8);
    assert_eq!(upload.report().selected_bytes, 36);
    assert!(upload.bind());
    assert!(!Arc::clone(&upload).bind());
    assert!(!upload.ready());
    assert!(!upload.is_success());
    assert_eq!(upload.report().receipt_counts().reported, 0);
    let document = upload.report().batch_document();
    assert_eq!(document["receipts"], serde_json::json!([]));
    assert_eq!(document["published_bytes"], serde_json::Value::Null);
    assert!(!document.to_string().contains("selected-0.txt"));
    upload.configure_with(|request| {
        assert!(request.is_some());
        Err(Absence::NotNegotiated)
    });
    assert!(!upload.available());
    upload.configure_with(|_| panic!("original batch was replayed"));
    assert_eq!(upload.report().selected_files, 8);
    assert_eq!(upload.reports().len(), 1);
}

#[test]
fn the_single_file_wrapper_keeps_the_existing_apple_document_budget() {
    let files = Files::new(1);
    let upload = Upload::select(&files.paths[0]).unwrap();
    assert_eq!(upload.report().selected_files, 1);
    assert_eq!(upload.report().selected_bytes, 1);
    let legacy = upload.report().document();
    assert_eq!(legacy.as_object().unwrap().len(), 16);
    assert!(legacy.get("receipts").is_none());
    assert!(legacy.get("selected_files").is_none());
    assert_eq!(legacy["receipt"], "none");
    assert_eq!(legacy["published_bytes"], serde_json::Value::Null);
    let history = vec![legacy.clone(); MAX_UPLOADS];
    let status = serde_json::json!({
        "version": 1, "available": false, "ready": false,
        "sources_released": false,
        "selection": {"token": 0, "attempt": 0, "state": "none", "selected_bytes": null},
        "transfer": legacy, "history": history,
    });
    assert!(status.to_string().len() < 16_384);
    let batch = upload.report().batch_document();
    assert_eq!(batch["selected_files"], 1);
    assert_eq!(batch["receipt"], legacy["receipt"]);
}

#[test]
fn invalid_or_over_limit_members_refuse_the_whole_selection() {
    let files = Files::new(9);
    assert!(matches!(
        Upload::prepare_many(&[]),
        Err(SelectRefusal {
            reason: SelectReason::TooMany,
            ..
        })
    ));
    assert!(matches!(
        Upload::prepare_many(&files.paths),
        Err(SelectRefusal {
            reason: SelectReason::TooMany,
            ..
        })
    ));
    let duplicate = [files.paths[0].clone(), files.paths[0].clone()];
    assert!(matches!(
        Upload::prepare_many(&duplicate),
        Err(SelectRefusal {
            index: 1,
            reason: SelectReason::DuplicateName
        })
    ));
    let missing = [files.paths[0].clone(), files.root.join("not-selected")];
    assert!(matches!(
        Upload::prepare_many(&missing),
        Err(SelectRefusal { index: 1, .. })
    ));
    let directory = [files.paths[0].clone(), files.root.clone()];
    assert!(matches!(
        Upload::prepare_many(&directory),
        Err(SelectRefusal {
            index: 1,
            reason: SelectReason::Directory
        })
    ));
    // A refused partial preparation retained no handle: ordinary sources remain
    // writable. Windows deny-write source handles would make this fail.
    fs::write(&files.paths[0], b"still local").unwrap();
}

#[test]
fn all_selected_publications_and_original_cleanup_are_required() {
    let receipts = [published(1, 2), published(2, 0), published(3, 7)];
    assert!(completed_batch_succeeded(
        3,
        true,
        true,
        receipts.into_iter()
    ));
    assert!(!completed_batch_succeeded(
        3,
        true,
        true,
        receipts[..1].iter().copied()
    ));
    assert!(!completed_batch_succeeded(
        2,
        true,
        true,
        receipts.into_iter()
    ));
    assert!(!completed_batch_succeeded(
        3,
        false,
        true,
        receipts.into_iter()
    ));
    assert!(!completed_batch_succeeded(
        3,
        true,
        false,
        receipts.into_iter()
    ));
    assert!(!completed_batch_succeeded(
        0,
        true,
        true,
        std::iter::empty()
    ));
}

#[test]
fn a_later_failure_keeps_each_earlier_publication_without_certifying_the_batch() {
    for outcome in [
        FileSendOutcome::HostRefused(fr_wire::files::Reason::Conflict),
        FileSendOutcome::InterruptedBeforePublication(FileSendError::Cancelled),
        FileSendOutcome::PublicationUnknown,
        FileSendOutcome::HostPublished {
            bytes: 9,
            publication: Publication::DurabilityUnknown,
        },
        FileSendOutcome::ControllerPublished {
            bytes: 9,
            publication: Publication::Durable,
        },
    ] {
        let receipts = [published(1, 3), receipt(2, outcome)];
        assert!(!completed_batch_succeeded(
            2,
            true,
            true,
            receipts.into_iter()
        ));
        let counts = ReceiptCounts::from_receipts(receipts.into_iter());
        assert_eq!(counts.reported, 2);
        assert!(counts.published >= 1);
        let first = receipt_document(0, receipts[0]);
        assert_eq!(first["index"], 0);
        assert_eq!(first["id"], 1);
        assert_eq!(first["published_bytes"], 3);
        assert_eq!(first["durable"], true);
        let second = receipt_document(1, receipts[1]);
        assert_eq!(second["index"], 1);
        assert_eq!(second["id"], 2);
        assert_ne!(second["outcome"], "host_published");
        if matches!(outcome, FileSendOutcome::PublicationUnknown) {
            assert_eq!(counts.publication_unknown, 1);
            assert_eq!(second["published_bytes"], serde_json::Value::Null);
            assert_eq!(second["durable"], serde_json::Value::Null);
        }
        if matches!(
            outcome,
            FileSendOutcome::HostPublished {
                publication: Publication::DurabilityUnknown,
                ..
            }
        ) {
            assert_eq!(counts.published, 2);
            assert_eq!(counts.durability_unknown, 1);
            assert_eq!(second["published_bytes"], 9);
            assert_eq!(second["durable"], false);
        }
    }
}
