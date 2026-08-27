Plum is a small, statically typed, compiled language.

A breaking release, and the one that finishes a job 0.0.8 started. The
standard library now has modules, and the flat prelude holds only what
genuinely has to be there.

## Four modules

```plum
use Os;
use Time;

let stamp (): String = Time.rfc7231(Time.now())
let conf (): Result[String, String] = Os.read_file("app.conf")
```

| module | what is in it |
|---|---|
| `Os` | files, directories, environment, subprocesses, platform, exit |
| `Time` | the clock, and the calendar on top of it |
| `Net` | TCP sockets |
| `Http` | HTTP client and server, built on `Net` |

The rule, unchanged from 0.0.8: `Array`, `String`, `Option`, `Result`,
`Int`, `Float` stay in the prelude because `T.f(x)` **is** the
method-call mechanism — `xs.map(f)` only works because `Array.map` is
in scope. A namespace that names no type and dispatches to nothing is a
module.

Still in the prelude, needing no `use`: `println`/`print`, the `assert`
family, `Json`, and every type namespace.

### What moved where

| was | is now |
|---|---|
| `read_file` `write_file` `list_dir` `is_directory` | `Os.read_file` and friends |
| `env_var` `run_process` `exit_with` | `Os.env_var` `Os.run_process` `Os.exit_with` |
| `tcp_listen_on` `tcp_connect_to` `tcp_accept_connection` | `Net.listen_on` `Net.connect_to` `Net.accept` |
| `tcp_read` `tcp_write` `tcp_close_connection` | `Net.read` `Net.write` `Net.close` |
| `http_get` `http_post` `http_request` `http_serve` | `Http.get` `Http.post` `Http.request` `Http.serve` |
| `HttpResponse` `HttpRequest` `HttpHeader` | `Http.Response` `Http.Request` `Http.Header` |

The names lost their prefixes because the module supplies one:
`Net.connect_to` rather than `Net.tcp_connect_to`, `Http.Response`
rather than `Http.HttpResponse`.

`read_file` went into `Os` rather than a new `Fs`, because `Os` already
owned `make_dir`, `remove_tree` and `copy_tree` — a separate `Fs` would
have split the filesystem across two modules. This is the shape Go
settled on: `os.ReadFile`, `os.Getenv`, `os.Exit`, `os.MkdirAll` in one
place, `net` separate, string helpers on the type.

## Modules can depend on modules

`Http` is ordinary Plum over `Net`'s sockets — no IR, no backend, no
extern surface of its own — so it is a separate module rather than more
of `Net`. That needed a mechanism: `use Http;` pulls `Net` in with it.
You do not have to know what a module is built on to use it.

## Four `String` functions, two of which were redundant

- `string_le(a, b)` **removed.** It was `<=` spelled out. The operator
  agrees with it on every case tried, including prefixes, empty
  strings and non-ASCII.
- `chars_join(cs)` **removed.** It duplicated `Array.concat_all`, and
  was the slower of the two — a fold of `.concat()` against a runtime
  primitive. The compiler had ten call sites paying for that.
- `string_reverse` → **`String.reverse`**
- `string_is_ascii_ws` → **`String.is_ascii_ws`** (which also retired a
  byte-identical private copy sitting eight lines away)

## Upgrading

Add the `use` line the error asks for:

```
unbound variant/function: Os -- `Os` is a standard library module;
add `use Os;` to this file
```

For the renamed functions the error names the old spelling as unbound,
and the table above says what to write instead.

The bootstrap seed was regenerated, which is a large but purely
generated diff — `bootstrap/check-seed` requires it after a prelude
rename, and says so itself when it fails.
