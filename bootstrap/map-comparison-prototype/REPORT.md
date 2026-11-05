# Map #281: comparison after the Perceus reuse fixes

Base: merged main `044c0b2` (#444). Perceus is held constant. No compiler,
runtime, public Map, or hashing contract is changed by this experiment.

## Recommendation for discussion

Advance a **bitmap trie** as the general-purpose Map candidate. Bitmap16 is
my starting choice for this update-scaling issue: it combines lower allocation
traffic and faster mixed/deletion workloads than bitmap32. Bitmap32 remains a
credible choice for lookup-heavy applications; its lookup workload is faster
and its final live footprint is slightly smaller. Patricia is the alternative
for strongly unique, allocation-sensitive construction, but lookups and final
storage cost more. Red-black offers the lowest final tree storage here but its
mixed/deletion workload is substantially slower.

Do **not** replace Map with these templates as written. The separate singleton
collision arrays roughly double live storage versus the current Map at 10,000
integer keys. Before a production implementation, discuss an inline singleton
leaf with a separate collision-node case, and whether the workload justifies
16- or 32-way branching. Packed CHAMP is unmeasured; do not extrapolate these
HAMT measurements into a claim about CHAMP. This is a representation decision,
not a proposal to replace Perceus or add manual ownership rules.

## Normal integer workload, 10,000 keys, local construction

Requested bytes are exact counters; times are three-sample local medians from
a separate run with no other validation jobs launched alongside it. Lookup is
100,000 hits plus 100,000 misses with assertions. Mixed updates retain the full
original map and perform delete/reinsert for each key. Delete workload timings
also include validation; these are workload comparisons, not primitive latency.

| Candidate | Build bytes | Live after build | Build ms | Lookup ms | Mixed ms | Delete ms |
|---|---:|---:|---:|---:|---:|---:|
| baseline | 1,850,177,560 | 650,768 | 549.28 | 14.54 | 1235.06 | 606.72 |
| patricia | 1,519,952 | 1,519,968 | 13.15 | 63.96 | 17.69 | 14.90 |
| bitmap16 | 6,618,056 | 1,299,824 | 14.69 | 31.36 | 17.26 | 11.80 |
| bitmap32 | 8,300,552 | 1,213,304 | 15.61 | 23.84 | 19.64 | 13.88 |
| rbtree | 2,086,448 | 1,120,016 | 30.88 | 93.93 | 123.94 | 88.19 |

The 1,000 → 10,000 construction comparison distinguishes scaling from a
single-size speedup. Current Map grows from 25,068,560 to 1,850,177,560 requested
bytes (~74× for 10× keys); Patricia from 151,952 to 1,519,952 (~10×); bitmap16
from 508,200 to 6,618,056 (~13×). Rehash costs are included in construction.

## Ownership and reclamation, 1,000 integer keys

| Candidate | Local build bytes | Ref build bytes | History build bytes | History peak bytes | Empty after snapshots drop |
|---|---:|---:|---:|---:|---:|
| baseline | 25,068,560 | 25,084,560 | 25,108,944 | 8,259,000 | 29,192 |
| patricia | 151,952 | 527,744 | 552,128 | 692,432 | 0 |
| bitmap16 | 508,200 | 548,120 | 572,504 | 655,944 | 0 |
| bitmap32 | 646,456 | 686,376 | 710,760 | 744,632 | 0 |
| rbtree | 209,216 | 718,872 | 743,256 | 747,136 | 0 |

All tracked cases return to **zero** live wrapped bytes after the entire scope
exits. Flat Map retains 250,632 bytes while empty after the 10,000-key case;
that capacity is reclaimed when the map drops. Trees collapse to Empty. Genuine
snapshots require storage until released; Perceus cannot remove that obligation.
Ref.get introduces sharing during updates, so local reuse results must not be
substituted for Ref performance. These are requested payload bytes, not RSS.
No new compiler-memory claim is made: this PR changes no compiler sources.

## Forced complete-hash collisions

Four routing hashes hold 1,000 distinct keys. These are correctness/adversarial
workloads, not a model of normal hashing. All candidates remain linear within
collision buckets; the representation change does not promise constant-time
operations under arbitrary hash collisions. Int/String cross-pairs exercise
owned keys and values. Raw rows also cover 1, 8, and 32 keys, ordinary String
keys/values, misses, replacement, repeated deletion, and retained versions.

## Evidence and limits

- 520 size/mode/configuration rows passed semantic and lifetime assertions;
  260 timing-only rows record three-sample medians.
- 60 AddressSanitizer cases passed with leak detection enabled.
- Four additional ASan red-black checks cover 8,000 mixed operations against an
  array model, plus draining each tree, checking ordering, black heights, red
  adjacency, and left leaning after every mutation.
- The measurement shim compiles with `-Wall -Wextra -Werror`.
- Raw results and compiler/platform/source provenance are in `evidence/`.
  Allocation-run timing columns are deliberately not used in the table above.
- Array-backed collision buckets add singleton allocation overhead. Tiny-map
  specialization, packed leaves, iteration, equality, aggregate keys, application
  traces, cross-platform timing and hash-flood resistance are not settled here.
- The current public contract already excludes NaN keys, including aggregates;
  #288 remains a separate policy decision. Storage must preserve Eq/Hash laws.

#281 remains open. The next production design must preserve public Map/Set
semantics and explicitly budget steady-state storage as well as allocation
traffic; this draft experiment is evidence for that discussion.
