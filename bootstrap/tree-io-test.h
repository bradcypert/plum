// Shared native entry/stream/hash tests, used on POSIX and Windows.
static void tree_io_tests(void) {
    long long root = plum_tree_root_open("src"); assert(root);
    long long file_entry = plum_tree_entry_open(root, "nested/data", 0); assert(file_entry);
    assert(plum_tree_entry_kind(file_entry) == 1);
    assert(*plum_tree_entry_identity(file_entry));
    assert(!strcmp(plum_tree_file_hash(file_entry, "nested/data", 1),
        "3bf1183476811055e3cf3938f3ad661407b7ef96bf74144b235d9c52c8b05f41"));
    assert(!strcmp(plum_tree_file_hash(file_entry, "nested/data", 2),
        "fb30593ca32df57a8a0f19d551710dc880a98b208b6e180b07e6aff59102d148"));
    plum_tree_entry_close(file_entry);
    // Repeated independent enumerations must start at the beginning.
    for (int run = 0; run < 3; run++) {
        long long entry = plum_tree_entry_open(root, ".", 0); assert(entry);
        long long dir = plum_tree_dir_open(entry); assert(dir);
        int found = 0;
        for (;;) {
            long long kind = plum_tree_dir_next(dir); assert(kind >= 0);
            if (!kind) break;
            if (!strcmp(plum_tree_dir_name(dir), "nested")) { assert(kind == 2); found++; }
        }
        assert(found == 1);
        plum_tree_dir_close(dir);
    }
    FILE *big = fopen("src/nested/big", "wb"); assert(big);
    for (int i = 0; i < 131073; i++) assert(fputc(i & 255, big) != EOF);
    assert(fclose(big) == 0);
    file_entry = plum_tree_entry_open(root, "nested/big", 0); assert(file_entry);
    assert(!strcmp(plum_tree_file_hash(file_entry, "nested/big", 2),
        "59143c73fbc669c18bdeb36c7cfa13888c03a2d935b18d939c628692360c16ff"));
    plum_tree_entry_close(file_entry);
    file_entry = plum_tree_entry_open(root, "nested/big", 0); assert(file_entry);
    long long reader = plum_tree_reader_open(file_entry); assert(reader);
    size_t total = 0;
    for (;;) {
        long long n = plum_tree_reader_next(reader); assert(n >= 0 && n <= 65536);
        if (!n) break;
        const unsigned char *data = (const unsigned char *)plum_tree_reader_data(reader);
        for (long long i = 0; i < n; i++) assert(data[i] == ((total + (size_t)i) & 255));
        total += (size_t)n;
    }
    assert(total == 131073); plum_tree_reader_close(reader);
    file_entry = plum_tree_entry_open(root, "nested/big", 0); assert(file_entry);
    long long text = plum_tree_text_open(file_entry); assert(text);
    assert(plum_tree_text_size(text) == 131073);
    const unsigned char *bytes = (const unsigned char *)plum_tree_text_data(text);
    for (size_t i = 0; i < 131073; i++) assert(bytes[i] == (i & 255));
    plum_tree_text_close(text);
    assert(!plum_tree_text_open(plum_tree_entry_open(root, "nested", 0)));
    assert(!plum_tree_text_open(0));
    long long seen = plum_tree_seen_open(); assert(seen);
    for (int i = 0; i < 10000; i++) {
        char key[32], path[64]; snprintf(key, sizeof(key), "id:%d", i);
        snprintf(path, sizeof(path), "logical/path/%d", i);
        assert(!plum_tree_seen_add(seen, key, path));
    }
    for (int i = 9999; i >= 0; i--) {
        char key[32], path[64]; snprintf(key, sizeof(key), "id:%d", i);
        snprintf(path, sizeof(path), "logical/path/%d", i);
        assert(plum_tree_seen_add(seen, key, "alias") == 1);
        assert(!strcmp(plum_tree_seen_previous(seen), path));
    }
    assert(plum_tree_seen_add(seen, "", "invalid") == -1);
    plum_tree_seen_close(seen);
    long long hash = plum_tree_hash_open(); assert(hash);
    const char binary[] = {0, 1, 2};
    assert(!plum_tree_hash_update(hash, binary, 1));
    assert(!plum_tree_hash_update(hash, binary + 1, 2));
    assert(!strcmp(plum_tree_hash_finish(hash),
        "ae4b3280e56e2faf83f414a6e3dabe9d5fbe18976544c05fed121accb85b53fc"));
    assert(plum_tree_hash_update(hash, binary, 1) == -1);
    plum_tree_hash_close(hash);
    FILE *writer = fopen("src/nested/writer", "wb"); assert(writer);
    assert(fputs("before", writer) >= 0 && !fclose(writer));
#if defined(_WIN32)
    assert(CreateHardLinkA("outside/writer-hardlink", "src/nested/writer", NULL));
    int linked = CreateSymbolicLinkA("src/writer-link", "nested/writer", 2) != 0;
#else
    assert(link("src/nested/writer", "outside/writer-hardlink") == 0);
    assert(symlink("nested/writer", "src/writer-link") == 0);
    int linked = 1;
    assert(chmod("src/nested/writer", 0640) == 0);
#endif
    file_entry = plum_tree_entry_open(root, "nested/writer", 0); assert(file_entry);
    char original[80]; strcpy(original, plum_tree_entry_identity(file_entry));
    plum_tree_entry_close(file_entry);
    assert(plum_tree_write(root, "nested/writer", 0, "wrong", "after", 5) == -1);
    contents("src/nested/writer", "before");
    if (linked) assert(plum_tree_write(root, "writer-link", 1, original, "after", 5) == -1);
    assert(!plum_tree_write(root, linked ? "writer-link" : "nested/writer", 0, original, "after", 5));
    contents("src/nested/writer", "after");
    contents("outside/writer-hardlink", "before");
    assert(plum_tree_write(root, "nested/writer", 0, original, "stale", 5) == -1);
#if !defined(_WIN32)
    struct stat written; assert(!stat("src/nested/writer", &written));
    assert((written.st_mode & 0777) == 0640);
#endif
    if (linked) {
        contents("src/writer-link", "after");
#if defined(_WIN32)
        assert(GetFileAttributesA("src/writer-link") & FILE_ATTRIBUTE_REPARSE_POINT);
#else
        assert(!lstat("src/writer-link", &written) && S_ISLNK(written.st_mode));
#endif
        assert(!remove("src/writer-link"));
    }
    assert(!remove("src/nested/writer"));
    assert(!remove("outside/writer-hardlink"));
    plum_tree_entry_close(root);
    assert(remove("src/nested/big") == 0);
}
