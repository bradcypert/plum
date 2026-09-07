Plum is a small, statically typed, compiled language.

`Duration` and a monotonic clock, `select` arms that stop waiting, and
JSON decoding that says which field disagreed — plus a memory-corruption
fix in the compiler that has no workaround short of upgrading.

## A closure inside a match arm could corrupt the heap

Fixed. This is the reason to take this release even if none of the
features below matter to you.

A closure captures its whole enclosing environment, so a closure cell
can hold values its body never mentions. The compiler's last-use pass
did not know that: inside a closure body it still had the enclosing
function's locals in scope, and when one of them was not read again it
offered that value's cell as somewhere to build a new one. But a
captured value belongs to the closure, not to the frame running its
body — so the body released a reference it never held, and the closure's
own cleanup released the same value again later.

The shape that hits it is ordinary:

```plum
let found = entries[0].value;
Result.map(inner_run(found, p), |x| Some(x))
```

`|x| Some(x)` never names `found`, and that is exactly why the pass
thought `found` was free. Symptoms were a `malloc(): unaligned tcache
chunk` abort or a wrong value, on the SECOND call, some distance from
the code at fault. Values kept alive only by a static string literal
never showed it, because releasing an immortal value twice is free.

Programs with no closures compile to identical output.

`bootstrap/exec_corpus/closure_capture_reuse` pins it. Worth saying what
missed it: 149 corpus fixtures under AddressSanitizer with leak
detection, and the bootstrap fixed point — the compiler simply does not
write that shape anywhere in its own source. It was found by writing the
JSON module below.

## Decoding JSON

`json_parse` gives back a faithful `JsonValue`. Getting a real type out
of one meant matching `JsonObject`, scanning an `Array[JsonEntry]` by
hand, and matching `JsonString` — once per field, and with the field's
name gone by the time anything could report a failure.

```plum
use Json;

struct Repo { name: String, stars: Int, owner: Owner, license: Option[String] }

let repo (): Json.Decoder[Repo] =
    Json.map4(
        Json.field("name",    Json.string()),
        Json.field("stars",   Json.int()),
        Json.field("owner",   owner()),
        Json.field("license", Json.nullable(Json.string())),
        |n, s, o, l| Repo { name: n, stars: s, owner: o, license: l })

Json.decode_string(repo(), text)    // Result[Repo, String]
```

Decoders compose, and the composition is what carries the path:

```
expected String at data.repos[1].name
no field `admin` at owner
expected a whole Number at stars
```

Nobody threaded that string. `at(["data", "repos"], index(1, field("name",
string())))` is nested `field`s, and each one extends the path the
decoder inside it reports against.

Present, absent and null are three different states and get three
different answers. `field` requires the key; `nullable` permits its
value to be null; `optional_field` accepts either. `int()` refuses
`41.5` rather than truncating it.

The rest: `string bool int float value null_as`, `list`, `map` through
`map6`, `succeed`, `fail`, `and_then`, `one_of`, `decode`.

This is a library, not syntax — a generic struct with a closure field,
compiled by the same compiler as everything else. A decoder that misreads
a document is a COMPILE error at the line that misreads it, which is the
thing a path-string API cannot do.

## `Duration`, sleep, and a monotonic clock

Time had one function, `Time.now()`, in whole seconds. Anything wanting
to wait, or to measure how long something took, had nothing to use.

```plum
use Time;

Time.sleep(Time.millis(250))

let t0 = Time.instant();
run_it();
Time.since(t0).as_millis()
```

`Duration` is a type rather than a number, so `Time.sleep(500)` does not
compile and cannot mean milliseconds on one line and seconds on the
next. Built with `nanos micros millis seconds minutes hours zero`, read
with `as_nanos as_micros as_millis as_seconds`, combined with `add sub
scale negate`, compared with `lt le gt ge min max compare`. Durations
can be negative — `between` a later and an earlier instant is the
negation of the other order, which is more useful than a saturating zero.

`Time.instant()` reads a MONOTONIC clock, which never moves backwards
and is unaffected by the system clock being set. It is the one to
measure with. `Time.now()` and `Time.now_millis()` remain the wall
clock, which is the one to timestamp with. The origin of an `Instant` is
deliberately meaningless: two of them are only ever subtracted.

`==` works on both because it is structural; ordered comparison is
`Duration.lt` and friends, since `<` is defined on `Int`, `Float` and
`String` and on nothing else.

## `select` stops waiting forever

`select` could multiplex channels but could only block, so "take one if
something is ready" and "wait 200ms" both needed a hand-rolled loop.

```plum
select {
    n = rx => handle(n),
    else => "nothing ready",
}

select {
    n = rx => handle(n),
    Time.millis(200) => "timed out",
}
```

Both compile to the same call: `else` is a timeout of zero. No new
keyword — a timeout arm is an expression of type `Duration` where a
channel would be, and `else` was already a keyword.

## Also

`plum test` now runs tests in submodules, not only in a project's root
module, and reports them by qualified name — `shapes.area_is_positive`
rather than `area_is_positive`. A project whose tests sat beside the
code they cover was silently running none of them.
