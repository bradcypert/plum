---
name: plum-ci
description: Prepare Plum changes for a PR or diagnose failing Plum CI, including bootstrap seed freshness, generated artifacts, and source-built compiler checks. Use before pushing Plum changes and when a Plum Actions run fails.
---

# Plum CI

Run commands from the repository root. Treat `.github/workflows/ci.yml` and
its invoked harnesses as the current check inventory; select checks from the
changed files and their generated consumers, rather than only the bug's
behavioral test.

## Bootstrap and generated artifacts

The compiler embeds native sources. Passing a C test or manually linking
current shims proves runtime behavior, but does not prove that the checked-in
seed contains those sources or that a fresh clone can build correctly.

For changes to `native_stdlib/`, vendored TLS inputs, or compiler bootstrap
behavior:

1. Run `bootstrap/check-shims`. `bootstrap/gen-shims` maintains the embedded
   file manifest; with `@embed_file`, editing an existing C file may leave
   that manifest unchanged. Adding or removing inputs can require generation.
2. Build a compiler from current source. The checked-in `./sh.real` may have
   stale embedded shims. Prefer CI's seed-to-source path, with outputs under
   a task-specific temporary directory:

   ```sh
   ./bootstrap/from-seed -o /tmp/plum-ci-seed
   /tmp/plum-ci-seed build bootstrap/self_host -o /tmp/plum-ci-compiler
   ```

3. Run the seed gate:

   ```sh
   PLUM=/tmp/plum-ci-compiler ./bootstrap/check-seed --seed-compiler /tmp/plum-ci-seed
   ```

   Its native-source fingerprint covers content, even comments, not merely
   ABI changes. An existing shim edit can require a seed refresh even when
   `check-shims` passes.
4. If that check reports a stale seed, regenerate using the source-built
   compiler: `PLUM=/tmp/plum-ci-compiler bootstrap/gen-seed`. Include both
   `bootstrap/seed/plum.ll` and `bootstrap/seed/shims.sha256`. Rebuild the seed
   executable from the regenerated IR and rerun `check-seed` against it.
   Updating the fingerprint alone does not refresh embedded sources.

If the source build fails because the bootstrap executable embeds old
shims, use the documented seed path or explicitly link current-source IR
against current native shims. Record the workaround, then still verify the
regenerated seed through `check-seed`; a manual link is not that proof.

Follow root `AGENTS.md` for property-test and documentation requirements.
Use the source-built compiler for relevant public API fixtures as well as
native shim tests, so tests exercise the embedded implementation users get.
Retain guard memory limits and choose a build timeout appropriate to a whole
compiler build; `./sh` defaults to a short timeout.

## CI diagnosis and completion

Locate the first failed job and step for the PR's current head. Aggregate
jobs often fail because a dependency failed, and skipped jobs provide no
validation. Distinguish these from independent test failures.

Compare the failed command with local validation. Reproduce it, fix the
underlying source or generated artifact, and rerun that gate before pushing.
Capture a new lesson only when it changes future decisions; keep exact
commands in their harnesses instead of copying the entire workflow here.

After pushing, inspect the new run. Report which checks passed, failed, or
remain pending. A local workaround and a successful native test do not make
a pending or skipped bootstrap gate green.
