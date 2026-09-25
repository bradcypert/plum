# Embedded standard library source

Ordinary Plum, edited as Plum. The compiler bakes each file in with
`@embed_file` while *it* is being compiled, so an installed `plum`
still needs nothing beside the binary.

This directory is **outside** `bootstrap/self_host/` so
`collect_project` does not treat these files as compiler modules.

| File | Loaded from |
|---|---|
| `prelude.plum` | `codegen/prelude.plum` (`cg_prelude_src`) |
| `<Name>.plum` | `codegen/stdlib.plum` (`cg_std_source`) |

A new `use`-gated module still needs its name in
`parser.std_module_names()`, a `cg_std_source` arm, and a file here.
