#!/usr/bin/env bash
# GUARD WRAPPER around the self-hosted binary (./sh.real).
#
# Rebuild with:  plum build bootstrap/self_host -o sh.real     (never -o sh)
#
# Uses a real cgroup MemoryMax (RSS-based), NOT `ulimit -v`. That matters:
# `ulimit -v` caps VIRTUAL address space, so it both (a) trips on harmless
# large reservations and (b) kills the process before RSS grows -- which
# produced a false "low memory use" reading that led to a wrong diagnosis
# and, worse, to disabling the guard entirely right before a 44.9GB OOM
# that killed the terminal. MemoryMax kills on ACTUAL memory, in the
# process's own cgroup, so a runaway can never take the terminal with it.
# The guard DEGRADES rather than failing. `systemd-run --user` needs a
# user session bus, which a CI runner and most containers do not have --
# and every harness in `bootstrap/` goes through this wrapper, so a hard
# dependency here means none of them run anywhere but a developer's
# desktop. Where the cgroup is unavailable the `timeout` still applies;
# only the memory ceiling is lost, and a disposable runner is exactly
# where that ceiling matters least.
#
# `SH_NO_CGROUP=1` forces the fallback.
#
# --- How the ceilings are sized (measured 2026-09-03) ---
#
# The guard has two jobs, and only one of them was being done. Stopping
# a runaway before it takes the terminal down needs any ceiling below
# RAM. CATCHING A REGRESSION needs one close to what the work actually
# costs, and the harnesses had defaults of 4G and 8G against a worst
# observed case of 369 MB -- 22x, which cannot fire on anything short of
# a total runaway. A guard that never fires is not a guard; it is a
# comment with a syscall.
#
# So each harness now carries a ceiling sized to what IT does, from peak
# RSS measured per invocation across every harness:
#
#   256M  the compiler alone -- `emit-llvm`, `check`, `lsp`, `complete`.
#         Whole-compiler `emit-llvm` peaks at 57 MB and most fixtures at
#         13 MB. This is the tier that matters: before tail calls in a
#         cycle started returning early, the same work peaked at 731 MB,
#         and 256M is the only tier tight enough to notice that coming
#         back.
#   512M  anything that BUILDS a small program. clang dominates these --
#         the Plum half is a few MB of the ~160 MB observed -- so the
#         headroom is really clang's, not ours.
#     1G  building the WHOLE compiler: 369 MB, of which clang is ~310.
#         Also this file's own default, since that is the ceiling a
#         person gets when they run `./sh build bootstrap/self_host` by
#         hand.
#
# clang runs INSIDE the guarded cgroup, so its memory counts against
# these; that is why the build tiers are loose and the compiler-only
# tier can be tight. Both build tiers sit about 3x over measurement,
# which is headroom for a different clang version rather than for us.
#
# `bootstrap/cross-check` is deliberately still 4G: it drives `zig cc`,
# whose footprint has not been measured here.
set -u
real="$(dirname "$(readlink -f "$0")")/sh.real"

# BOTH guards degrade, and until 2026-09-03 only one of them did.
# `timeout` is GNU coreutils: macOS ships nothing under that name and
# Homebrew installs it as `gtimeout`, so this script did not merely lose
# its time limit there, it failed outright with "exec: timeout: not
# found" and took every harness that runs through it with it. Nothing
# noticed because every macOS CI step invoked `sh.seed` or `sh.real`
# directly; `bootstrap/debug-info-check` was the first to use this
# wrapper on a Mac, and it broke on the first run.
#
# Built up as a list so the four combinations of "have a cgroup" and
# "have a timeout" do not become four copies of the exec line. The
# `${wrap[@]+...}` guard is not decoration: macOS ships bash 3.2, where
# expanding an empty array under `set -u` is an error.
wrap=()
if [ "${SH_NO_CGROUP:-0}" != "1" ] && command -v systemd-run >/dev/null 2>&1 \
   && systemd-run --user --scope -q -- true >/dev/null 2>&1; then
    wrap+=(systemd-run --user --scope -q -p MemoryMax="${SH_MEM:-1G}" -p MemorySwapMax=0 --)
fi

if command -v timeout > /dev/null 2>&1; then
    wrap+=(timeout "${SH_TIMEOUT:-25}")
elif command -v gtimeout > /dev/null 2>&1; then
    wrap+=(gtimeout "${SH_TIMEOUT:-25}")
fi

exec ${wrap[@]+"${wrap[@]}"} "$real" "$@"
