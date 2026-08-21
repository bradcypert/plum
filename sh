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
set -u
real="$(dirname "$(readlink -f "$0")")/sh.real"

if [ "${SH_NO_CGROUP:-0}" != "1" ] && command -v systemd-run >/dev/null 2>&1 \
   && systemd-run --user --scope -q -- true >/dev/null 2>&1; then
    exec systemd-run --user --scope -q \
      -p MemoryMax="${SH_MEM:-1G}" -p MemorySwapMax=0 \
      -- timeout "${SH_TIMEOUT:-25}" "$real" "$@"
fi

exec timeout "${SH_TIMEOUT:-25}" "$real" "$@"
