Plum v0.0.41 brings 56 development commits since v0.0.40, improving compiler
correctness, editor tooling, filesystem safety, and Windows I/O.

## Compatibility and migration

- **Streaming file reads use owned native buffers.** `File.read` keeps its
  independent Bytes result and positive-size requirement, releasing private
  capacity after each call. Use `File.read_into` with a retained `NativeBuffer`
  to reuse storage. Short-lived file-reading workers no longer retain scratch
  allocations. The internal `file_read_n`/`file_read_data` ABI is replaced by
  caller-owned `file_read_into`.

- **Owned native buffers support bounded mutable FFI.** `NativeBuffer` provides
  scoped text/binary edits, copies, growth and separate optional capacity
  diagnostics. Text edits return an independent validated String while the
  lease is still held. TCP/TLS reads now use caller-owned receive storage;
  `Net.read_into` permits capacity reuse and worker exit leaves no receive
  scratch allocation. Internal native receive functions use `*_recv_into`
  instead of pointer/count scratch APIs. `Net.read_bytes` requires positive
  capacity; zero is an error rather than an EOF signal.

- **CStr now owns its backing storage.** String/Bytes `.as_cstr()` shares
  the existing cell with normal Perceus ownership, making temporary, stored
  and returned values valid without a copy or wrapper allocation. Change
  native pointer return annotations from `CStr` to `RawCStr`; raw pointers
  retain their explicitly unsafe native lifetime. `Bytes.from_cstr` accepts
  RawCStr, and both pointer types require `unsafe` for `.as_string()`.
- **JSON serialization is now fallible.** `json_stringify(value)` returns
  `Result[String, String]`. Handle `Ok(text)` before writing or sending JSON;
  `Err("non-finite JSON number")` rejects NaN and either infinity, including
  nested values. JSON parsing, `String.parse_float`, and source float literals
  reject binary64 overflow. Underflow still rounds to subnormal values or
  signed zero. Use `Float.is_finite` to validate values before serialization.
- **Native byte copies require explicit unsafe calls.** Call
  `Bytes.from_cstr(pointer, length)` inside `unsafe { ... }`, supplying all
  arguments. It cannot be partially applied or used as a function value. For
  positive lengths, the caller must provide readable memory for the entire
  copy; the returned bytes own a fresh copy.
- **Invalid programs are rejected consistently.** Empty matches, incorrect
  generic type arities, invalid boolean/unary operands, and non-Unit branches
  in conditionals without `else` are rejected. When public types in multiple
  modules share a bare name, use a module-qualified name to resolve the
  ambiguity. Local, root, and prelude declarations retain their precedence.
- **Partial applications capture inputs once.** Supplied arguments and the
  callee are evaluated when the closure is constructed, rather than again
  each time it is called. Move repeated work into the closure body explicitly
  if that behavior is required.

- **Project symlinks stay within their declared root.** The default
  `symlinks: "within-root"` policy rejects absolute links, escapes, cycles,
  and duplicate physical sources. Use explicit path dependencies for external
  trees, or `symlinks: "reject"` to refuse source links entirely.
- **Linked Git packages use versioned hashes.** `plum fetch` prints
  `hash_version: 2` alongside `sha256`; copy both fields into the dependency.
  Existing link-free version 1 hashes remain valid. To migrate a package with
  links, fetch without its old hash, review the contents, and record both new
  fields. Package validation rejects escaping links and special files.

## Compiler and language server

- Lexer errors retain original source positions, unterminated strings are
  rejected, and interpolations are validated completely. CR and CRLF source
  positions are handled consistently.
- Guard temporaries are released when scoped match arms fall through, and
  traced call stacks remain local to each thread.
- LSP semantic requests use current buffer snapshots. Lifecycle and JSON-RPC
  validation reject malformed requests, and position validation handles large
  inputs without recursion proportional to their numeric magnitude.
- File URIs escape paths correctly and preserve UNC authorities.

## Formatting, documentation, and editor grammar

- Formatter indentation follows source order and handles tabs while preserving
  multiline literal contents.
- Documentation rendering escapes untrusted content, disambiguates generated
  page paths, and anchors output writes without following symlinks.
- The editor grammar supports function contracts, opaque handles, compile-time
  builtins, negative integer patterns, scientific literals, interpolation
  expressions, nested struct update paths, and all select arm forms. Bitwise
  expressions follow compiler precedence, and multiline strings retain their
  string tokens.

## Files, paths, and packages

- File handles avoid identity reuse and coordinate concurrent access and close.
  Streaming seeks preserve 64-bit offsets.
- Directory listing preserves ownership, reports read errors, and validates
  UTF-8. Source discovery, directory collection, and lexical path helpers use
  linear growth; UNC share roots are preserved.
- Project traversal is confined, recursive copies preserve symlinks, and
  changing a path extension cannot escape the original directory.
- Windows filesystem APIs preserve UTF-8 paths. Git dependency cache and vendor
  paths retain repository identity.
- `Os.is_directory` documentation now matches its missing-path behavior.

## Processes, concurrency, and time

- Process capture preserves output and reports capture failures. Process
  registries are synchronized, and Windows children inherit only intended
  handles. Windows process exit statuses retain all 32 bits, child stdin
  writes complete in bounded chunks, and console reads keep owned results.
- Finite child waits honor their duration across platforms. Channel select
  timeouts use monotonic deadlines.
- Windows TCP listeners reserve ports exclusively, and TCP/HTTPS share safe
  Winsock initialization.
- Wall-clock conversion retains millisecond precision, and calendar formatting
  enforces supported bounds across the full Int epoch range.

## Collections and JSON

- `String.index_of` searches in linear time without materializing characters.
  Empty-separator splits build one owning codepoint array.
- `Map.from_arrays` respects the shared prefix when input lengths differ.
- JSON parsing uses owned byte builders with bounded nesting. Decoder error
  paths distinguish literal object keys from nested paths.

## Installing

Release archives are built natively for Linux x86_64 and arm64, macOS x86_64
and arm64, and Windows x86_64. Each archive has a SHA-256 checksum.
**Clang is required to compile Plum programs.**

See [RUNNING.md](https://github.com/bradcypert/plum/blob/v0.0.41/RUNNING.md)
for setup and build-cache controls, and the
[full commit comparison](https://github.com/bradcypert/plum/compare/v0.0.40...v0.0.41)
for all changes.
