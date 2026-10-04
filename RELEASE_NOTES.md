Plum v0.0.37 adds Git dependencies, HTTPS, byte buffers, richer match patterns, and compiler fixes.

## Packages

Git dependencies name a repository, a full commit hash, and a SHA-256 content hash in `plum.pkg`. Run `plum fetch` to populate a shared cache; builds verify the hash and never fetch implicitly.

- `plum vendor` copies Git dependencies into the project's `vendor/` tree for builds without a cache or network.
- `plum cache list` and `plum cache clean` inspect and remove fetched packages.
- `plum doc` documents your project's own modules by default. Use `--with-deps` to include dependency APIs.

## Standard library

- **HTTPS:** `Http.get`, `Http.post`, and `Http.request` support HTTPS with hostname and certificate verification. TLS is bundled in the compiler, so building an HTTPS client does not require a separately installed TLS development library. Redirects remain the caller's responsibility.
- **Bytes:** a byte buffer for binary data, with file I/O, UTF-8 validation and conversion, hexadecimal output, and encoding helpers.
- **Terminal text:** `Terminal.display_width`, `Terminal.truncate`, and `Terminal.pad_right` operate on terminal display cells.
- **Option fallbacks:** `Option.or` and `Option.or_else` provide eager and lazy alternatives.
- **Random numbers:** `Rng` uses SplitMix64. The same seed remains reproducible within this version, but produces a different sequence from earlier versions.

## Language and compiler

- Match patterns support negative numeric literals and alternatives such as `A(x) | B(_, x)`. Alternatives must bind the same names with compatible types; guards run after an alternative matches.
- String literals support `\xNN` and `\u{...}` escapes. Unknown escapes and malformed hexadecimal or Unicode escapes are rejected with source positions.
- Empty `Array.fold` accumulators infer their element types from the callback.
- Project and dependency discovery skip dot-prefixed files and directories, including macOS AppleDouble companions and `.git`. Explicitly named hidden source files remain usable.
- Unexpected-character errors include the file, line, column, and caret. Non-printable characters are identified by Unicode codes such as `U+0000`.
- Interpolation blocks that do not end in an expression have a clearer diagnostic.
- Channel ends are retained correctly across spawned tasks and nested closure captures.
- `Array.sort_by` preserves the order of equal elements when given a `<=` predicate.
- `plum build` exits nonzero when compilation fails and derives the output name correctly when building `.` or `..`.
- The test runner collects only `test_` functions returning `Unit`.

## Bootstrap and validation

The standard library is maintained as Plum source and embedded into the compiler with `@embed_file`. LLVM constant and shim embedding use fewer allocations, and builds avoid compiling unused TLS support.

Seed generation writes to a scratch file before replacing the checked-in seed, so an interrupted generation cannot truncate it. CI shares bootstrap artifacts and runs validation in parallel across Linux, macOS, and Windows.

The compiler still bootstraps from the checked-in LLVM seed with clang and no network. Release archives contain the compiler binary; clang is required to compile Plum programs.
