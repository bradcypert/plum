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
`bootstrap/compiler-metadata.json`, a `cg_std_source` arm, and a file here.
Run `bootstrap/gen-compiler-metadata` after editing the module list. The
generated compiler declarations and bootstrap tools share that list;
`parser.std_module_names()` exposes it to the rest of the compiler.
