# Filesystem traversal design

Status: implemented; final platform verification in progress.

The native adapters, compiler consumers and integration tests implement the
contract below. `bootstrap/copy-tree-smoke` covers ownership, link handling,
replacement races, resource limits and injected failures; platform-smoke runs
the Windows adapter on Windows. `bootstrap/tree-memory-check` measures native
allocation bounds separately from compiler allocation counters.

This design coordinates issues #186 (copying source links), #283 (copying
through destination links), #184 (copy enumeration failures), #176 (package
hashing), and #94 (source discovery). It also covers formatter discovery and
LSP snapshot overlays, which otherwise undermine the same boundary.

Source discovery now shares a held root between its source records, uses a
bounded explicit stack, and rejects physical aliases/cycles across project
and dependency roots. Source parsing and cache hashing reopen relative to
that root and verify the discovered identity before reading the held entry.
The native identity index grows geometrically and retains identities and
first logical paths, not one descriptor per source. Parser reads still need
whole-source storage; streaming hashes do not. Windows directory iterators use independent handle-relative opens.
Formatting now shares the bounded source inventory, including its existing
hidden/vendor inclusions. Explicit files and selected project subdirectories inherit
the nearest declared project policy and dependencies, while preserving the
selected subtree, module layout and logical path spelling. Dependency roots
remain independent. Native C selection uses the same held-root boundary. Writes resolve the regular target,
verify its discovered identity, and publish an exclusive temporary through
the held parent, preserving traversed symlinks and unrelated hardlinks.
POSIX target permission bits are retained. Native tests cover a target swap
before publication, stale identities, and Windows publication allocation
failure; the formatter integration harness runs in platform-smoke.
Native C discovery now uses a bounded stack and an identity index, preserves
platform selection/order, and reports enumeration failures. It passes paths
to a trusted native compiler: that compiler's includes, assembly and file
access are not sandboxed. Names-only discovery defers link inspection until
a consumer has applied exclusions, while package integrity inventory still
reads link records. Manifests are read through held roots too: dangling or
escaping manifest links are errors, and `reject` also rejects a linked
manifest. The writer supports exclusive creation of missing confined targets
for unsaved buffers; a racing creator is never overwritten.

LSP semantic requests clone the nearest declared project root and all resolved
dependency roots into one owned temporary container. Temporary manifests rebind
the dependency graph to those copies; original manifests and dependency files
are untouched. Snapshot construction and manifest parsing run in a worker so
malformed projects cannot terminate the server. Live buffers overlay resolved
regular targets, including missing targets behind confined dangling links.
Conflicting texts for one physical target are refused; identical aliases are
allowed. Original source identities are checked after copying because copying
breaks hardlinks. Diagnostics and definitions map back through both root and
buffer-alias mappings. Manifest buffer edits must be saved before semantic
analysis; graph resolution uses saved configuration. Request cleanup owns the
whole container on success and failure. Internal commands transport path bytes
through ASCII envelopes rather than Windows narrow argument conversion.

Snapshots remain rebuilt per request. Streaming copy limits file-content
working memory, but copying the whole declared project and dependency closure
can increase disk I/O and temporary disk usage compared with copying only the
requested directory. The server retains open buffers; serialization and the
worker's decoded buffers add transient memory proportional to all open buffer
text, including buffers later excluded by root selection. Conflict indexes
retain identities, SHA-256 digests and first paths, rather than additional
copies of source text. Analysis still retains its usual whole-source and AST
storage. There is no persistent snapshot or semantic cache in this change.

Cache layout discovery and usage scans now use bounded, non-following
inventories. Each package is scanned once for both row and summary totals.
Regular files contribute their metadata size; links contribute stored target
text bytes, including dangling or external links. No target is opened or read.
Fetch and vendor reserve exclusive staging containers and publish only the
validated payload. Ownership markers remain outside that payload; cleanup
never removes a predictable sibling owned by another operation. Concurrent
publication accepts an existing destination only after integrity validation.
Tests cover concurrent fetch/vendor, preservation of pre-existing `.partial`
directories, failed-checkout cleanup, cache cycles and excessive depth.

Cache usage retains directory metadata for its bounded active stack and the
list of package roots. It reads file-size metadata without allocating source
contents. Unique staging adds small per-operation directories and ownership
markers; concurrent writers can temporarily duplicate a full package on disk.

Manifest discovery probes one child without following it, rather than
allocating a complete directory inventory for every ancestor. A dangling link
is present and subsequently fails its confined read; it is not silently
interpreted as a missing manifest. Ancestor lookup errors at its depth bound.
Manifest cache hashes also read through confined entries and include the
actual inherited manifest for a selected subtree. Shared path utilities
preserve native filename bytes and compare Windows path components without
using offsets into case-folded Unicode strings.

