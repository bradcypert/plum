-- `ftdetect/` files are sourced by Nvim's own filetype system very
-- early at startup (before any plugin manager gets a chance to run) —
-- this guarantees `.plum` files opened directly from the command line
-- (`nvim foo.plum`) get the right filetype before anything else looks
-- at the buffer.
vim.filetype.add({ extension = { plum = "plum" } })
