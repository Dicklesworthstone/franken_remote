# File authorization envelopes

This implements PROTOCOL.md kinds 0x0070 through 0x0074, without an ATP codec or
runtime dependency in fr-wire. The existing FRD0 header and extension rules apply.
All integers are big-endian. The fixed body prefix is session (u128), lease
(u128), approved local handle (u128), and transfer ID (u64). None is a bearer
credential: callers must supply the authenticated channel, direction and role.
Observers cannot use this lane. Complete record size F cannot exceed selected C.

Each variable ATP payload is prefixed by its u32 byte length. FileOffer adds a
u16 profile and its ObjectManifest. FileAccept adds profile (u16), size (u64),
bytes/second (u32), maximum data chunk (u32), concurrency (u16, exactly one),
and ObjectRequest. FileChunk carries the bounded ATP payload. FileComplete adds
publication disposition (u8), reason (u16), published bytes (u64), and ATP proof.
FileCancel adds a nonzero u16 reason. Complete encoding distinguishes durable
publication, publication with uncertain durability, refusal, and unknown effect;
a refusal cannot contain published bytes or a success proof.

Profile 1 names the pinned Asupersync 0.5.0 portable single-regular-file full
object subset. The runtime adapter must validate the inner ATP operation and
integrity before emitting success. The envelope does not enable resumption,
directory synchronization, file access, or a new network listener by itself.

Six new tests independently construct bytes for all five kinds, exhaust every
truncation point, and exercise binding/direction/handle refusal, complete-record
budgets, forged lengths and contradictory receipts. Source Cargo tests and strict
Clippy pass for fr-wire; this is not independent wire interoperability.

The native pair uses one logical nonzero binding for both unidirectional streams.
Both contexts must retain that same binding; role identifies the sender.

Profile 2 (`ATP_PORTABLE_DIRECTORY_FULL`) extends the same pinned ATP full-object
schema to a portable file tree. `FileOffer` must name profile 2 and contain
`is_directory = true`; the host echoes profile 2 in `FileAccept` before indexed
`ObjectData` can be sent. Profile 1 still refuses directories. A profile mismatch
is terminal for that file attachment, never an implicit downgrade.

The directory root is one portable basename inside the locally approved drop
root. Entries use contiguous indices and portable relative component paths,
with at most 64 files, 128 file/implicit-directory nodes, eight components per
path, 1024 bytes per path, and 16 KiB of combined names. The manifest is at most
32 KiB and must also fit the negotiated complete-record limit. Existing content,
rate, concurrency, session-attempt, and original-authority budgets still apply.
Every file SHA and ATP flat Merkle commitment is verified before one no-replace
rename publishes the entire tree. `Proof.files` is the actual manifest file
count, including empty files. Empty roots are supported; empty non-root
directories, filesystem metadata, links, packed entries, deltas and resumption
remain outside this content-only profile. This is explicit directory transfer,
not automatic synchronization or permission to browse remote paths.

## Native explicit file and directory selection

`fr connect NODE --control --experimental-native --display only --send PATH`
now selects either a regular file (profile 1) or a directory root (profile 2).
Repeat `--send` to mix up to eight selections in one connection. The host must
explicitly enable receiving with `frd run ... --input-agent PATH --files DIR`;
files do not grant control, and an observer cannot send them.

The native selector pins each root with a no-follow descriptor and verifies its
original type and identity. It does not walk directories or read file content
before connection admission. Only the existing authorized disk worker traverses
and hashes a tree, enforcing the profile limits above. An unsupported nested
entry refuses that selection rather than being skipped. Each tree counts as one
transfer, with its aggregate content bytes charged against the existing per-file
and cumulative session limits. Publication remains an atomic no-replace rename.
Selections run in order and stop on the first failure, retaining earlier receipts;
there is no automatic replay after an uncertain publication or reconnect.

The content-free completion still identifies selections by their zero-based
`index`, never by name. Successful `files_sent[].bytes` is the host-reported total
content size. For a directory whose size has not been measured,
`files_refused[].bytes` is JSON `null`, not an inode size or a fabricated zero.
Empty roots are supported; empty non-root directories and other exclusions above
are still refused. There is no download, picker, delta or resumable-transfer UI.

Regression targets include `fr-files --test directory_batch`, native selection
unit tests, and the controlled-loop
`selected_directory_uses_the_native_controlled_owners_and_keeps_input_live` test.
These distinguish source/unit/fixture coverage from real graphical desktop or
live-tailnet qualification; selecting a directory does not establish either.
