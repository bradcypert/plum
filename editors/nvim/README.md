# Plum support for Neovim

A minimal, self-contained Neovim runtime bundle for Plum — LSP
diagnostics (`plum lsp`) and tree-sitter syntax highlighting. Laid out
as a normal Neovim runtime directory (`ftdetect/`, `lsp/`, `plugin/`,
`queries/`), so adding it to `'runtimepath'` is all that's needed; it
isn't published as a standalone plugin anywhere.

## Setup

**1. `plum` on your `$PATH`** (the LSP config runs `plum lsp`):

```sh
cargo install --path crates/plumc   # from the plum repo root
```

**2. Build the tree-sitter parser** — a platform-specific binary
artifact, not checked into the repo, so this is a one-time local step
(and needs re-running after any grammar change):

```sh
cd tools/tree-sitter-plum
npm install
npx tree-sitter build -o ../../editors/nvim/parser/plum.so
```

**3. Add this directory to Neovim's runtimepath.**

If you use [lazy.nvim](https://github.com/folke/lazy.nvim) (e.g.
LazyVim), add a local plugin spec pointing `dir` at this directory —
e.g. in `~/.config/nvim/lua/plugins/plum.lua`:

```lua
return {
  {
    "plum.nvim",
    dir = "/absolute/path/to/plum/editors/nvim",
    lazy = false,
  },
}
```

Otherwise, add it to `'runtimepath'` directly, e.g. in `init.lua`:

```lua
vim.opt.runtimepath:append("/absolute/path/to/plum/editors/nvim")
```

Either way, `plugin/plum.lua` runs automatically once the directory is
on `'runtimepath'` — no further config needed. Open a `.plum` file and
check `:checkhealth vim.lsp` / `:Inspect` (cursor on a token) to
confirm.

## Layout

- `ftdetect/plum.lua` — filetype detection for `.plum`.
- `lsp/plum.lua` — native (:help lsp-quickstart) LSP config for `plum
  lsp`.
- `plugin/plum.lua` — auto-run: `vim.lsp.enable("plum")` plus a
  `FileType` autocmd that registers and starts the tree-sitter parser.
- `queries/plum/highlights.scm` — symlinked from `tools/tree-sitter-
  plum/queries/highlights.scm`, the single source of truth (see that
  file for capture-name conventions).
- `parser/` — where the compiled `plum.so` goes (step 2 above);
  git-ignored, not part of the repo.

## Known limitation

`plum lsp` reports diagnostics one at a time (the first error found),
not every error in a project at once — this matches how the rest of
the Plum compiler reports errors today (see the top-level README's
"Editor support" section). Fix-and-recheck is fast in practice; this
isn't Neovim/tree-sitter-specific.
