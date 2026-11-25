//! Real file publication with a one-shot fault in directory durability only.
use super::{Scratch, verified};
use crate::{
    receive::{DropDirectory, Error, Publication},
    sync::{
        SyncBaseline, SyncError, SyncJobConfig, SyncLimits, SyncRoundFailure, SyncRoundOutcome,
        execute_sync_round,
    },
};
use fr_core::ids::{InputLeaseId, RemoteSessionId};
use fr_wire::{
    files::{Context, Direction, Lane, Reason, Role},
    sync_job_state::{ConflictPolicy, SyncDirection, SyncPhase},
};
use std::{fs, sync::atomic::Ordering};

#[test]
fn durability_fault_is_consumed_only_after_a_real_publication() {
    let scratch = Scratch::new();
    let root = scratch.open();
    root.0.fail_publication_sync.store(true, Ordering::Release);
    assert!(matches!(
        verified(&root, "cancelled", b"private").publish_named_checked(|| Err(Error::Retired)),
        Err(Error::Retired)
    ));
    assert_eq!(scratch.entries(), 0);
    fs::write(scratch.0.join("occupied"), b"keep").unwrap();
    assert!(matches!(
        verified(&root, "occupied", b"private").publish_named(),
        Err(Error::Conflict)
    ));
    assert!(root.0.fail_publication_sync.load(Ordering::Acquire));

    let published = verified(&root, "committed", b"published")
        .publish_named()
        .unwrap();
    assert_eq!(published.publication(), Publication::DurabilityUnknown);
    assert_eq!(published.name(), "committed");
    assert_eq!(fs::read(scratch.0.join("committed")).unwrap(), b"published");
    assert_eq!(fs::read(scratch.0.join("occupied")).unwrap(), b"keep");
    assert!(!root.0.fail_publication_sync.load(Ordering::Acquire));
    assert_eq!(
        verified(&root, "later", b"confirmed").publish(),
        Ok(Publication::Durable)
    );
}

struct Round {
    host_dir: Scratch,
    controller_dir: Scratch,
    host: DropDirectory,
    controller: DropDirectory,
    config: SyncJobConfig,
}
impl Round {
    fn new(direction: SyncDirection, conflict: bool) -> Self {
        let host_dir = Scratch::new();
        let controller_dir = Scratch::new();
        let host = host_dir.open();
        let controller = controller_dir.open();
        let round = Self {
            host_dir,
            controller_dir,
            host,
            controller,
            config: SyncJobConfig {
                job_id: 19,
                direction,
                conflict_policy: ConflictPolicy::KeepBoth,
                limits: SyncLimits::default(),
            },
        };
        let source = match direction {
            SyncDirection::HostToController => &round.host_dir,
            SyncDirection::ControllerToHost => &round.controller_dir,
            SyncDirection::Bidirectional => unreachable!("one-way test case"),
        };
        for (name, bytes) in [
            ("a-before", b"before".as_slice()),
            ("b-uncertain", b"uncertain".as_slice()),
            ("c-after", b"after".as_slice()),
        ] {
            fs::write(source.0.join(name), bytes).unwrap();
        }
        if conflict {
            let (destination, _) = round.destination();
            fs::write(destination.0.join("b-uncertain"), b"keep").unwrap();
        }
        round
    }

    fn destination(&self) -> (&Scratch, &DropDirectory) {
        match self.config.direction {
            SyncDirection::HostToController => (&self.controller_dir, &self.controller),
            SyncDirection::ControllerToHost => (&self.host_dir, &self.host),
            SyncDirection::Bidirectional => unreachable!("one-way test case"),
        }
    }

    fn run(
        &self,
        baseline: &SyncBaseline,
        revision: u64,
        check: impl FnMut() -> Result<(), SyncError>,
    ) -> Result<SyncRoundOutcome, SyncRoundFailure> {
        let context = Context {
            session: RemoteSessionId::from_raw(11),
            lease: InputLeaseId::from_raw(12),
            handle: 13,
            channel: 42,
            sender: Role::Host,
            direction: Direction::ToHost,
            lane: Lane::Files,
        };
        execute_sync_round(
            &self.config,
            context,
            Context {
                sender: Role::Controller,
                ..context
            },
            &self.host_dir.0,
            &self.host,
            &self.controller_dir.0,
            &self.controller,
            baseline,
            revision,
            check,
        )
    }
}

#[test]
fn sync_retains_uncertain_publication_and_never_replays_it_on_a_later_round() {
    for direction in [
        SyncDirection::HostToController,
        SyncDirection::ControllerToHost,
    ] {
        for conflict in [false, true] {
            check_uncertain_round(direction, conflict);
        }
    }
}