Diagnostic excerpts and debug line tables read through registered source
capabilities and recheck discovered identity. Their registry retains shared
roots and path/policy/identity metadata until the next parse or process exit;
it does not cache source contents or hold one file descriptor per source.
Cache publication rechecks observed source digests through these capabilities
using streaming hashes. Only explicitly embedded input paths are persisted
in the cache read manifest; ordinary sources are already in its confined key.
Explicit `@embed_file` declarations retain their existing ability to read
outside the project. Generated native intermediates and build-cache records
also remain outside source traversal policy. These are separate trusted inputs,
not discovered Plum source paths.

Filesystem errors carry consumer path context and a captured refusal reason,
POSIX errno, Win32 error or native NTSTATUS. The native reason uses a bounded
512-byte thread-local buffer and is captured before cleanup can overwrite it.

## User-visible contract

Project discovery supports `within-root` (default) and `reject`. Store the
choice in the `symlinks` string field on `Package` in `plum.pkg`.
Unknown values are errors. Every project and dependency has its own root;
a consumer's setting cannot authorize a dependency to escape its root.
All commands, including check, build, run, test, fmt, and LSP requests, use
the same project policy. A project without a manifest uses `within-root`.
An external shared source directory belongs in an explicit path dependency.

`within-root` resolves relative links component by component from the held
root. A `..` that would ascend above that root is an error, even if later
components would return inside. Absolute links are rejected for project
consumption, including absolute links that happen to name this checkout.
Cycles, unresolved source links, unsupported reparse points, and resource
limits produce diagnostics naming the entry and operation. Do not silently
turn an enumeration or metadata error into an empty directory.

`reject` reports links encountered in the selected source traversal.
Existing exclusions (hidden directories, the project's vendor directory,
and other command-specific exclusions) apply before entering a subtree.
Package integrity validation has a separate, complete entry inventory,
excluding only the documented metadata exclusions such as `.git`.

Copying preserves symbolic links and their target text without opening the
targets. This includes dangling links in a general-purpose copy. Package
validation rejects absolute and escaping links before publishing a vendored
tree or consuming a fetched package, even when no digest was declared.
On platforms where a link cannot be faithfully created, return an explicit
error; never silently replace it with target contents or a text file.
Windows junctions and arbitrary reparse tags are not portable symlinks and
are rejected in the initial implementation.

Existing destination symlinks and reparse points are always rejected.
Regular destination files are replaced using exclusive temporary siblings
and relative rename, rather than truncated in place. This also prevents
writes through destination hard links. Copying is not transactional across
the whole tree: a failure may leave partial output. Vendoring and snapshots
own staging directories and must discard them on every failure.

## Shared mechanism, separate responsibilities

Introduce an internal native filesystem module with opaque, owned root and
entry handles. It owns non-following lookup, enumeration, identity, link
text access, bounded resolution, and stream access. Platform adapters supply
POSIX descriptor-relative operations and Windows handle-relative operations.
Do not expose a checked path and then reopen it through an ordinary string
API: that would reintroduce the race between validation and use.

Keep copy policy, package serialization, module assignment, and formatting
outside the platform adapters. Share primitives and traversal invariants,
not a callback with branches for every compiler operation. Handle release
must work on early return, allocation failure, and cancellation. Errors
carry the operation, logical path, and platform cause.

Acquire the caller-selected root once. Ancestors in that supplied path may
resolve normally; policy applies below the acquired root. Define root-link
handling explicitly in the public copy API, including trailing separators.
Overlap detection uses opened directory identities and ancestry, not only
textual path prefixes. Repeated aliasing must not bypass overlap rejection.

For following internal links, use a bounded component resolver anchored at
the root. Never use realpath followed by a pathname open as a confinement
check. Windows must inspect reparse data from opened handles and resolve
supported link targets through the same root boundary. Unknown tags fail.
Held handles protect against replacement by a link; they do not promise
an immutable filesystem snapshot or confinement against arbitrary hostile
directory renames, mount changes, or privileged filesystem mutation.

## Discovery and file aliases

Track directory identity along the active traversal path for cycle detection.
Use a separate identity-to-logical-path map for source files. Discovering the
same physical source through two logical paths is an error naming both paths;
do not arbitrarily pick a module based on enumeration order. This applies to
hard links too. Directory alias expansion needs a bound so a small graph
cannot generate exponentially many logical paths before an error is found.

Preserve existing module assignment independently of identity checks. Read
source contents through the validated entry handle, and retain the logical
path for diagnostics. Native source enumeration and compiler-owned asset
reads need the same boundary review. A general native compiler or user code
is not sandboxed by this traversal policy.

Formatting uses the same inventory and refuses ambiguous source aliases.
Its writes operate relative to validated directories and replace regular
entries atomically. It must not replace the symlink itself when the intent
is to edit an internal target.

## LSP snapshots

Snapshot the complete declared project root so a link valid for compilation
remains valid in the snapshot. The current per-file parent directory is not
an adequate root for links reaching sibling modules.

