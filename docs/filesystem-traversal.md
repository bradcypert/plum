# Filesystem traversal design

Status: accepted direction; implementation and verification pending.

Implementation progress: the POSIX copy walker now uses held descriptors,
an explicit stack capped at 128 directories, and a single 64 KiB transfer
buffer. `bootstrap/copy-tree-smoke` exercises link preservation, destination
link rejection, hard-link-safe replacement, overlap rejection, a deterministic
source swap, enumeration errors, depth exhaustion, and descriptor cleanup
under address/undefined-behavior sanitizers. The POSIX root-relative entry
resolver also follows relative links through held descriptors, rejects
absolute/escaping targets, bounds link expansion at 40, supports rejecting
links, and returns owned entry descriptors. Its tests cover internal links,
cycles, dangling targets, strict policy, and repeated-failure cleanup.
The Windows adapters now use native handle-relative opens, reparse inspection,
relative-link resolution, and temporary-file replacement. Native Windows tests
cover junction rejection, hard-link-safe replacement, allocation failures,
Unicode entries, and supported symlink handling; they are wired into Windows
CI and platform-smoke. Their execution on Windows remains to be verified.
Hash migration, discovery, and overlay integrations remain pending; the
compiler does not yet consume the resolver.
This intermediate implementation is not ready for release: preserved links
must not be consumed by the old unrestricted compiler walks and overlays.

This design coordinates issues #186 (copying source links), #283 (copying
through destination links), #184 (copy enumeration failures), #176 (package
hashing), and #94 (source discovery). It also covers formatter discovery and
LSP snapshot overlays, which otherwise undermine the same boundary.

## User-visible contract

Project discovery supports `within-root` (default) and `reject`. Store the
choice in a proposed `symlinks` string field on `Package` in `plum.pkg`.
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

Add an explicit proposed `hash_version: 2` field to `Dep`; absence means the
legacy algorithm. Keep `sha256` as 64 hexadecimal characters. Reject unknown
versions. Fetch output must print the version with the digest.

Legacy verification preserves existing digests for ordinary link-free trees,
but refuses links with a migration diagnostic. It must never fall back to
following targets to reproduce an old digest. Do not automatically rewrite
the user's manifest or accept either digest after a mismatch.

Version 2 hashes typed, length-framed records in deterministic relative-path
order. Paths use `/`. File records contain their streamed content digest;
link records contain their exact stored target bytes. Directory records make
empty directories explicit. Define the byte encoding, integer framing,
ordering, exclusions, and domain-separation prefix in fixed test vectors
before shipping. File modes remain excluded and documented. All entries
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
tracking grows with discovered source count. Deterministic hashing needs
sorted entry metadata: account explicitly for its path/name storage, rather
than claiming the entire operation has constant memory. Stream record
digests into the final hash instead of retaining an Array[Int] of all hashes.

Native allocations and OS handles are outside Plum's ordinary reference
count counters. Measure them separately with allocation tracking, descriptor
counts, sanitizers, and peak RSS. Repeated failures must return to baseline.
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
