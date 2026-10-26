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

## Portable trees with explicit directory entries

Profile 3 (`ATP_PORTABLE_DIRECTORY_ENTRIES`) preserves nested empty directories
using the pinned ATP `TransferManifest` and its existing structural metadata.
This profile uses Asupersync 0.6.0 at the repository's existing revision
`ee72b665ee87f6c811b1631136676fbc31ec0a88`; it adds no dependency or alternate
hashing/transfer implementation.
The sender selects it only when the selected tree needs explicit directory
entries. Ordinary content-only trees and an empty selected root retain profile 2.
An offer of profile 3 requires a matching profile-3 `FileAccept` before data;
an older receiver may refuse it, but the sender never silently drops directories
or downgrades the selection to profile 2.

An explicit directory is an empty leaf; its ancestors are implicit. No explicit
entry may also be an ancestor of another entry, in either manifest order.
The directory has size zero, the upstream empty-content SHA-256, no
packed members, and `EntryMetadata` containing only `FileKind::Directory` with
every other field at its default. Regular-file metadata must be absent; the
commitment uses ATP's default regular-file metadata for those entries.
The manifest's `metadata_root_hex` must match ATP's existing
`metadata_commitment` over **all** entry paths and their effective metadata,
including default regular-file metadata when the field is absent. The receiver
checks that commitment before creating staging. It also verifies the existing
flat content Merkle root before publication. This keeps a zero-byte file and an
empty directory distinct even though they have the same content hash.

No permissions, ownership, timestamps, extended attributes, link targets,
directory-root metadata, packed members, delta or resumption fields are admitted.
Received directories use private local permissions. Directory indices never
accept `ObjectData`; there are no marker files and no directory content reads.
The private tree, original authority checks, cleanup ownership and single atomic
no-replace publication are the same as profile 2.

The existing bounds apply to the complete structure: at most 64 explicit
manifest entries (regular files plus explicit directories), 128 non-root nodes,
eight path components, 1024 bytes per path, 16 KiB of combined names and a 32 KiB
manifest within the negotiated record limit. Empty entries still consume entry,
name, node and metadata-work budgets. Declared content bytes count only regular
file contents. As in upstream ATP, `Proof.files` counts **all explicit manifest
entries**, including the structural directories; it is not a regular-file-only
counter. An all-empty nested tree still needs verification and a publication
receipt with zero content bytes.

## Native explicit file and directory selection

`fr connect NODE --control --experimental-native --display only --send PATH`
now selects either a regular file (profile 1) or a directory root (profile 2 or
3, depending on whether explicit directory entries are required).
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
Empty roots, empty regular files and nested empty directories are supported by
the appropriate profile. Other exclusions above still refuse the selection.
These profiles do not provide picker, delta or resumable-transfer UI.

Regression targets include `fr-files --test directory_batch`, native selection
unit tests, and the controlled-loop
`selected_directory_uses_the_native_controlled_owners_and_keeps_input_live` test.
These distinguish source/unit/fixture coverage from real graphical desktop or
live-tailnet qualification; selecting a directory does not establish either.
The native command targets additionally check mixed and all-empty trees in
`real_files/trees.rs` and `real_downloads.rs`, with independent filesystem checks,
content-free receipts and continued input on the original controlled session.

## Host-to-controller downloads

Without duplex selection, the optional `file-download` v1 capability fixes an
attached native Files lane's direction as `ToController`. Its absence retains
the original `ToHost` direction. Download-only native offers replace
`file-channel-scope` with this capability, preserving the existing behavior of
older peers.

The optional `file-duplex` v1 extension admits exactly two role-specific file
attachments on the same original controller: `Files` is `ToHost`, and
`FilesDownload` (role 9) is `ToController`. It requires positive selection of
`file-atp-full`, `file-download`, `file-channel-scope`, `native-file-receive` and
`native-input-attachment` and `native-media-attachment`, as well as the original
control role. Both old direction
capabilities without this complete profile do not authorize two attachments.
The retained selection and exact authenticated attachment role determine direction
before parsing any application payload; the first file record cannot change it.
Each role spends its own binding, ticket and channel-derived handle once.

Both owners preserve independent local permissions, finite source selections,
destination quotas, publication receipts and cancellation. A completed role may
retire without resetting its sibling. Ordinary controller turns service input
and renewal first, then visit each bounded file owner once with alternating first
service. Family shutdown retains both owners until their original cleanup has
completed. Reconnect does not carry a source selection, accepted destination,
permission or uncertain result into another attempt.

The fullest native clipboard/audio/duplex offer stays within the existing 24
capability ceiling by omitting only optional attachment-credit negotiation when
necessary. That profile uses the existing bounded fixed-allowance attachment
path; it does not enlarge queue, record, stream, rate or deadline ceilings. A
peer whose actual limits cannot admit the optional attachment still refuses it.

The host chooses a finite local source selection and the controller chooses a
private local destination. Both derive the nonzero scope handle from the
authenticated completed channel binding. The receiver's authority comes from
the original viewer input owner and uses conservative host-clock projection;
it does not manufacture a host-side input session. Existing ATP profiles, byte
and count limits, one-use attachment, lease checks, atomic publication and
explicit unknown-effect outcomes apply in either direction.

`frd run --input-agent PATH --once --send PATH` and
`fr connect NODE --control --receive DIR` expose this workflow. See
[NATIVE_FILE_DOWNLOAD.md](NATIVE_FILE_DOWNLOAD.md) for selection, cancellation and
retained-receipt semantics.
