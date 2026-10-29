// Model bytes received from POSIX argv or a Windows ANSI API without
// asking the source lexer to accept an invalid UTF-8 string literal.
const char *native_path_bytes(void) { return "dir-\xfd/leaf-\xfc.ext-\xfe"; }
const char *native_path_leaf(void) { return "leaf-\xfc.ext-\xfe"; }
