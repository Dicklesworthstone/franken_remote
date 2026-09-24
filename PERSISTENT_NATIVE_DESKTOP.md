# Protected serial acceptance through a native desktop

`LinuxServer::serve_desktop` composes the existing protected serial listener with
one exact `session_agent::source::desktop::dispatch::Driver`. This is the
serial-acceptance counterpart to `Driver::serve_on_linux`, which deliberately
serves one observation attempt. Neither API replaces the native transport,
source preparation, local permission owner, or shared-viewer hub.

Before an admitted peer reaches the desktop, idle acquisition and unambiguous
peer refusals can follow the existing listener's cooldown/retirement policy. A
refused peer does not consume the cold desktop slot or invoke its factory. An
independently authorized peer can subsequently enter that same selected share.
Each native attempt still needs fresh local connection/remote-session IDs,
installed membership evidence, optional live host policy, and local approval.

The `Connections` callbacks allocate those request IDs, notify the existing local
approval capability, and receive authentic peer outcomes obtained by the serial
listener. Notification is not consent. The callbacks are local and nonblocking;
they cannot override fatal identity, ingress or policy failures. The asynchronous
source factory is invoked only after the original observation consent and media
capability negotiation. Source authority must be created at that point, not
left aging while earlier unauthenticated peers wait or fail.

The desktop's local event callback runs before listener/network work on every
turn. Both original futures stay pinned through waits. The listener supervisor,
broker/credential contexts and source context must remain distinct and use the
same runtime. No additional task, runtime, authority grant, native worker,
packet queue, or automatic effect replay is introduced by the composition.

## Lifetime and cleanup

This API owns **one selected OS-share lifetime, with capacity-one networking**.
It is not simultaneous multi-client UDP routing or a source-restart loop. Source
termination, including the existing last-subscriber shutdown, ends the whole
composition. A new share requires fresh explicit owners after observed cleanup;
normal peer departure does not secretly leave an encoder running indefinitely.

`End` identifies the original service that returned first and preserves its
result. The companion service is then fenced and its pending work eagerly
dropped. Its cancellation is not relabeled a successful session or an observed
OS cleanup result. Whole-share abandonment does not fabricate a per-peer callback
result. A terminal future can be retained/repolled without retaining or restarting
the original application work. Caught local panics also fence both owners before
control returns to the embedding application.

Retain the `Driver` for its original child `reap`, the factory's
`Launch::retain_cleanup` receipt for failed preparation, and the `LinuxServer`
for explicit firewall `stop`. Their cleanup uses a separate bounded context.
Cancellation alone does not authorize rule removal while an escaped original
transport still owns an ingress lease.

## Continuous reliable records

The accompanying transport fix stages closed batches of reliable records. New
admissions remain counted in the existing project queue until the staged batch
is absent from both native pending and retransmission storage. Only then does
that batch release retained credit. Waiting records keep their own unchanged
send deadlines; a later record cannot perpetually keep an acknowledged earlier
batch charged. This uses the existing exact empty-state witness rather than
cloning live native buffers or guessing individual ACKs. The earliest original
batch deadline remains conservative. An ACK-drain boundary can affect throughput;
no unchanged WAN-throughput claim is made.

## Evidence and remaining scope

The integration regressions execute actual TLS/UDP, credential-checked Unix
HTTP, Host/Viewer approval, selected-display attachment, native child IPC, media
record delivery, and explicit child/firewall-owner retirement. They include a
static-source stream that lasts beyond its original authority lease, denial,
refusal followed by an authorized viewer, ingress loss while a factory is parked,
and caught local panic after a first frame. The same static-source test fails
with the original transport's busy-stream retirement accounting.

Interface/nftables replies, installed-tailnet metadata, OS capture permission,
HEVC payloads and decoder acknowledgements are explicit fixtures. These tests
do not qualify actual kernel filtering, installed Tailscale/logind, a physical
display or codec. The existing isolated namespace runner remains authoritative
for this lifecycle fixture. This does not wire the unfinished `frd run` CLI into
a complete installed desktop host and does not change existing validation gates.
