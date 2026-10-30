#if defined(_WIN32)
#include "copy-tree-windows-test.c"
#else
// POSIX regression coverage for descriptor-relative copying.
#include <assert.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int fail_enumeration;
static int swap_source;
static int test_openat(int parent, const char *name, int flags, ...) {
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list args;
        va_start(args, flags);
        mode = (mode_t)va_arg(args, int);
        va_end(args);
    }
    if (swap_source && !strcmp(name, "victim")) {
        swap_source = 0;
        assert(renameat(parent, name, parent, "parked") == 0);
        assert(symlinkat("../outside", parent, name) == 0);
    }
    return openat(parent, name, flags, mode);
}
static struct dirent *test_readdir(DIR *dir) {
    if (fail_enumeration) {
        fail_enumeration = 0;
        errno = EIO;
        return NULL;
    }
    return readdir(dir);
}
#define readdir test_readdir
#define openat test_openat
#include "../native_stdlib/os_shim.c"
#undef readdir
#undef openat

static void directory(const char *path) { assert(mkdir(path, 0700) == 0); }
static void file(const char *path, const char *text) {
    FILE *f = fopen(path, "wb");
    assert(f);
    assert(fwrite(text, 1, strlen(text), f) == strlen(text));
    assert(fclose(f) == 0);
}
static void contents(const char *path, const char *expected) {
    char buf[128] = {0};
    FILE *f = fopen(path, "rb");
    assert(f);
    size_t n = fread(buf, 1, sizeof(buf), f);
    assert(n == strlen(expected));
    assert(memcmp(buf, expected, n) == 0);
    assert(fclose(f) == 0);
}
static void link_text(const char *path, const char *expected) {
    char buf[4096];
    ssize_t n = readlink(path, buf, sizeof(buf));
    assert(n == (ssize_t)strlen(expected));
    assert(memcmp(buf, expected, (size_t)n) == 0);
}
static int descriptor_count(void) {
    int count = 0;
    // Count the range used by these fixtures without opening another handle.
    for (int fd = 0; fd < 1024; fd++)
        if (fcntl(fd, F_GETFD) >= 0) count++;
    return count;
}

int main(void) {
    char root[] = "/tmp/plum-copy-test-XXXXXX";
    assert(mkdtemp(root));
    int cwd = open(".", O_RDONLY | O_DIRECTORY);
    assert(cwd >= 0 && chdir(root) == 0);
    directory("src"); directory("dst"); directory("outside");
    file("outside/keep", "untouched");
    directory("src/nested"); file("src/nested/data", "copied");
    assert(symlink("..", "src/cycle") == 0);
    assert(symlink("absent", "src/dangling") == 0);
    assert(symlink("../outside", "src/external") == 0);
    assert(symlink("nested/data", "src/file-link") == 0);
    assert(os_copy_tree("src/", "dst/") == 0);
    contents("dst/nested/data", "copied");
    link_text("dst/cycle", ".."); link_text("dst/dangling", "absent");
    link_text("dst/external", "../outside"); link_text("dst/file-link", "nested/data");
    contents("outside/keep", "untouched");
    long long held = plum_tree_root_open("src");
    assert(held > 0);
    long long resolved = plum_tree_entry_open(held, "file-link", 0);
    assert(resolved > 0);
    char bytes[6];
    assert(read((int)(resolved - 1), bytes, sizeof(bytes)) == sizeof(bytes));
    assert(memcmp(bytes, "copied", sizeof(bytes)) == 0);
    plum_tree_entry_close(resolved);
    assert(plum_tree_entry_open(held, "file-link", 1) == 0);
    assert(plum_tree_entry_open(held, "external/keep", 0) == 0);
    assert(plum_tree_entry_open(held, "../outside/keep", 0) == 0);
    assert(plum_tree_entry_open(held, "dangling", 0) == 0);
    assert(plum_tree_entry_open(held, "nested/data/", 0) == 0);
    assert(symlink("loop", "src/loop") == 0);
    assert(plum_tree_entry_open(held, "loop", 0) == 0);
    assert(symlink("/etc/passwd", "src/absolute") == 0);
    assert(plum_tree_entry_open(held, "absolute", 0) == 0);
    assert(symlink("../nested/data", "src/nested/internal") == 0);
    resolved = plum_tree_entry_open(held, "nested/internal", 0);
    assert(resolved > 0);
    plum_tree_entry_close(resolved);
    int resolver_baseline = descriptor_count();
    for (int i = 0; i < 100; i++) {
        assert(plum_tree_entry_open(held, "loop", 0) == 0);
        assert(plum_tree_entry_open(held, "external/keep", 0) == 0);
        assert(descriptor_count() == resolver_baseline);
    }
    plum_tree_entry_close(held);
    assert(os_copy_tree("src", "src") == -1);
    assert(os_copy_tree("src", "src/nested") == -1);
    assert(os_copy_tree("src/nested", "src") == -1);
    assert(symlink("src", "root-link") == 0);
    assert(os_copy_tree("root-link/", "dst") == -1);
    assert(os_copy_tree("src", "root-link/") == -1);

    directory("one"); directory("output"); file("one/keep", "replacement");
    assert(symlink("../outside/keep", "output/keep") == 0);
    assert(os_copy_tree("one", "output") == -1);
    contents("outside/keep", "untouched");
    assert(unlink("output/keep") == 0);
    assert(link("outside/keep", "output/keep") == 0);
    assert(os_copy_tree("one", "output") == 0);
    contents("output/keep", "replacement"); contents("outside/keep", "untouched");

    directory("dirs"); directory("dirs/child"); directory("dir-output");
    file("dirs/child/new", "new");
    assert(symlink("../outside", "dir-output/child") == 0);
    assert(os_copy_tree("dirs", "dir-output") == -1);
    assert(access("outside/new", F_OK) == -1);

    directory("race"); directory("race/victim"); directory("race-output");
    swap_source = 1;
    assert(os_copy_tree("race", "race-output") == -1);
    assert(swap_source == 0);
    assert(access("race-output/victim/keep", F_OK) == -1);

    directory("special"); assert(mkfifo("special/pipe", 0600) == 0);
    assert(os_copy_tree("special", "output") == -1);
    int before = descriptor_count();
    for (int i = 0; i < 100; i++) {
        fail_enumeration = 1;
        assert(os_copy_tree("one", "output") == -1);
        assert(descriptor_count() == before);
    }
    contents("output/keep", "replacement");

    directory("deep"); directory("deep-output");
    int fd = open("deep", O_RDONLY | O_DIRECTORY);
    assert(fd >= 0);
    for (int i = 0; i < PLUM_COPY_DEPTH; i++) {
        assert(mkdirat(fd, "d", 0700) == 0);
        int next = openat(fd, "d", O_RDONLY | O_DIRECTORY);
        assert(next >= 0); close(fd); fd = next;
    }
    close(fd);
    assert(os_copy_tree("deep", "deep-output") == -1);
    assert(descriptor_count() == before);
    // Remove the extra deepest level before using the bounded removal API.
    char path[1024] = "deep";
    for (int i = 0; i < PLUM_COPY_DEPTH; i++) strcat(path, "/d");
    assert(rmdir(path) == 0);
    assert(os_remove_tree("deep") == 0);
    assert(os_remove_tree("deep-output") == 0);
    assert(fchdir(cwd) == 0); close(cwd);
    assert(os_remove_tree(root) == 0);
    puts("copy-tree POSIX tests: ok");
    return 0;
}

#endif
