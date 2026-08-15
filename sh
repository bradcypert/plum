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
set -u
exec systemd-run --user --scope -q \
  -p MemoryMax="${SH_MEM:-1G}" -p MemorySwapMax=0 \
  -- timeout "${SH_TIMEOUT:-25}" "$(dirname "$(readlink -f "$0")")/sh.real" "$@"
