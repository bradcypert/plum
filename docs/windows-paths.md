# Windows filesystem encoding

Plum filesystem strings use UTF-8. `native_stdlib/path_shim.h` owns the
Windows conversion boundary: strict UTF-8 becomes UTF-16 before wide CRT
or Win32 calls, and returned UTF-16 paths become strict UTF-8. There is no
ANSI fallback, replacement-character decoding, locale dependence, or
successful truncation. Invalid encoding returns the existing failure
sentinel, which public wrappers report as `Err`. POSIX path bytes retain
their existing behavior.

This covers streaming and whole-file reads/writes, metadata and existence,
directory enumeration/creation, removal/replacement, cwd, temporary paths,
executable location, tool lookup, and cache path I/O. Tree traversal uses
the same strict input policy while retaining its handle-relative security
rules. Process capture files also use wide APIs and UTF-8 stored paths;
otherwise the shared whole-file reader could not read those files under a
Unicode temporary directory.

The adapter is embedded alongside the C shims, marked as a header rather
than a compilation unit. The generator, cache fingerprints and seed shim
hash include it. Installed compilers therefore need no checkout headers.

## Ownership and memory

Each input conversion allocates exactly the required UTF-16 storage,
including its terminator, and frees it before returning from the native
operation. A two-path replacement briefly owns two conversions. Failure to
allocate or decode returns an error before using that path; partial
conversions are freed. No converted-path cache or process-global growing
buffer is introduced. Peak additional conversion memory is O(path length),
independent of file size and operation count. Existing filesystem path
limits remain in force.

A Windows directory stream owns one reusable `4 * MAX_PATH` byte UTF-8
entry buffer (1,040 bytes with the supported MinGW headers) in addition to
its native wide stream. Reading each entry allocates no new shim storage;
closing the stream releases both. Public `Os.list_dir` still copies entries
into its returned strings, so its output memory remains proportional to the
names returned. Returned cwd/temp/executable paths continue to use bounded
thread-local UTF-8 buffers, with bounded stack UTF-16 scratch during calls.

This is a correctness change with a small Windows conversion cost. It does
not reduce file payload memory, change reference counting, implement precise
Perceus dropping (#349), or alter whole-file/capture buffering. Native Windows
tests inject allocation failures and check tracked scratch returns to zero
between operations, including repeated work before process exit. The tests
also assert no per-entry allocation. These are lifetime guarantees, not a
claim about total process RSS or compiler peak memory.

## Remaining boundaries

The runtime still receives narrow Windows `argv`. Unicode command-line
arguments need a separate UTF-16-to-UTF-8 entry-point change; invalid UTF-8
from that source now fails consistently at filesystem boundaries. The
native-object cache's conservative non-ASCII tool certification bypass
also remains in place. This change does not expand long-path support.

Embedded NUL validation remains #136 (#142 is its duplicate). Native path
parameters are already NUL-terminated, so conversion cannot recover a suffix
lost before reaching the shim. That needs validation at the length-aware
String/CStr boundary across filesystem, process and network APIs.

Windows filenames with unpaired UTF-16 surrogates have no valid UTF-8
representation and are reported as errors by directory enumeration.
