# Independently owned selected-display capture

`DiscoveredSource::configure_local` joins a local full-display choice to the
original discovered capture worker, without using a viewer's `SelectedDisplay`
or borrowing that viewer's authority. The caller must already possess local
observation consent. Peer admission, an attached network renewer, and an input
owner are refused; the operation never creates consent, readiness or control.
The existing network-selected `configure` path remains separate.

The original catalog aliases, exact full-display geometry and codec dimensions
are checked before configuration. The private worker receives its original
native selection, not the network alias. One absolute native deadline starts
when the configuration future is created, including time before its first poll.
Refusal and cancellation fence the source before releasing the original child.
A retained Launch retirement handle can collect that child with a separate
cleanup context. No replacement worker or display connection is created.

`CaptureSource::selected_catalog` returns only that selected display, with the
original alias and catalog revision, under the exact same source authority.
An equal numeric session ID is insufficient. This bounded metadata snapshot
is neither a topology observation nor evidence of fresh pixels; idle native
topology checks and capture still use their existing source guards. Unrestricted
mutable worker access retires the selection as well as capture provenance.

This enables a locally selected source to be handed to the existing shared
publisher rather than tying native monitor configuration to its first viewer.
Normal listener and shared-session selection/attachment integration are separate
work; this API is not an installable multi-viewer workstation or an OS consent UI.

## Executed validation

Eight `frd --test local_source` cases pass using real supervised child processes
and the production worker IPC, capture and authority implementations. The child
is an explicit synthetic monitor protocol fixture, not native X11, real HEVC,
physical visibility or live-tailnet evidence. Tests cover same-child alias
translation, selected-only disclosure, unchanged-frame provenance, independent
viewer departure, equal-numbered foreign authority, stale selection and wrong
dimensions, input-owner refusal, native configuration/topology refusals, unpolled
cancellation, original deadlines, and mutable-worker invalidation. No assertions
or production deadlines were relaxed. Strict daemon production and complete new
test-target pedantic Clippy, formatting and whitespace checks pass.

Execution rebuilds eight first-party libraries from source snapshot
16cd6e8d48a2d444e8d4ed072ac40128f91eab9d plus this slice with the pinned
nightly-2026-08-31 compiler. External build inputs are from GitHub run35461102442;
the exact compiler and all external lockfile versions/checksums were compared,
and archive hashes verified before use. Initial harness attempts had missing
split metadata and duplicate getrandom-version selection; supplying both matching
rlib/rmeta artifacts and the manifest's getrandom0.4.3 corrected the harness,
without changing dependencies or repository build files. This is focused runtime
and source validation, not a cold or full-workspace Cargo test claim.

Refs: plan7/11.2/11.3/19; fr-p1-frame-pipeline-am1,
fr-p2-viewer-admission-e62. Broader qualification gates remain open.
