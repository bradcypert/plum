# Percent encoding without temporary integer arrays

`Encoding.percent_encode` previously built an `Array[Int]`, converted it to
Bytes, validated the ASCII result as UTF-8, and unpacked a Result. Byte reads
also went through the public optional-index API. The output was correct, but
conversion spent much more memory on intermediate objects than on the result.

## Alternatives compared

A bounded experiment compared the existing integer-array path, an owned String
accumulator with a static 256-entry encoded-byte table and direct byte reads,
an exact-size two-pass fill, and that fill with an identity-sharing branch.
All passed an oracle over every byte value, empty input, unreserved ASCII and
mixed Unicode/NUL content in debug/release. The fill prototypes substituted only
the measured wrapper's LLVM and lacked production size/failure handling. The
production measurements below use ordinary unmodified compiler builds.

For 262,144 mixed input bytes (`a`, `é`, NUL, `🙂`, repeated 32,768 times):

| Approach | Requested traffic | Result request | Process peak requested bytes |
| --- | ---: | ---: | ---: |
| Integer array | 19,848,761 | 720,913 | 9,899,217 |
| Owned String accumulator | 2,978,328 | 995,283 | 1,924,311 |
| Exact fill, either sharing policy | 720,913 | 720,913 | 986,434 |

For 327,680 unreserved ASCII bytes (`azAZ09-._~`, repeated 32,768 times):

| Approach | Requested traffic | Additional live bytes at observation |
| --- | ---: | ---: |
| Integer array | 14,605,929 | 327,697 |
| Owned String accumulator | 1,319,538 | 442,323 |
| Always allocate exact output | 327,697 | 327,697 |
| Share unchanged input | 0 | 0 |

Zero additional live bytes in the last row means the result shares the input
already live in the before snapshot. The original 327,697-byte input request
remains alive, owned by both input and result. It does not mean zero retained
storage. The two modes reported identical requested-memory figures.

A maximum-size `3 * n` reservation avoids the planning pass but can retain
three times the necessary payload space for mostly unreserved input. An owned
accumulator preserves ordinary Perceus concat reuse but grows/copies repeatedly
and retains spare capacity. Checked planning plus identity sharing gives the
smallest traffic here without adding a writable buffer API.

## Production contract and Perceus

One byte classifier implements the unchanged unreserved alphabet from
[RFC 3986 §2.3](https://www.rfc-editor.org/rfc/rfc3986#section-2.3): ASCII letters,
digits, hyphen, period, underscore and tilde. Other bytes produce uppercase
`%XX`. Space stays `%20` and plus stays `%2B`; this is not form encoding.

The planner validates the input byte length, then adds one or three output
bytes with checked arithmetic. It rejects signed/header overflow before output
allocation or writes. Repetition's scaled-length check validates the input;
concatenation's addition check is factored into a helper accepting diagnostic
context, preserving existing concatenation errors. Percent encoding uses its
own allocation-free size diagnostic.

The constructor **borrows** Bytes and returns a +1 String:

- Empty output uses the shared static empty cell.
- Unchanged unreserved ASCII retains and retags the original cell, following
  the existing sharing behavior of `Bytes.as_string` after validation.
- Escaped output requests exactly `output_length + 17` bytes through the
  existing allocator and ByteStorage accounting, then fills ASCII bytes and NUL.

Neither branch mutates input. Identity sharing keeps the original allocation,
including reserve capacity, alive until the result is released. This deliberate
tradeoff avoids a second buffer; it is not compaction. The native regression
keeps a 4,113-byte request alive through a 66-byte identity result after releasing
the original input, and verifies final release. Escaped results do not retain
input. Public String/Bytes/CStr aliases remain immutable under later appends.

Perceus ARC and compiler-selected reuse stay intact. A future consuming expansion
path would need compiler-proven ownership transfer and capacity checks; a unique
reference count alone does not authorize mutation of a borrowed parameter. No
new ownership policy or cell representation is introduced here.

## One Encoding source

The compiler now embeds `bootstrap/std/Encoding.plum` directly, matching the
existing Path/Url/Time pattern. This replaces its manually duplicated source
string, so the private runtime bridge and public implementation cannot drift
between documentation/source and the module actually compiled for users.
Other encoders and all decoders keep their existing algorithms.

## Larger memory story and verification

The principal benefit is in **compiled programs using percent encoding**, also
including URL component encoding and the compiler's documentation/LSP URI paths.
Escaped output retains the same exact-size result as before. Identity output
shares its existing owner; empty output has no new heap allocation. Requested
traffic, retained storage, process peak and RSS are separate measurements.

The bootstrap seed is refreshed and rebuilt through the actual seed-to-source
path. Compiler traffic measurements below cover the combined encoder and source
embedding change on identical inputs, separately from program measurements.

[Issue #211](https://github.com/bradcypert/plum/issues/211) was closed as a
false-positive stack report after generated musttail calls were reviewed. This
change targets intermediate allocations and makes no stack-exhaustion claim.
Related encoding and LSP URI issues #25 and #284 are already completed.

```sh
python3 bootstrap/measure-percent-encode.py /path/to/before /path/to/after \
  --out /tmp/percent-encoding-measurements.json
python3 bootstrap/string-growth-check.py /path/to/after --sanitize
bootstrap/property-check /path/to/after
PLUM=/path/to/after bootstrap/alloc-check
```

An allocation-free native observer checks every output byte and NUL. Traffic/live
deltas exclude setup and input; process peak includes them. Input stays live
across observation, and the result's CStr keeps its owner alive. The storage
guard extracts actual emitted LLVM with O0/O2, tracked/untracked and optional
ASan/LSan. It checks all 256 byte values, exact escaped requests, identity retain
and final release, spare-capacity ownership, empty results, length/header/sum
overflow and injected OOM. The public fixture contains an independent all-byte
answer generated by Python's `quote_from_bytes(..., safe='-._~')`. Properties
check concatenation, output lengths, identity and decoding, using public APIs.

Encoding's two pre-existing base64 alphabet arrays still cost 130 startup
allocations / 3,360 requested bytes. Allocation fixtures account for them
explicitly: 1,000 identity calls stay at that startup cost; 1,000 mixed calls
add exactly 1,000 allocations / 39,000 requested bytes.

## Production measurements

[Program measurements](percent-encoding-measurements.json) confirm the prototype
figures using unmodified compilers in debug and release. Escaped results have
one exact allocation; unreserved and empty inputs have zero new allocations.
The `additional_live_bytes` field records a snapshot difference, with identity
sharing explained explicitly in the measurement metadata.

[Compiler measurements](percent-encoding-compiler-measurements.json) use the same
final sources, one warmup and two recorded samples per mode. The combined encoder
and single-source module change reduces cumulative emission traffic by roughly
0.47–0.64 MB in these samples:

| Compiler emission | Before requested bytes | After requested bytes |
| --- | ---: | ---: |
| Default/debug | 1,073,382,207–1,073,493,363 | 1,072,915,462 |
| Release | 907,097,044–907,097,045 | 906,459,610–906,530,294 |

The emitted output is stable within each compiler/mode; requested-byte totals
varied slightly between repeated runs and are reported as observed ranges. These
compiler figures are separate from the much larger per-program encoding savings.
RSS measurements are recorded in the artifact; debug ranges overlap and these
few samples do not establish an RSS improvement. No timing improvement is claimed.
