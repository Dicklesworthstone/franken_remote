# Observation-client disconnect and closing receive credit

`ViewerSession::disconnect(reason)` ends an already-bound observation session on
its original connection. The call stops ordinary session I/O, observation
responses and clock service before its future is polled. An ownership guard is
installed before preflight or transport callbacks, so preparation unwind and an
unpolled abandoned future still run local teardown. The existing stop handle can
cancel the exchange; no cancelled context is reset or given fresh authority.

The method uses `QuicRecords::close_with_request`, not an ordinary transport loan
or a second socket. Its construction-time 250-ms deadline is shortened by the
session's original silence deadline and any pending challenge-response deadline.
It sends one session-close request and examines only the bounded original control
lane, without application callbacks, fresh observations or challenge responses.
The original destination/security guard and native-backlog refusal remain active.

`CloseOutcome` distinguishes request transport acknowledgement, an optional exact
host `Closed` report, and transport success/failure. A received report remains in
`ViewerSession::closed_report()` after local teardown or a rejected second call.
A missing report never becomes completed cleanup or zero outstanding effects.
The ordinary protected host dispatcher and its deferred reporter already handle
this request; neither endpoint needs a raw transport callback for the exchange.

Only an originally negotiated observation role can use this API. A control-intent
session refuses and closes locally; its native input-cleanup and lease-specific
reporting obligations remain with their original owners. Callers owning a native
decoder or presentation loop must stop those operations before disconnecting.
This method does not claim another worker exited, change input receipt ledgers,
or automatically wire the native window's emergency-stop action. Running-window
coordination and independently confirmed native cleanup remain separate work.

## Closing must continue the original receive accounting

The canonical exchange in d5708959 consumed stale control records without
advancing connection-level receive credit. With a 2-KiB connection window, a
valid final report behind several 1-KiB control records timed out even after the
close request was acknowledged. The defect also occurred after earlier traffic
had advanced the live session's offsets.

The exchange now transfers `read_bytes`, `advertised_limit`, and the ORIGINAL
connection window. Only actual reads of the original control stream advance
those counters. The same fixed window is advertised at its consumed offset;
undrained media does not mint credit. This neither enlarges negotiated buffers
nor changes the 32-record examination limit, deadlines, authority, or callbacks.

Two actual TLS/UDP regressions fail on the original exchange and pass with this
fix. They cover a fresh window and eight previously consumed record windows,
then place the final report behind eight valid padded observation challenges.
The challenges use real encoding and are checked by the production parser;
closing does not answer them or renew observation.

## Executed verification

The final selected runtime results are 44 unique passing tests: the two new
receive-credit regressions, all 32 existing terminal tests in source 7708fc0,
and the ten protected observation-closure tests (five new client API cases and
five unchanged server cases). Tests cover exact uncertainty, missing reports
after credential loss, ordinary renewal shutdown, cancelled and unpolled futures,
control-intent refusal, malformed requests, and original owner retention.
The two failing-before credit results are retained separately from the passing
runs. No production timeout or existing assertion was weakened.

Strict pedantic Clippy passed for the complete transport and daemon production
libraries and the two changed test targets. Changed-file formatting and whitespace
checks passed. All eight relevant first-party libraries were built from the
checksum-verified 7708fc0 source archive plus the canonical d5708959 exchange and
these changes, using pinned nightly-2026-08-31 and unchanged compiler/lock-matched
external libraries retained by CI. This is not a cold dependency build or a full
current-main workspace test. Later disjoint native consent and transport scheduling
changes are preserved at publication, not included in this executed baseline.

The protected-listener tests run in isolated mount/user/network namespaces with
explicit LocalAPI identity, interface/firewall, approval and cleanup fixtures.
TLS/UDP, protocol, session and custody paths are real; installed Tailscale, native
input release, HEVC/GPU, physical presentation and hardware are not qualified.
The separate concurrent exchange suite is not counted as rerun here.

Refs: plan section 19; PROTOCOL.md sections 4/6;
`fr-rc-protocol-refusal-closure-5dx` remains open.
