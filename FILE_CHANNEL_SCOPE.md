# Channel-derived file scope

`file-channel-scope` version 1 selects a controller-to-host drop convention on
an independently admitted `file-atp-full` Files attachment. The host maps that
one-use attachment to an explicitly approved local drop directory. For this
convention the file envelope's u128 endpoint handle is the completed attachment's
nonzero u32 channel-binding ID, widened without changing value. Both peers derive
it from the authenticated role-specific binding/ticket exchange, never from a
path in FileOffer or an unverified proposed record. No new record layout is used.

An implementation using this convention MUST require positive selection of the
capability and the existing attachment/Files/ATP capabilities, the original
controlling session and input lease, the full selected display/view binding,
and independent local file permission. No disk receiver starts before attachment
completion, and no sender reads a source before an explicit local file selection.
The handle names the selected endpoint; it is not a permission or filesystem path.
The original setup deadline covers pre-poll idle and transport backpressure.

The explicit pre-agreed-handle APIs remain available for other application file
scopes. Applications MUST use matching conventions on both endpoints; selecting
this drop workflow requires no out-of-band numeric handle. It does not enable
read-only transfer, remote browsing, source-path requests, downloads or resume.
The convention is not advertised by a client/host workflow that cannot service it.


The implementation entry points and verification scope are described in
[NATIVE_FILE_SEND.md](NATIVE_FILE_SEND.md#drop-directory-scope-without-prearranged-handles).