Resolve open-buffer aliases against the original root and map overlays to
their corresponding regular entries in the snapshot. Never overlay with an
unrestricted write through a copied link. Conflicting open buffers for one
physical source produce a diagnostic rather than order-dependent content.
Preserve URI-to-logical-path mappings for diagnostics and definitions.
Close all snapshot handles and remove scratch output on every failure.

## Package hash migration

The `hash_version: 2` field on `Dep` selects the new format; absence means the
legacy algorithm. Keep `sha256` as 64 hexadecimal characters. Reject unknown
versions. Fetch output must print the version with the digest.

Legacy verification preserves existing digests for ordinary link-free trees,
but refuses links with a migration diagnostic. It must never fall back to
following targets to reproduce an old digest. Do not automatically rewrite
the user's manifest or accept either digest after a mismatch.

Version 2 hashes records in ascending relative-path byte order. Paths use
`/`. The hash input starts with the ASCII bytes `plum-tree-v2\n`. Each record
is a one-byte type (`F`, `D`, or `L`), the decimal path byte length, a newline,
the path bytes, a newline, the decimal payload byte length, a newline, and
the payload bytes. Decimal lengths have no leading zeroes. A file payload
is its raw 32-byte SHA-256 digest; a directory payload is empty; a link
payload is its target text. Windows targets are decoded from UTF-16 to UTF-8
with separators expressed as `/`. POSIX target bytes are preserved.
The root itself and entries named `.git` are excluded. Empty directories
are included. File modes remain excluded and documented. All entries
inside the root are hashed independently, so changing an internal target
changes the tree hash even though a link record hashes only its target text.

Git checkouts which substitute regular text files for symlinks cannot be
treated as equivalent verified packages. Detect unsupported checkout/link
capabilities and report them; never infer link types from file contents.

## Memory and resource contract

Stream file data through one reusable transfer/hash buffer. Neither copying
nor hashing may allocate memory proportional to the largest file. Replace
the current hashing path that reads an entire file and constructs another
buffer containing framing and contents.

Use an explicit traversal stack and a documented depth ceiling (initially
128 directory levels). Bound link expansion and directory alias work too.
Active directory handles and traversal state grow with depth. Source identity
tracking grows with discovered source and directory count. Deterministic hashing needs
sorted entry metadata: account explicitly for its path/name storage, rather
than claiming the entire operation has constant memory. Stream record
digests into the final hash instead of retaining an Array[Int] of all hashes.

Native allocations and OS handles are outside Plum's ordinary reference
count counters. Measure them separately with allocation tracking, descriptor
counts, sanitizers, and peak RSS. Repeated failures must return to baseline.
The reproducible `bootstrap/tree-memory-check` fixture copies and hashes
8 MiB and 128 MiB files. Both copies peak at 67,584 bytes of native-owned
heap allocations on Linux; hashing allocates no heap and uses its 64 KiB
stack buffer. The instrumented process peaks around 2.4–2.6 MiB RSS for the
combined fixture. A synthetic index of 1,024/2,048/4,096 fixed-length
identities and logical paths retains 73,760/147,488/294,944 native bytes.
These totals exclude libc's directory/stdio allocations and allocator
bookkeeping; RSS includes libc, stack and instrumentation overhead. They
are native-operation measurements, not whole-compiler memory estimates.
The harness checks zero live owned allocations after each operation and
injects failures during identity-table creation/growth and string copying.

This change does not solve unrelated compiler ownership or cache retention.

## Delivery and acceptance

1. Implement and test the native root/entry primitives on POSIX and Windows.
2. Migrate copying and enumeration error handling with deterministic swap,
   overlap, destination hard-link, and fault-injection regressions.
3. Add streaming versioned hashing, legacy compatibility fixtures, and
   package validation before publication and consumption.
4. Migrate source/native discovery, formatting, and snapshot overlays with
   the manifest policy and alias diagnostics.
5. Run integration fixtures for internal, escaping, absolute, dangling,
   cyclic, chained, and unsupported links; trailing separators; Unicode
   names; large/wide/deep trees; and all cleanup paths.
6. Regenerate public reference documentation with a current-source compiler.
   Run focused native tests, cross-check, platform-smoke, pkg-check,
   lsp-smoke, fmt-check, property-check, check-docs, and bootstrap gates.

The implementation PR must report measured memory and handle bounds,
platform validation performed, migration behavior, and remaining limits.
Do not close the linked issues on the strength of this design document.

## Windows filename compatibility

Directory enumeration and stored link targets use UTF-8 at the native
boundary. Existing Windows runtime arguments can contain native ANSI code
page bytes. The tree adapter first decodes strict UTF-8 and, only on invalid
UTF-8, accepts strict native-code-page input to preserve that existing
filename-byte contract. It does not substitute replacement characters.
This does not redesign the runtime's narrow argv handling or remove its
existing code-page limitations; general Windows argv normalization remains
outside this traversal change. Relative-path slicing preserves bytes on
both Windows and POSIX, including non-UTF-8 POSIX filenames.
