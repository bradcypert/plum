# THROWAWAY: Map representation comparison (#281)

This branch is an experiment, not a replacement standard library. Keep its PR
as a design review; do not merge the prototype into production. Perceus remains
the ownership strategy for every candidate.

Run from the repository root with a compiler built from the current source:

```sh
SH_TIMEOUT=600 ./sh build bootstrap/self_host -o /tmp/plum-map-comparison-compiler
python3 bootstrap/map-comparison-prototype/run.py --compiler /tmp/plum-map-comparison-compiler
python3 bootstrap/map-comparison-prototype/run.py --compiler /tmp/plum-map-comparison-compiler --quick --sanitize --out /tmp/plum-map-comparison-asan
```

Requires Linux, Clang, Python 3, and GNU-compatible linker wrapping. Generated
projects and executables live under `/tmp`. `--quick` measures 1,000 keys once;
the full run uses 1, 8, 32, 1,000 and 10,000 keys and three timing samples.
History and forced-collision cases stop at 1,000 keys to bound quadratic work.

## Contract and candidates

Every candidate exposes persistent `put`, `get -> Option`, and `remove`.
Workloads cover Int/String keys and values, hits and misses, replacement,
delete/reinsert, repeated deletion, retained versions, and Ref-held updates.
Normal flat-map cases call the unchanged public stdlib. Forced collisions use
an extracted copy of its algorithm with hashing reduced to four distinct hashes;
the trees use the identical reduction. This is labelled in each result row.
It tests distinct keys with identical complete routing hashes, not just shared
bucket indices. This modified baseline is not a claim about stdlib's normal
hash distribution.

Patricia uses compressed binary branches. Bitmap variants are compact HAMTs
at widths 16/32, not packed CHAMP. Red-black uses left-leaning balancing with
delete fixup; it is not the paper's specialized implementation or the previous
insertion-only Okasaki prototype. All tree candidates store an array of entries
per full hash. This simple collision representation adds a separate array even
for singleton hashes; measured costs include it. Optimizing singleton layout
remains a separate design choice, not an assumed free improvement.

The checked-in public Map contract excludes NaN, including nested NaN keys.
This experiment does not change that policy or claim to close #288. Hashing and
key equality remain distinct from the storage representation.

## Measurement boundaries

`results.json` contains separate uninstrumented timing and allocation-tracked
rows. Never compare timings from tracked rows. Build timing includes key/value
construction; lookup timing includes assertions, hits, misses and key/value
construction. Mixed and deletion timings are workload timings including
validation, not isolated primitive latency. Ref mode uses Ref for all mutations.
Every mixed-update workload retains a full snapshot; mode 2 additionally retains
every construction version. These distinctions must remain visible in reports.

Cumulative counters measure requested allocation traffic. The native tracker
wraps malloc/free, preserving allocation addresses and capacity behavior; its
registry and libc-internal allocations are excluded. It does not track realloc
or calloc. Peak/live bytes describe tracked workload payload, **not RSS**.

The checkpoints distinguish live storage after construction, peak storage,
storage after deletion while snapshots remain live, storage of the empty map
after snapshots drop, and storage after the entire scope exits. Every tracked
run asserts zero live bytes at that final checkpoint. Flat Map may retain bucket
capacity while empty; that is different from a leak. Assertions check snapshots
after mutations and deletion, and repeated removal of absent keys.

## Decision

See the measured report alongside this file. Production migration requires a
separate design decision and PR; #281 remains open throughout the experiment.

For timing runs, run this alone after validation finishes (three samples per
configuration, no allocation tracker):

```sh
python3 bootstrap/map-comparison-prototype/run.py --compiler /tmp/plum-map-comparison-compiler --timing-only --out /tmp/plum-map-comparison-timing
```

After the sanitizer matrix, validate red-black ordering, black heights, red-node
adjacency and left leaning against a separate array model over 2,000 mixed
operations per key/value/hash configuration:

```sh
python3 bootstrap/map-comparison-prototype/validate-rbtree.py /tmp/plum-map-comparison-compiler /tmp/plum-map-comparison-asan
```

Metadata records the compiler digest, base revision, platform and source digests.
Timings are local measurements, not portable performance guarantees. Build
counters start after empty-map setup; live and peak counters include that setup.
