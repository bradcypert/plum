Plum v0.0.39 fixes build-cache cleaning during concurrent runs.

`plum cache clean build` now atomically moves the existing cache aside before
removing it. Concurrent cache users and cleaners can no longer remove entries
from the tree being traversed and cause a spurious cleaning failure. Runs
started concurrently may populate a new cache. Fetched packages are preserved.

This patch retains the validated IR and object caching introduced in v0.0.38.
To compare warm cached and uncached runs with an isolated cache:

```sh
PLUM_CACHE=/tmp/plum-cache-test PLUM_CACHE_TRACE=1 plum run myapp
PLUM_CACHE=/tmp/plum-cache-test PLUM_CACHE_TRACE=1 plum run myapp
PLUM_NO_BUILD_CACHE=1 plum run myapp
PLUM_CACHE=/tmp/plum-cache-test plum cache clean build
```

See [RUNNING.md](https://github.com/bradcypert/plum/blob/v0.0.39/RUNNING.md)
for cache behavior and controls. Use a trusted local cache directory.

Release archives contain the compiler binary; clang is required to compile
Plum programs.
