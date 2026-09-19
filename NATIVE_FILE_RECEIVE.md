# Native file receive

`fr-files::quic::HostReceiver` consumes the one-use `FilesChannel` on the
original native connection. Its caller services files before and after that
connection's ordinary I/O turn; it does not start another listener or runtime.
The dedicated bulk pair accepts only versioned file records after positive
file/input capability negotiation and completion of the original input attachment.
The disk owner still checks the actual input monitor and separately approved
local drop directory; a channel handle alone is never file permission.

Service processes at most one incoming record and one reply. Backpressure
retains the original record or reply and its original deadline, not a replayed
operation. Stopping or resetting files retires only that pair, preserves consumed
attachment IDs, and leaves control/input/clipboard available. Completed filesystem
outcomes remain inspectable after the worker exits or proof delivery fails.
Observation startup explicitly refuses this optional role rather than silently
installing it among the media channels.

The pinned compiler rebuilds the project-owned file/transport/daemon libraries
against unchanged retained upstream libraries. All 66 file tests and 37 native
attachment tests pass, including a 120,007-byte UDP/TLS transfer across the stream
receive-credit window, corrupt-content refusal, partial reset cleanup, foreign
connection rejection, and publication receipt retention. Strict Clippy passes
for those checked libraries/tests. These are explicit admission/controller
fixtures, not live Tailscale, hardware, installation, resumption or end-user UI
qualification. The normal application still has to negotiate and own this
optional channel, and select/send files through its local interface.
