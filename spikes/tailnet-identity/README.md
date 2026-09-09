# Installed Tailscale identity qualification

This experiment exercises `fr-tailnet` against the installed Linux daemon. It
owns plan sections 6.1–6.3 and 19.2 under `fr-p0-tailnet-identity-q0h`. It does
not open a listener, grant a desktop session, change policy or supply an
alternate identity authority.

## Reproduce the read-only probe

Build with the repository-pinned nightly, through RCH:

```sh
RCH_REQUIRE_REMOTE=1 rch exec -- cargo build -p fr-tailnet --example localapi_check --locked -j 2
```

Run the resulting executable on the Linux machine whose installed daemon is
being qualified. Use the executable path from Cargo's artifact output or the
RCH worker artifact; do not substitute an older local executable. Supply a
current local node address and a peer node address with nonzero ports:

```sh
./localapi_check "$LOCAL_ENDPOINT" "$PEER_ENDPOINT" own-user
./localapi_check "$LOCAL_ENDPOINT" "$PEER_ENDPOINT" tailnet
```

The two addresses are **metadata query inputs**, not evidence of an accepted
transport connection. The output always marks `transport_ingress_qualified`
false. Successful metadata authorization exits 0; a typed refusal exits 2;
invalid invocation/runtime setup exits 1. Neither exit 0 nor widening the
local scope qualifies tunnel ingress, membership or input authority. The
fixed installed socket must authenticate a root daemon before any HTTP query.
The existing one-second lookup/authorization deadlines and response limits
apply. Output contains no addresses, names, identities or metadata bodies.

## Linux 1.102.3: missing positive machine approval

The installed daemon reported `1.102.3-t9329c3677-ga522f65e9`. A bounded
status/WhoIs/status capture at 2026-09-09 13:23:05 UTC authenticated the same
root-owned daemon for all three requests. The selected peer and host had
matching user IDs, and their relevant metadata was stable across the status
reads. This does not establish the full membership/sharing matrix.

The returned `WhoIs.Node` omitted `MachineAuthorized`; top-level `CapMap` was
null. The adapter keeps its requirement for positive machine approval and a
desktop application grant. `MachineNotAuthorized` now identifies the first
missing condition separately from `CapabilityDenied`. It describes the
evidence returned by the daemon, not a claim about the administrator's intended
approval policy. Adding an app grant alone would not satisfy the former check.

The official v1.102.3 annotated tag `9329c3677031109ff6d0b80abee0cddc8f35ff6f`
points to source commit `53a0d659afa51835dd7a9283873cca44261454f8`:

- [`MachineAuthorized` remains a non-deprecated bool with `omitempty`](https://github.com/tailscale/tailscale/blob/53a0d659afa51835dd7a9283873cca44261454f8/tailcfg/tailcfg.go#L437).
  The default false value can therefore be absent from JSON.
- [WhoIs serializes the returned node directly](https://github.com/tailscale/tailscale/blob/53a0d659afa51835dd7a9283873cca44261454f8/ipn/localapi/localapi.go#L596).
  [Successful node/user lookup](https://github.com/tailscale/tailscale/blob/53a0d659afa51835dd7a9283873cca44261454f8/ipn/ipnlocal/local.go#L1698)
  does not establish that approval bit.
- [The local machine-status check](https://github.com/tailscale/tailscale/blob/53a0d659afa51835dd7a9283873cca44261454f8/types/netmap/netmap.go#L198)
  refers to the self node; a running local backend cannot substitute for peer
  approval. [Peer application grants](https://github.com/tailscale/tailscale/blob/53a0d659afa51835dd7a9283873cca44261454f8/tailcfg/tailcfg.go#L1669)
  are a distinct signal.

Public client source does not settle why this peer's approval field was unset
or provide a qualified replacement signal. Do not weaken the check based on
reachability, user equality, `InNetworkMap`, names or absent sharing markers.

The [sanitized fixture projection](../../crates/fr-tailnet/tests/fixtures/linux-1.102.3/README.md)
retains the observed missing field and explicit null. Its replay is regression
evidence; synthetic positive grants in adjacent tests remain synthetic.

The [retained results](results/linux-1.102.3-20260909/) contain actual stdout,
stderr and exit codes for both scopes. The final `verified-*` binary SHA-256 is
`397a3e83825ea397c3cdb0f5bd397faec841c8b9e9e4c7ef616e2a4f4538a3e6`.
Both runs exited 2 with `MachineNotAuthorized`; the earlier baseline returned
`CapabilityDenied`. `source.sha256` binds the owned source/projection bytes;
the build transcript binds base `02cabc95` plus its explicit overlay. Build
logs omit unrelated dependency artifact JSON, retaining compiler diagnostics,
the relevant first-party artifacts and the terminal RCH result.

An intermediate rebuild reused the baseline executable despite the changed
source overlay. The source edits predated completion of the earlier build;
refreshing the owned files' modification times forced Cargo to compile both
the library and example (`fresh:false`). That rebuilt executable has the new
refusal label above (`fresh-*` records); `verified-*` repeats the live check
after the final Clippy fixes. Six invalid invocations and a loopback-address
refusal also passed against that final executable. The stale run is excluded from changed-source
evidence. An initial baseline launch also raced an unfinished executable copy
and failed with `Text file busy`; the retained baseline runs occurred only
after the copy completed and its hash matched the retrieved artifact.

This record is consumed by the owner of `fr-p0-tailnet-identity-q0h` when
deciding whether the installed profile qualifies. It prevents promoting a
fixture-only positive result over the observed live refusal. Supersede this
negative row when an independently qualified installed profile and sharing
matrix replace it; retain or remove historical files only with owner approval.

## Source checks

The retained workspace run passed 418 tests with no failures or ignored tests;
this includes 25 `fr-tailnet` tests and all three new negotiation-framing
regressions for `fr-m6u`. The example lane passed its two tests; its other
example harnesses contain no tests and are compile evidence only. Workspace
check and Clippy passed with all targets/features and warnings denied for
Clippy. Cargo commands
used strict RCH on `vmi1149989`, serially with two jobs, against base `02cabc95`
plus the explicit source overlay recorded in each transcript. No local build
fallback was used. Formatting and document-link checks also passed.

UBS exited 1 on the six touched Rust files: two critical classifications, 420
warnings and 68 informational findings. The criticals are the existing
fixture-server `panic!` and an existing FRD0 `Record::decode` test incorrectly
classified as JWT decoding. Neither was suppressed or weakened. The 420
warnings were not all individually adjudicated; this is **not** a scanner-green
or full-project qualification claim. UBS's shadow workspace contains no Cargo
manifest, so its displayed build/lint status is not used as Cargo evidence.
Only decorative trailing whitespace was trimmed from the retained UBS output.

## Qualification still required

Successful installed-daemon application admission, different-owner members,
tagged members/hosts, shared-in hosts, external sharees, multi-tailnet users,
policy removal, identity changes, real key expiry and IPv4/IPv6 ingress
enforcement remain unqualified. Windows and the macOS installation variants
have not been tested. This Linux negative row does not close the Phase 0 gate
or establish the zero-policy-edit profile.
