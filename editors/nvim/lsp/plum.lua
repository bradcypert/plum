-- Nvim's native LSP config format (:help lsp-quickstart) — a table
-- returned from `lsp/<name>.lua` on the runtimepath is equivalent to
-- calling `vim.lsp.config['<name>'] = { ... }` directly, and is picked
-- up by both the built-in client and nvim-lspconfig without needing a
-- plugin-specific server definition, since `plum` isn't in
-- nvim-lspconfig's own registry.
--
-- Requires `plum` on your `$PATH` — `cargo install --path
-- crates/plumc` from the plum repo root, or a symlink to
-- `target/debug/plum` / `target/release/plum` somewhere on PATH.
return {
  cmd = { "plum", "lsp" },
  filetypes = { "plum" },

  -- A Plum project has no manifest file (`plum run <dir>` treats ANY
  -- directory of `.plum` files as a project — see DESIGN.md's "a
  -- directory IS a module") — so there's no single canonical marker
  -- file like `Cargo.toml`/`package.json` to search for. `.git` is a
  -- reasonable first guess (matches the whole repo most Plum projects
  -- will live in); when that's not found (a standalone `.plum` file,
  -- or a repo Nvim wasn't opened inside), fall back to the file's own
  -- directory — `plumc`'s project walker treats that exactly the same
  -- way `plum run` would if you ran it from that directory.
  root_dir = function(bufnr, on_dir)
    local fname = vim.api.nvim_buf_get_name(bufnr)
    on_dir(vim.fs.root(fname, { ".git" }) or vim.fs.dirname(fname))
  end,
}
