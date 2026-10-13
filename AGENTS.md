# CI lessons

Before pushing a change that adds a property test:

- Keep the test within the public API available to `bootstrap/properties`;
  compiler-private helpers such as `parse_float_raw` are not in scope.
- Check the edge-case construction and expected output against Plum itself.
  For example, unary `-0.0` constructs negative zero, and its formatted
  positive result is `"0"`.
- Run `bootstrap/property-check` with a compiler built from the current
  source, so the property test uses the same prelude and runtime as CI.

When a source doc comment changes, regenerate `docs/stdlib` with a compiler
built from the current source. The checked-in `./sh` may be stale and can
produce old docs. A reliable sequence is:

```sh
./sh build bootstrap/self_host -o /tmp/plum-check-compiler
PLUM=/tmp/plum-check-compiler ./bootstrap/check-docs --record
PLUM=/tmp/plum-check-compiler ./bootstrap/check-docs
```

Run the focused property and documentation checks before pushing; the Linux
CI job reports these failures separately from the later aggregate gate.
