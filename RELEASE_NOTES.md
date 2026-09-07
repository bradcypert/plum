Plum is a small, statically typed, compiled language.

A one-line fix to the language server on macOS and Windows. If you are
on Linux, 0.0.20 and 0.0.21 behave identically.

## Cross-file diagnostics went to the wrong file

On macOS and Windows, an error in one file of a project was attributed
to a temporary file instead of the file it is in — so your editor
underlined nothing, and the diagnostic pointed somewhere you have never
opened. Errors in the file being edited were unaffected.

The language server checks an unsaved buffer by copying the project to a
scratch directory, so diagnostics come back carrying scratch paths and
are mapped home by matching that prefix. In 0.0.20 the two sides of that
comparison stopped being built the same way: one had been normalized and
the other had not.

It only showed up where the temporary directory needed normalizing.
macOS sets `TMPDIR` with a trailing slash, which produces a doubled
separator; Linux usually leaves it unset and produces `/tmp`, with
nothing to collapse. That is why every Linux run passed while both other
platforms failed.

`bootstrap/lsp-smoke` now sets `TMPDIR` that way itself, on every
platform, so the awkward shape is exercised where it can be iterated on
rather than only where it happens to occur.

## Upgrading

Nothing else changed. If you are on 0.0.20 and use the language server
on macOS or Windows, this is worth taking; otherwise it can wait.