fn check_uncertain_round(direction: SyncDirection, conflict: bool) {
    let round = Round::new(direction, conflict);
    let (destination, owner) = round.destination();
    let mut armed = false;
    let failure = round
        .run(&SyncBaseline::new(), 1, || {
            // The first real publication remains durable. Fail only the next
            // post-rename directory sync, after normal hashing and staging.
            if !armed && destination.0.join("a-before").exists() {
                owner.0.fail_publication_sync.store(true, Ordering::Release);
                armed = true;
            }
            Ok(())
        })
        .unwrap_err();
    assert!(armed);
    assert!(!owner.0.fail_publication_sync.load(Ordering::Acquire));
    assert_eq!(failure.error, SyncError::DurabilityUnknown);
    assert!(!failure.cleanup_failed);
    assert_eq!(failure.published.len(), 2);
    assert_eq!(failure.published[0].original_name(), "a-before");
    assert_eq!(failure.published[0].publication, Publication::Durable);
    let uncertain = &failure.published[1];
    assert_eq!(uncertain.original_name(), "b-uncertain");
    assert_eq!(uncertain.publication, Publication::DurabilityUnknown);
    assert_eq!(uncertain.conflict_copy, conflict);
    assert_eq!(
        fs::read(destination.0.join(uncertain.published_name())).unwrap(),
        b"uncertain"
    );
    assert_eq!(destination.entries(), if conflict { 3 } else { 2 });
    assert!(!destination.0.join("c-after").exists());
    if conflict {
        assert_eq!(
            fs::read(destination.0.join("b-uncertain")).unwrap(),
            b"keep"
        );
    }

    let terminal = &failure.wire_states.last().unwrap().state;
    assert_eq!(terminal.phase, SyncPhase::Failed);
    assert_eq!(terminal.reason, Reason::UnknownEffect);
    assert_eq!(terminal.entries_total, 3);
    assert_eq!(terminal.entries_completed, 2);
    assert_eq!(terminal.bytes_total, 20);
    assert_eq!(terminal.bytes_completed, 15);
    assert_eq!(terminal.conflicts_kept, u32::from(conflict));
    assert!(failure.wire_states.iter().all(|emitted| {
        !matches!(
            emitted.state.phase,
            SyncPhase::Settled | SyncPhase::ConflictRecorded
        )
    }));

    resume_after_durability_unknown(&round, &failure, conflict);
}

fn resume_after_durability_unknown(round: &Round, failure: &SyncRoundFailure, conflict: bool) {
    let (destination, _) = round.destination();
    let uncertain = &failure.published[1];
    let refused = round
        .run(&failure.baseline, 2, || Err(SyncError::AuthorityRevoked))
        .unwrap_err();
    assert_eq!(refused.error, SyncError::AuthorityRevoked);
    assert!(refused.published.is_empty());
    assert_eq!(refused.baseline, failure.baseline);
    assert!(!destination.0.join("c-after").exists());

    // A fresh caller-authorized round may deliver the untouched successor.
    // Editing the committed destination must not replay its original version
    // or create another conflict copy, even when publication was not renamed.
    fs::write(
        destination.0.join(uncertain.published_name()),
        b"new local edit",
    )
    .unwrap();
    let retry = round.run(&failure.baseline, 3, || Ok(())).unwrap();
    assert_eq!(retry.published.len(), 1);
    assert_eq!(retry.published[0].original_name(), "c-after");
    assert_eq!(retry.published[0].publication, Publication::Durable);
    assert_eq!(destination.entries(), if conflict { 4 } else { 3 });
    assert_eq!(
        fs::read(destination.0.join(uncertain.published_name())).unwrap(),
        b"new local edit"
    );
    assert_eq!(fs::read(destination.0.join("c-after")).unwrap(), b"after");
    let settled = round.run(&retry.baseline, 4, || Ok(())).unwrap();
    assert!(settled.published.is_empty());
    assert_eq!(destination.entries(), if conflict { 4 } else { 3 });

    fs::remove_file(destination.0.join(uncertain.published_name())).unwrap();
    let removed = round.run(&settled.baseline, 5, || Ok(())).unwrap();
    assert!(removed.published.is_empty());
    assert!(!destination.0.join(uncertain.published_name()).exists());
    assert_eq!(destination.entries(), if conflict { 3 } else { 2 });
    deliver_new_source_version(round, &removed.baseline, conflict);
}

fn deliver_new_source_version(round: &Round, baseline: &SyncBaseline, conflict: bool) {
    let source = match round.config.direction {
        SyncDirection::HostToController => &round.host_dir,
        SyncDirection::ControllerToHost => &round.controller_dir,
        SyncDirection::Bidirectional => unreachable!("one-way test case"),
    };
    let (destination, _) = round.destination();
    fs::write(source.0.join("b-uncertain"), b"a new source version").unwrap();
    let changed = round.run(baseline, 6, || Ok(())).unwrap();
    assert_eq!(changed.published.len(), 1);
    let receipt = &changed.published[0];
    assert_eq!(receipt.original_name(), "b-uncertain");
    assert_eq!(receipt.publication, Publication::Durable);
    assert_eq!(receipt.conflict_copy, conflict);
    assert_eq!(
        fs::read(destination.0.join(receipt.published_name())).unwrap(),
        b"a new source version"
    );
    assert_eq!(destination.entries(), if conflict { 4 } else { 3 });
    if conflict {
        assert_eq!(
            fs::read(destination.0.join("b-uncertain")).unwrap(),
            b"keep"
        );
    }

    // The receipt suppresses only the old hash, not future source edits;
    // the newly delivered version also converges without another copy.
    let settled = round.run(&changed.baseline, 7, || Ok(())).unwrap();
    assert!(settled.published.is_empty());
    assert_eq!(destination.entries(), if conflict { 4 } else { 3 });
}
