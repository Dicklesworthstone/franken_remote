# Native observation profile

`fr connect` and `fr displays` use `fr_client::native::observation_offer` rather
than maintaining an independent capability list in the executable. The required
bootstrap stays display selection, decoder configuration, role attachment and
bounded media delivery. The role remains Observe; this profile offers no input,
clipboard, files, audio or independent visibility evidence.

Two implemented features are offered optionally at their exact wire versions:

- `reference-recovery`: the canonical host/viewer loops can replace a failed
  reference chain on the original connection and native workers. The original
  failure deadline covers drain, attachment and first decode. Neither an
  observation renewal nor native completion creates extra recovery time.
- `decoder-metrics`: the host solicits bounded, generation-bound receiver load
  for its existing pacing controller. Feedback remains advisory; receiving a
  metric is not permission to observe or control, nor evidence of visible pixels.

The host's intersection must positively select each feature before its records
or state owner can exist. Hosts with only the mandatory bootstrap still work;
recovery and feedback may be selected independently. A mismatched optional
version is omitted. A missing mandatory bootstrap or required incompatible
version refuses instead of downgrading the session. SessionOpened cannot add an
unselected feature or turn an observing selection into a control request.

This activates existing runtime behavior; it does not qualify native transport,
codec hardware, or end-to-end desktop operation. The experimental native opt-in
and original identity/TLS/consent checks are unchanged. The fixed transport
namespace still admits only one full replacement before typed exhaustion; this
profile does not recycle streams, widen limits or silently reacquire control.

Tests exchange actual startup records for every optional-feature intersection.
The canonical running-peer loss tests use this exact offer, supervised synthetic
codec children and TLS/UDP connections. They check that observation recovery
continues on the original owners, that feedback resumes only when selected,
and that recovery also works without decoder metrics. Synthetic codec payloads
are not real HEVC or hardware qualification. Related work remains tracked by
`fr-p1-loss-recovery-20s` and `fr-p1-fr-client-bis` (plan sections 12, 13, 14 and 17).
