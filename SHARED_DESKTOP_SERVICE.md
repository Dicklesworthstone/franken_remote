# One shared-desktop service lifetime

`SessionAgent::serve_shared_desktop` joins the original local source-consent
registration, `Publisher::serve` and `shared_viewers::Hub` in one cancellation-safe
future. Callers no longer have to manually multiplex capture and every viewer's
renewal. The service accepts one already-authorized selected source and its exact
original hub. Pointer identity, not reused numeric IDs, binds the pair. Another
publisher or local agent is refused without taking over or revoking its owner.

The platform supplies a required bounded, nonblocking local-event callback. It
runs before network or capture work on every poll and may register the task waker
for immediate OS-event delivery. After it runs, the original registration checks
capture permission, unlocked OS session and source scope. Only that registration
renews source consent; each viewer retains its original independent renewal and
admission driver. No OS permission, input authority, visible frame or source
observation is inferred from a timer, packet, decode result or successful renewal.
The callback must feed actual platform events/probes, not assumed permissions, and
retain responsibility for any held-input cleanup returned by agent lifecycle APIs.

One timer is registered with the source's existing Asupersync timer driver, with a
10 ms maintenance bound or earlier consent deadline. Registrations are updated,
not queued. Capture's existing timer also binds to its source context at poll;
there is no fallback timer thread, alternate runtime or detached service. This is
a cooperative scheduling bound, not a wall-clock preemption guarantee for blocking
caller code. The original network and capture futures remain alive across polls:
a stalled native capture cannot stop consent checks, viewer renewal or late joins.

Cancellation, a local stop, source failure, entropy failure and unwinding fence
all active AND pending viewers before source/media release and cancellation of the
original network/capture futures. Fencing occurs before a terminal result returns,
even if the caller retains the completed future. A per-poll unwind guard likewise
protects a future retained after catching a local adapter panic. The original
Publisher remains available for supervised child reaping. Local Stop affects this
source/cohort, not unrelated sources registered to the same SessionAgent.

The report exposes local source-renewal count and a viewer-service counter snapshot
at completion. These are permission/driver milestones, not presentation evidence.
The existing limits, source keyframe coalescing, codec dependencies and no-catch-up
capture policy are unchanged. No extra encoder, native worker, network owner,
media queue, protocol message, input grant or dependency is introduced.

## Verification scope

Nine new combined-service tests cover continued source and viewer renewal past
both original lifetimes, real late joining while a healthy client consumes frames,
unpolled cancellation with pending admissions, source/agent mismatch, permission
loss before entropy or network work, entropy failure, caught adapter unwinding,
local-stop isolation and timer cleanup. A test stops the original capture child
with SIGSTOP, then delivers a local permission event: the service fences the source
without waiting for capture completion, and the same supervisor reaps that child.
The continuing-source test retains the original process and idle frame identity;
there is no dummy encoding or manufactured visibility claim.

The final canonical shared-session group passes 54 tests, including these nine and
the seven Hub tests. All nine new tests also pass on the default test-thread stack;
the combined group uses its previously required 16 MiB stack. Production daemon
and complete actual Linux daemon unit-test source pass strict pedantic Clippy.
Changed-file formatting and whitespace pass. The retained broader startup suite
has one unresolved transport Expired failure in the delayed-Configured link helper
before its intended assertion; its assertions/deadlines are unchanged. The same
failure reproduces in a freshly built unchanged 8306c6d9 baseline. That baseline's
full startup run passes 44 tests and also fails the all-blocked-viewer timing case;
the current feature run passes that case, but this is not a claim to have fixed
its intermittent behavior. Together with ten unchanged late-join and eight
unchanged local-source tests, the final current-code groups total 117 passed and
one unresolved failure. Baseline results are not counted as feature passes. The
repository-wide docs check remains blocked by an unrelated existing relative link
in docs/software-encoder-evaluation.md, not this service's documentation.

The implementation builds on the source-tested Hub checkpoint. First-party
libraries are source-built with nightly-2026-08-31; external inputs match the
compiler, package versions and checksums retained by run 35461102442. Execution
uses real TLS/UDP and supervised process IPC, with explicit monitor, permission,
identity, codec and decode-acknowledgement fixtures. The focused runtime harness
removes only unrelated test registrations in an external copy; production code and
original retained assertions are unchanged. Full actual test source is separately
linted. This is not cold/full-workspace, native OS/HEVC, GPU, physical-display,
independent-transport or live-tailnet qualification.

Listener admission and native platform event adapters still require integration;
this service does not invent a listener or claim that cached permission state is
an OS probe. The caller must supply actual local events and already-admitted
sessions. Input control remains outside this observation-only service. Broader
beads remain open. Refs: plan 7/11/17/19; fr-p1-frame-pipeline-am1 and
fr-p2-viewer-admission-e62. See [viewer service](SHARED_VIEWER_SERVICE.md) and
[local source ownership](PREVIEWER_SOURCE_CONSENT.md).
