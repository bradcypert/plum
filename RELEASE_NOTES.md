Plum v0.0.40 brings 40 development commits since v0.0.39, focused on correctness,
resource ownership, safer I/O, and faster native builds.

## Compatibility and migration

- **JSON serialization can fail.** `json_stringify(value)` now returns
  `Result[String, String]`. Match `Ok(text)` before sending or writing it;
  `Err("non-finite JSON number")` rejects NaN and either infinity, including
  inside arrays and objects. JSON parsing, `String.parse_float`, and source
  float literals also reject binary64 overflow. Underflow still rounds to
  subnormal values or signed zero. `Float.is_finite` is available to validate
  values before constructing JSON.

- **Channels report disconnection.** `Receiver.recv()` now returns `Option[T]`:
  match `Some(value)` for a message and `None` after the final sender drops and
  buffered messages have drained. `Sender.send(value)` now returns
  `Result[Unit, T]`: `Err(unsent)` returns ownership of the payload when there
  are no receivers. Unit-returning producers must handle the result or
  explicitly discard it with `tx.send(value); ()`.
- **Select receive bindings also contain `Option[T]`.** Keep the
  `received = rx => ...` binding and match `Some`/`None` inside the arm.
  Refutable receive patterns such as `Some(value) = rx` are rejected. Remove
  closed receivers from subsequent selects: they remain ready, and ties use
  arm order. See the updated [concurrency example](https://github.com/bradcypert/plum/blob/v0.0.40/examples/concurrency/main.plum).
- **Invalid or ambiguous input is rejected earlier.** JSON parsing rejects
  duplicate object members, and `Json.int` rejects values outside the binary64
  safe integer range, `-9007199254740991` through `9007199254740991`. Send
  larger integer identifiers as strings. Base64 decoding rejects
  malformed padding and nonzero unused tail bits. Package manifests reject
  duplicate fields and conflicting dependency namespaces. HTTP and URL
  validation is stricter; malformed messages that previously slipped through
  can now fail.
- **Seeded floating-point RNG samples may change.** `Rng.float` now uses 53
  exactly representable bits to keep its result below 1.0. Existing integer
  sequences are preserved for range widths through the maximum Int.

## Concurrency and memory

- Channel receivers and selectors wake when the last sender disappears;
  dropping the last receiver releases queued payloads. Task/channel failure
  paths and detached task results preserve ownership, and discarded loop
  results are released. Channels remain unbounded while consumers are live.
- Heap reference counts are thread-safe. Native handle cleanup tracking grows
  beyond its former fixed capacity and synchronizes cleanup claims.
- Generic channel types remain scoped correctly, and generic factories with
  Unit arguments instantiate correctly.

## Files, processes, and platform support

- Whole-file writes use atomic replacement to preserve existing contents on
  failure. Symlinks and special-file destinations are rejected; replacement
  changes file identity and does not preserve hard links, ownership, ACLs, or
  other metadata. On Unix, new files use mode 0600 and replacements preserve
  ordinary permission bits. Whole-file reads reject incomplete reads; text reads
  reject invalid UTF-8, while byte reads preserve binary data. Nonpositive read
  sizes fail explicitly.
- Recursive removal anchors traversal to held directories to prevent symlink
  traversal. Recursive copies reject overlapping source and destination paths.
- Windows process creation and environment overlays preserve Unicode, path
  handling preserves drive-relative paths, and socket I/O bounds native lengths.
- OS path results are safe under concurrent calls, truncated executable paths
  are rejected, and environment values are validated before becoming strings.
- Empty stdin reads return immediately, and timed waits avoid deadline overflow.

## HTTP and URLs

- HTTP bodies preserve arbitrary bytes, and transport reads/writes handle
  partial I/O. Response handling respects request methods and status codes,
  including HEAD and responses without bodies.
- Request heads and message framing are validated before reading bodies or
  dispatching handlers. Invalid fields are rejected before serialization,
  and URL components use bounded UTF-8 validation.

## Collections, numbers, and JSON

- Arrays use stable merge sort and bounds-check updates and removals. Map
  hashing follows structural key equality, so equal keys hash consistently.
- Seeded integer RNG ranges support all ordered Int bounds, including widths
  that overflow signed subtraction. `Float.abs(-0.0)` produces positive zero.
- Replacing an empty search string leaves the original string unchanged.
- JSON serialization accumulates output linearly and uses explicit traversal.
- `Float.to_fixed` documentation now states its existing 0–17 decimal-place
  precision limit.

## Compiler and tooling

- Native builds select embedded TLS from declared native dependencies, avoiding
  unnecessary TLS compilation. Windows reuses validated native objects;
  concurrent temporary-directory creation is safer. macOS debug builds compile
  native C inputs before linking.
- Unsupported target operating systems are rejected explicitly.
- LSP formatting fixes document ownership and replacement ranges. `plum doc`
  exits with an error when its output cannot be written.

## Installing

Release archives are built natively for Linux x86_64 and arm64, macOS x86_64
and arm64, and Windows x86_64. Each archive has a SHA-256 checksum and contains
the compiler, README, and license. **Clang is required to compile Plum programs.**

See [RUNNING.md](https://github.com/bradcypert/plum/blob/v0.0.40/RUNNING.md)
for setup and build-cache controls, and the
[full commit comparison](https://github.com/bradcypert/plum/compare/v0.0.39...v0.0.40)
for all changes.
