# Fresh media attachments after reference loss

`NegotiatedMedia::begin_replacement` replaces a failed observation stream's
configuration, reliable recovery and video attachments on the original QUIC
connection. It uses the existing role-specific ticket/attachment protocol, not a
new identity, socket, runtime or packet queue. `reference-recovery` version 1
must be positively selected; control-requesting sessions explicitly refuse this
observation-only path.

## Ownership and deadlines

The caller first reports/accepts the real reference failure and fences the old
view, decoder submissions and application egress. Pass that failure owner's
ORIGINAL local deadline to `begin_replacement`, plus the installed control
binding. Hosts supply three distinct unpredictable tickets; viewers supply None.
Neither peer may use the other peer's uncorrelated clock as its local deadline.

Before retirement, the implementation verifies the original connection identity,
full parent/view binding, role, capability, ticket uniqueness, deadline, authority
callback and capacity for all three new pairs. The transport validates the whole
old configuration/recovery/video set before resetting anything. Retired pairs
remain tombstones; no binding, stream or ticket is recycled. The existing stream
ceiling currently allows one complete replacement after the initial three pairs;
exhaustion refuses before discarding the working set rather than growing history.

`Replacement::advance` performs bounded turns of the three sequential exchanges.
Call it between ordinary session turns so the original observation renewal,
clock and unrelated control services continue. Control records preceding an
offer stay in their original reliable order. Backpressure preserves pending
bytes and deadlines. Each role gets only the remaining original budget; final
native callbacks recheck that budget and authority too. Wrong-generation offers
refuse before route allocation. Expiry or an uncertain partial native effect
closes the original connection, never a foreign connection supplied by mistake.

`finish` returns only fully attached new media with the same view/configuration
and the next recovery generation. This is channel admission, not decoder
configuration, IDR verification, presentation evidence or an input grant. Keep
the original deadline for those later operations. A partially started exchange
retains the transport's abandonment fence. A parent abandoning the operation
before its first offer must also close its original session.

## Integration boundary

The production attachment owners are implemented; the test exercises actual
loss -> report -> retirement -> fresh three-role handshake -> reliable recovery
and resumed dependent pictures on the same receiver and QUIC connection. Native
decoder completions in that test are simulated. It constructs a new test sender
after attachment, so it does not claim retained sender rate-history or a complete
native application handoff. The actual session must preserve its original sender
policy and admission history when switching bindings. Automatic host dispatch,
canonical session/presenter transition and control reacquisition remain open.
Do not globally advertise full automatic healing based on this attachment slice.

## Verification

The exact source archive at 57173b3 was downloaded from repository-owned workflow
35428322755 and its archive checksums verified. All eight relevant first-party
libraries and the complete daemon test binary were rebuilt with the pinned
nightly-2026-08-31 compiler against unchanged, matching upstream libraries retained
by CI run35421807938. This is not a cold dependency or Cargo workspace build.

The source plus this change passes all 308 runnable daemon library tests, with
16 explicitly ignored and no failures; eight new replacement tests; six existing
recovery-control tests; and five existing transport media-retirement tests.
The new cases include complete reference loss and resumption, immutable timeout,
wrong generation/connection/role, preflight refusal, namespace exhaustion, real
control backpressure and abandonment. Strict Clippy passes for transport and
daemon libraries, complete daemon test source and new replacement tests. Two
existing recovery-policy Clippy errors were corrected without changing behavior.
No native HEVC, hardware, independent interoperability or live-tailnet qualification
is claimed. Concurrent later file/desktop changes are preserved separately.
