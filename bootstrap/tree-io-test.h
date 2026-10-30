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
    long long hash = plum_tree_hash_open(); assert(hash);
    const char binary[] = {0, 1, 2};
    assert(!plum_tree_hash_update(hash, binary, 1));
    assert(!plum_tree_hash_update(hash, binary + 1, 2));
    assert(!strcmp(plum_tree_hash_finish(hash),
        "ae4b3280e56e2faf83f414a6e3dabe9d5fbe18976544c05fed121accb85b53fc"));
    assert(plum_tree_hash_update(hash, binary, 1) == -1);
    plum_tree_hash_close(hash);
    plum_tree_entry_close(root);
    assert(remove("src/nested/big") == 0);
}
