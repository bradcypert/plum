Plum v0.0.38 adds validated build caching and reduces cold compilation costs.

`plum run` reuses validated LLVM IR. Supported Linux Clang invocations also
reuse compiled objects; every run links a fresh executable. Source files,
dependencies, embedded files, compiler contents and build modes determine
reuse. Corrupt entries are repaired, failed edits never run an older program,
and cache I/O failures fall back to compilation.

Build artifacts live under `PLUM_CACHE/build/v1`, defaulting to
`$XDG_CACHE_HOME/plum/build/v1` or `~/.cache/plum/build/v1`. The default
retention budget is 1 GiB; `PLUM_BUILD_CACHE_MAX_BYTES` overrides it.

To compare cached and uncached runs of the same release:

```sh
PLUM_CACHE_TRACE=1 plum run myapp
PLUM_CACHE_TRACE=1 plum run myapp
PLUM_NO_BUILD_CACHE=1 plum run myapp
```

`PLUM_NO_OBJECT_CACHE=1` keeps IR reuse while bypassing object caching.
`plum cache list build` inspects artifacts, and `plum cache clean build`
removes them without removing fetched packages. Use a trusted local cache
directory: integrity hashes detect corruption but do not authenticate another
writer's artifacts. Cached IR can retain embedded file contents until eviction
or cleaning.

Cold compilation also avoids repeated documentation decoding and repeated
helper discovery. Compiler fixes preserve unnamed `Unit` parameters and share
validated match patterns across compiler phases.

See [RUNNING.md](https://github.com/bradcypert/plum/blob/v0.0.38/RUNNING.md)
for cache behavior and controls, and the
[measurements](https://github.com/bradcypert/plum/blob/v0.0.38/docs/issue-63-results.md)
for the local performance tradeoffs. Object validation has a cost, so initial
cache population can be slower than an uncached run.

Release archives contain the compiler binary; clang is required to compile
Plum programs.
