-- Auto-sourced on startup by Nvim's own runtime loading, exactly like
-- `ftdetect/plum.lua` and `lsp/plum.lua` next to this file — as long
-- as this `editors/nvim/` directory is on 'runtimepath' (see this
-- directory's own README.md for how to add it), no further setup is
-- needed: this file enables the LSP config and wires up tree-sitter
-- highlighting with no action required from the user's own config.

-- Enables `lsp/plum.lua` (:help vim.lsp.enable()) — safe to call even
-- on a buffer that's already loaded by the time this file runs, since
-- `vim.lsp.enable` also starts clients for already-open matching
-- buffers, not just future ones.
vim.lsp.enable("plum")

-- tree-sitter-plum isn't in nvim-treesitter's own parser registry (a
-- local, not-yet-published grammar — see `tools/tree-sitter-plum/` at
-- the plum repo root), so it needs manual registration rather than
-- `require('nvim-treesitter').install('plum')`. Looks for the
-- compiled parser at `parser/plum.so` and queries at `queries/plum/
-- highlights.scm` — both resolved via 'runtimepath', which is exactly
-- where this directory's own `parser/` and `queries/` subdirectories
-- live once it's added (see the README). The parser `.so` itself is
-- NOT shipped in the repo (a platform-specific build artifact) —
-- build it locally per the README's `tree-sitter build` step; until
-- that's done, `vim.treesitter.language.add` below fails loudly
-- (caught here, logged once, and quietly gives up rather than
-- erroring on every single `.plum` buffer open).
vim.api.nvim_create_autocmd("FileType", {
  pattern = "plum",
  callback = function(args)
    local ok, err = pcall(vim.treesitter.language.add, "plum")
    if not ok then
      vim.notify("plum.nvim: tree-sitter parser not found — run `tree-sitter build` per editors/nvim/README.md\n" .. tostring(err), vim.log.levels.WARN)
      return
    end
    vim.treesitter.start(args.buf, "plum")
  end,
})
