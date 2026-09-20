# Shared selected-display bootstrap

An approved native observation session can now join an independently owned,
locally selected full-display source without creating another capture process
or borrowing the first viewer's observation lifetime. This connects
[local native selection](LOCAL_SELECTED_SOURCE.md) to normal display selection,
role attachments and the existing shared decoder-startup service.

## Ownership and execution

The local share-session owner discovers the native monitor catalog and calls
`DiscoveredSource::configure_local`. It prepares one actual shared recovery IDR
in the original `SharedFramePool` and constructs `Publisher` with that exact
source, pool and update. The owner must already have local observation consent;
none of these APIs creates OS permission, consent, or a controlling input lease.

The first approved observer calls `HostSession::start_shared_display` with the
pristine publisher and its original shared IDR. The normal host driver waits for
the local sharing surface, offers only the selected display, receives an explicit
viewer selection, and completes three fresh role-specific attachments. The
existing startup code checks the original source and pool rather than accepting
arbitrary encoded bytes. The result is a `SharedHost` with decoder startup still
pending: configuration, Configured, recovery delivery and FirstDecoded retain
their distinct meanings. No return value claims visible presentation.

Drive the returned host and the original publisher as sibling owners. Further
approved viewers call `HostSession::join_shared_display` using a weak `JoinQueue`.
They use the same display-selection and attachment exchange without borrowing the
capture task. The existing rate-bounded source IDR coalescer supplies their fresh
bootstrap, and ordinary `SharedHost::drive`/`serve` finishes the handshake.
Dropping the first viewer leaves the original source and healthy viewers intact.
The local `SessionAgent` source-consent registration/renewal must continue on its
original owner; a viewer heartbeat cannot renew that consent. The normal listener
and automatic OS share-session registry remain outside this slice.

## Admission and failure bounds

One fixed selected-only catalog is retained per publisher. It exposes neither
neighboring monitors nor native identifiers and is not a new topology observation.
Native capture keeps its existing generation and topology checks. Every completed,
pending and late subscriber must match the source display, geometry and codec
configuration, including the first subscriber before a cohort anchor exists.
The host and viewer selection proofs must remain alive after the attachment
handoff. The canonical host now retains its proof until closure.

The original call-time budget, at most two seconds, covers display selection,
attachments, waiting for a late-join IDR, configuration and first decode. Stage
handoff clamps deadlines; it cannot create a new window. Unpolled cancellation
revokes the joining viewer. A failed initial attempt can leave a still-authorized,
unstarted source available only within its original source bootstrap budget.
A running cohort cannot be borrowed through the first-observer API.

The existing canonical network driver checks both source and viewer authority
before writes and around each entropy callback. Tickets are fresh, role-specific
and generated outside publisher locks. Entropy failure retires the joining
viewer, not a healthy cohort. Source revocation fences all affected viewers before
another attachment write. Control-intent sessions are refused. No new worker,
transport, runtime, dependency, input authority or freshness witness is created.

## Executed validation and limits

Fifteen new tests pass: eight local-source tests plus seven canonical-session
tests. They cover original-child retention and alias translation, selected-only
disclosure, source/viewer independence, stale or wrong selection, native refusal,
call-time expiry, unpolled cancellation, first-observer bootstrap, a concurrent
late join, first-viewer departure, invalid input intent, source revocation and
entropy failure. They assert that the same original capture child survives and
that no decoder milestone grants input or visibility. The new first-subscriber
wrong-display assertion fails with the exact pre-enforcement pending-admission
implementation and passes with the final code. That is a focused negative control,
not a full old-workspace execution.

Final executed totals are **87 passed and one failed** across four targets:
eight local-source tests; 24 shared-session unit tests (seven new, 17 existing);
ten unchanged shared-late-join integration tests; and 45 of 46 unchanged
shared-startup integration tests. The startup failure is
`a_delayed_configured_reply_cannot_renew_the_original_shared_frame_deadline`:
the Link drive helper unwraps a transport Expired result before reaching its
intended delayed-Configured assertion. It repeats on the final full target and
on the exact unchanged 16cd6e8d baseline full target. That baseline run has a
second slow-viewer timing failure (44/46); an isolated baseline delayed test
passes, while an isolated final delayed test fails. This remains unresolved;
no assertion, authority deadline or transport error was relaxed to force green.

The 24-test shared-session group passes with the test harness stack set to
16 MiB. An earlier combined run aborted on an existing admission test's default
thread-stack overflow. All seven new session tests also pass separately on the
default stack. This is a test-run setting, not a production stack or deadline
change. The full Linux daemon unit-test source and production source pass strict
pedantic Clippy; the eight-test new integration target passes it as well.
Formatting, whitespace and repository documentation checks pass.

Runtime tests use real TLS/UDP, original session owners and supervised child
processes. Native monitor responses, codec bytes and decoder acknowledgements
are explicit fixtures. They are not native X11/HEVC, physical visibility, GPU,
independent wire-interoperability or live-tailnet qualification. Every relevant
first-party library was rebuilt from the checksum-verified 16cd6e8d snapshot
plus these slices with nightly-2026-08-31. Matching external libraries came from
GitHub run 35461102442, with compiler and all external lockfile versions/checksums
checked. The focused unit-test harness changes only unrelated test registrations
in an external source copy; original production and assertion bodies remain
intact. Full source Clippy uses the actual repository, not that filtered copy.
This is not a cold-dependency, full Cargo/workspace or release gate pass.

Refs: plan 7, 11.2, 11.3, 17 and 19; `fr-p1-frame-pipeline-am1` and
`fr-p2-viewer-admission-e62`. Broader qualification beads remain open.
