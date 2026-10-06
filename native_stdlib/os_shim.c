// Filesystem and self-location primitives, so the compiler stops
// shelling out to Unix commands to do them.
//
// `bootstrap/self_host/main.plum` used to reach these through
// `run_process`: `mktemp -d` (5 sites), `rm -rf`/`rm -f` (6), `cp -r`
// (1), `mkdir` (1), and `/proc/self/exe` (3). That works on Linux,
// works on macOS by luck, and cannot work on Windows, where none of
// those programs exist. `/proc/self/exe` is worse than the others: it
// is Linux-only, so the language server -- which re-invokes itself
// through it -- was already broken on a Mac.
//
// Replacing them is also just better on the platforms where they DID
// work. A `plum build` forked three processes to make a directory,
// delete a file, and delete a directory; now it forks one, for `clang`,
// which is the only one that was ever doing real work.
//
// Same shim conventions as its neighbours (see `net_shim.c`'s header
// for the general pattern): every function takes and returns only
// `long long` (Plum `Int`) or `const char *` (Plum `CStr`), because
// Plum's extern surface has no raw pointers, no out-parameters and no
// multi-value return.
//
// **Returned strings live in a static buffer**, not in `malloc`'d
// memory. The Plum side copies immediately into a string cell
// (`plum_str_new`), so the buffer's contents are needed only until the
// call returns -- and a `malloc` here would be a leak, since the
// extern boundary gives the Plum side no way to free it. Not
// re-entrant, and not required to be: nothing calls these from more
// than one thread.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <dirent.h>
#include <stdint.h>
#include <ctype.h>

#if defined(_WIN32)
#include <windows.h>
#include <direct.h>
#include <io.h>
#define PLUM_MKDIR(p) _mkdir(p)
#define PLUM_RMDIR(p) _rmdir(p)
#else
#include <unistd.h>
#define PLUM_MKDIR(p) mkdir((p), 0700)
#define PLUM_RMDIR(p) rmdir(p)
#endif

#if defined(__APPLE__)
#include <mach-o/dyld.h>
#endif

#if defined(__linux__) && defined(__has_include)
#if __has_include(<linux/if_alg.h>)
#define PLUM_CACHE_AF_ALG 1
#include <sys/socket.h>
#include <linux/if_alg.h>
#endif
#endif

#define PLUM_PATH_MAX 4096

static char plum_os_buf[PLUM_PATH_MAX];

// Compiler artifact support. Hash files in bounded memory: materializing the
// compiler executable as an Array[Int] just to fingerprint it would dominate
// a warm run. SHA-256 uses unsigned 32-bit arithmetic (FIPS 180-4).
typedef struct {
    uint32_t h[8];
    uint64_t bytes;
    unsigned char block[64];
    size_t used;
} plum_cache_sha;

static uint32_t plum_cache_rotr(uint32_t x, unsigned n) {
    return (x >> n) | (x << (32 - n));
}

static void plum_cache_block(plum_cache_sha *s, const unsigned char *p) {
    static const uint32_t k[64] = {
        0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
        0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
        0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
        0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
        0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
        0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
        0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
        0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2
    };
    uint32_t w[64], a,b,c,d,e,f,g,h;
    for (unsigned i = 0; i < 16; i++)
        w[i] = ((uint32_t)p[i*4] << 24) | ((uint32_t)p[i*4+1] << 16)
             | ((uint32_t)p[i*4+2] << 8) | p[i*4+3];
    for (unsigned i = 16; i < 64; i++) {
        uint32_t x = w[i-15], y = w[i-2];
        w[i] = w[i-16] + (plum_cache_rotr(x,7)^plum_cache_rotr(x,18)^(x>>3))
             + w[i-7] + (plum_cache_rotr(y,17)^plum_cache_rotr(y,19)^(y>>10));
    }
    a=s->h[0]; b=s->h[1]; c=s->h[2]; d=s->h[3];
    e=s->h[4]; f=s->h[5]; g=s->h[6]; h=s->h[7];
    for (unsigned i = 0; i < 64; i++) {
        uint32_t t1 = h + (plum_cache_rotr(e,6)^plum_cache_rotr(e,11)^plum_cache_rotr(e,25))
                    + ((e&f)^((~e)&g)) + k[i] + w[i];
        uint32_t t2 = (plum_cache_rotr(a,2)^plum_cache_rotr(a,13)^plum_cache_rotr(a,22))
                    + ((a&b)^(a&c)^(b&c));
        h=g; g=f; f=e; e=d+t1; d=c; c=b; b=a; a=t1+t2;
    }
    s->h[0]+=a; s->h[1]+=b; s->h[2]+=c; s->h[3]+=d;
    s->h[4]+=e; s->h[5]+=f; s->h[6]+=g; s->h[7]+=h;
}

static void plum_cache_init(plum_cache_sha *s) {
    static const uint32_t h[8] = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,
                                  0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
    memcpy(s->h,h,sizeof(h)); s->bytes=0; s->used=0;
}

static void plum_cache_update(plum_cache_sha *s, const unsigned char *p, size_t n) {
    s->bytes += n;
    while (n) {
        size_t take = 64 - s->used;
        if (take > n) take = n;
        memcpy(s->block+s->used,p,take); s->used+=take; p+=take; n-=take;
        if (s->used == 64) { plum_cache_block(s,s->block); s->used=0; }
    }
}

static void plum_cache_final(plum_cache_sha *s, char out[65]) {
    uint64_t bits = s->bytes * 8;
    unsigned char pad[128] = {0x80};
    size_t n = s->used < 56 ? 56-s->used : 120-s->used;
    for (unsigned i = 0; i < 8; i++) pad[n+i]=(unsigned char)(bits >> ((7-i)*8));
    plum_cache_update(s,pad,n+8);
    for (unsigned i = 0; i < 8; i++) snprintf(out+i*8,9,"%08x",(unsigned)s->h[i]);
    out[64]='\0';
}

const char *plum_cache_hash_text(const char *p, long long n) {
    static char out[65];
    if (n < 0) { out[0]='\0'; return out; }
    plum_cache_sha s; plum_cache_init(&s);
    plum_cache_update(&s,(const unsigned char *)p,(size_t)n);
    plum_cache_final(&s,out); return out;
}

// Linux's optional kernel SHA provider uses CPU acceleration when available.
// No new library/tool dependency; an unavailable provider falls back to the
// portable implementation. Cache correctness is independent of the provider.
static int plum_cache_kernel_hash(const char *path, char out[65]) {
#if defined(PLUM_CACHE_AF_ALG)
    struct sockaddr_alg sa;
    memset(&sa,0,sizeof(sa)); sa.salg_family=AF_ALG;
    memcpy(sa.salg_type,"hash",5); memcpy(sa.salg_name,"sha256",7);
    int parent=socket(AF_ALG,SOCK_SEQPACKET,0);
    if (parent < 0) return 0;
    if (bind(parent,(struct sockaddr *)&sa,sizeof(sa)) != 0) { close(parent); return 0; }
    int fd=accept(parent,NULL,NULL); close(parent);
    if (fd < 0) return 0;
    FILE *f=fopen(path,"rb");
    if (!f) { close(fd); return 0; }
    unsigned char buf[65536], digest[32]; size_t n; int bad=0;
    while ((n=fread(buf,1,sizeof(buf),f)) != 0) {
        size_t at=0;
        while (at < n) {
            ssize_t sent=send(fd,buf+at,n-at,MSG_MORE);
            if (sent <= 0) { bad=1; break; }
            at+=(size_t)sent;
        }
        if (bad) break;
    }
    if (ferror(f)) bad=1;
    if (fclose(f) != 0) bad=1;
    if (!bad && send(fd,NULL,0,0) != 0) bad=1;
    if (!bad && recv(fd,digest,sizeof(digest),0) != (ssize_t)sizeof(digest)) bad=1;
    close(fd);
    if (bad) return 0;
    for (unsigned i=0;i<32;i++) snprintf(out+i*2,3,"%02x",(unsigned)digest[i]);
    out[64]='\0'; return 1;
#else
    (void)path; (void)out; return 0;
#endif
}

const char *plum_cache_hash_file(const char *path) {
    static char out[65];
    unsigned char buf[32768]; size_t n;
    out[0]='\0';
    struct stat st;
    if (stat(path,&st) == 0 && S_ISREG(st.st_mode) && st.st_size > 1048576
        && plum_cache_kernel_hash(path,out)) return out;
    FILE *f=fopen(path,"rb");
    if (!f) return out;
    plum_cache_sha s; plum_cache_init(&s);
    while ((n=fread(buf,1,sizeof(buf),f)) != 0) plum_cache_update(&s,buf,n);
    int bad=ferror(f); if (fclose(f) != 0) bad=1;
    if (!bad) plum_cache_final(&s,out);
    return out;
}

// Verify the bytes actually copied, not a prior read of the source path.
// A concurrent cleaner can remove an entry at any point; that is a miss.
long long plum_cache_copy(const char *src, const char *dst, const char *digest) {
    FILE *in=fopen(src,"rb"); if (!in) return -1;
    FILE *out=fopen(dst,"wb"); if (!out) { fclose(in); return -1; }
    unsigned char buf[32768]; size_t n; int bad=0; char got[65];
    plum_cache_sha s; plum_cache_init(&s);
    while ((n=fread(buf,1,sizeof(buf),in)) != 0) {
        plum_cache_update(&s,buf,n);
        if (fwrite(buf,1,n,out) != n) { bad=1; break; }
    }
    if (ferror(in)) bad=1;
    if (fclose(in) != 0) bad=1;
    if (fclose(out) != 0) bad=1;
    plum_cache_final(&s,got);
    if (digest[0] && strcmp(got,digest) != 0) bad=1;
    if (bad) remove(dst);
    return bad ? -1 : 0;
}

const char *plum_cache_temp(const char *root) {
#if defined(_WIN32)
    if (GetTempFileNameA(root,"pcb",0,plum_os_buf) == 0) { plum_os_buf[0]='\0'; return plum_os_buf; }
    DeleteFileA(plum_os_buf);
    if (PLUM_MKDIR(plum_os_buf) != 0) plum_os_buf[0]='\0';
#else
    if (snprintf(plum_os_buf,sizeof(plum_os_buf),"%s/.partial-XXXXXX",root) >= (int)sizeof(plum_os_buf)
        || mkdtemp(plum_os_buf) == NULL) plum_os_buf[0]='\0';
#endif
    return plum_os_buf;
}

const char *plum_cache_tool(const char *name) {
    plum_os_buf[0]='\0';
#if defined(_WIN32)
    DWORD n=SearchPathA(NULL,name,".exe",sizeof(plum_os_buf),plum_os_buf,NULL);
    if (n == 0 || n >= sizeof(plum_os_buf)) plum_os_buf[0]='\0';
#else
    if (strchr(name,'/')) {
        if (!realpath(name,plum_os_buf)) plum_os_buf[0]='\0';
    } else {
        const char *env=getenv("PATH"); if (!env) return plum_os_buf;
        char *paths=strdup(env); if (!paths) return plum_os_buf;
        char *p=paths;
        while (p) {
            char *next=strchr(p,':'); if (next) *next++='\0';
            char candidate[PLUM_PATH_MAX];
            int n=snprintf(candidate,sizeof(candidate),"%s/%s",*p ? p : ".",name);
            if (n >= 0 && n < (int)sizeof(candidate) && access(candidate,X_OK) == 0
                && realpath(candidate,plum_os_buf)) break;
            p=next;
        }
        free(paths);
    }
#endif
    return plum_os_buf;
}

// Inspect trusted Clang JSON AST output, not C source spellings: adjacent
// literals and escapes can conceal assembler file-reading directives.
// Ordinary asm symbol labels are safe only when their emitted names have
// no quoting/whitespace/control characters that could inject assembly.
static const char *plum_cache_json_value(const char *p) {
    while (*p && isspace((unsigned char)*p)) p++;
    if (*p++ != ':') return NULL;
    while (*p && isspace((unsigned char)*p)) p++;
    return *p == '"' ? p+1 : NULL;
}
long long plum_cache_safe_ast(const char *ast) {
    if (*ast != '{' || !strstr(ast,"\"TranslationUnitDecl\"")) return 0;
    const char *p=ast;
    while ((p=strstr(p,"\"kind\"")) != NULL) {
        p+=6; const char *value=plum_cache_json_value(p);
        if (!value) continue;
        const char *end=strchr(value,'"'); if (!end) return 0;
        size_t size=(size_t)(end-value);
        for (size_t i=0; i+3<=size; i++) {
            if (!memcmp(value+i,"Asm",3)
                && !(size == 12 && !memcmp(value,"AsmLabelAttr",12))) return 0;
        }
        p=end+1;
    }
    p=ast;
    while ((p=strstr(p,"\"mangledName\"")) != NULL) {
        p+=13; const char *value=plum_cache_json_value(p);
        if (!value) return 0;
        for (; *value && *value != '"'; value++) {
            unsigned char c=(unsigned char)*value;
            if (!((c>='a' && c<='z') || (c>='A' && c<='Z')
                || (c>='0' && c<='9') || c=='_' || c=='.' || c=='$')) return 0;
        }
        if (*value != '"') return 0;
    }
    return 1;
}

// Generated LLVM contains quoted IR strings and identifiers, and comments.
// Only a bare `asm` token can introduce assembly. Do not mistake embedded
// C source or an application's string data for an LLVM assembly operand.
static int plum_cache_ir_word(unsigned char c) {
    return (c>='a' && c<='z') || (c>='A' && c<='Z') || (c>='0' && c<='9')
        || c=='_' || c=='.' || c=='$' || c=='-' || c=='@' || c=='%' || c=='!';
}
long long plum_cache_safe_ir(const char *ir) {
    const char *p=ir;
    while (*p) {
        if (*p == ';') { while (*p && *p!='\n') p++; }
        else if (*p == '"') {
            p++;
            while (*p && *p!='"') {
                if (*p=='\\') { p++; if (!*p) return 0; }
                p++;
            }
            if (!*p) return 0;
            p++;
        } else if (plum_cache_ir_word((unsigned char)*p)) {
            const char *start=p;
            while (plum_cache_ir_word((unsigned char)*p)) p++;
            if (p-start == 3 && !memcmp(start,"asm",3)) return 0;
        } else { p++; }
    }
    return 1;
}

// Creates a fresh, empty, private directory and returns its path.
// Returns "" -- an empty, non-null CStr -- on failure, because a null
// CStr return is a hard runtime abort under Plum's FFI semantics (see
// `net_shim.c`'s `tcp_recv` note, the same trade for the same reason).
const char *os_temp_dir(void) {
#if defined(_WIN32)
    char base[PLUM_PATH_MAX];
    DWORD n = GetTempPathA((DWORD)sizeof(base), base);
    if (n == 0 || n >= sizeof(base)) { plum_os_buf[0] = '\0'; return plum_os_buf; }
    // `GetTempFileNameA` creates a FILE; the directory of the same name
    // is what is wanted, so the file is removed and a directory put in
    // its place. The name is still unique -- that is what the call
    // bought -- and the window between the two is not a security
    // boundary this compiler relies on.
    if (GetTempFileNameA(base, "plum", 0, plum_os_buf) == 0) { plum_os_buf[0] = '\0'; return plum_os_buf; }
    DeleteFileA(plum_os_buf);
    if (PLUM_MKDIR(plum_os_buf) != 0) { plum_os_buf[0] = '\0'; }
    return plum_os_buf;
#else
    const char *tmp = getenv("TMPDIR");
    if (tmp == NULL || tmp[0] == '\0') tmp = "/tmp";
    if (snprintf(plum_os_buf, sizeof(plum_os_buf), "%s/plum-XXXXXX", tmp) >= (int)sizeof(plum_os_buf)) {
        plum_os_buf[0] = '\0';
        return plum_os_buf;
    }
    if (mkdtemp(plum_os_buf) == NULL) plum_os_buf[0] = '\0';
    return plum_os_buf;
#endif
}

// The path of the running executable. Three genuinely different calls
// for one question; there is no portable spelling.
const char *os_self_exe(void) {
    plum_os_buf[0] = '\0';
#if defined(_WIN32)
    DWORD n = GetModuleFileNameA(NULL, plum_os_buf, (DWORD)sizeof(plum_os_buf));
    if (n == 0 || n >= sizeof(plum_os_buf)) plum_os_buf[0] = '\0';
#elif defined(__APPLE__)
    uint32_t n = (uint32_t)sizeof(plum_os_buf);
    if (_NSGetExecutablePath(plum_os_buf, &n) != 0) plum_os_buf[0] = '\0';
#else
    ssize_t n = readlink("/proc/self/exe", plum_os_buf, sizeof(plum_os_buf) - 1);
    if (n < 0) { plum_os_buf[0] = '\0'; } else { plum_os_buf[n] = '\0'; }
#endif
    return plum_os_buf;
}

// 0 on success, -1 on failure, matching the other shims' Int-sentinel
// convention rather than C's.
long long os_make_dir(const char *path) {
    return PLUM_MKDIR(path) == 0 ? 0 : -1;
}

long long os_remove_file(const char *path) {
    return remove(path) == 0 ? 0 : -1;
}

// `rename`, which is ATOMIC within a filesystem: the destination is
// either the old file or the new one, never a half-written mixture.
// That is the whole reason this exists -- `plum fmt --write` builds the
// formatted text beside the original and renames it into place, so an
// interrupted run cannot leave somebody's source truncated.
//
// Across filesystems `rename` fails rather than copying, and the
// caller is told so rather than being silently given a slower,
// non-atomic fallback it did not ask for.
long long os_rename_file(const char *from, const char *to) {
    return rename(from, to) == 0 ? 0 : -1;
}

static int plum_is_dir(const char *path) {
    struct stat st;
    if (stat(path, &st) != 0) return 0;
    return S_ISDIR(st.st_mode) ? 1 : 0;
}

static int plum_join(char *out, size_t cap, const char *a, const char *b) {
    int n = snprintf(out, cap, "%s/%s", a, b);
    return (n > 0 && (size_t)n < cap) ? 0 : -1;
}

// Deletes a directory and everything under it. Recurses with a
// heap-allocated path buffer per level rather than a shared one,
// because the caller's path must stay intact across the child call.
//
// Symlinks are removed, never followed: `plum_is_dir` uses `stat`,
// which follows, so a symlink to a directory would recurse into the
// TARGET and delete someone else's files. `lstat` is used here for
// exactly that reason. On Windows there is no `lstat`; the reparse
// point is deleted by `remove` as an ordinary entry.
static int plum_remove_tree(const char *path) {
    DIR *d;
    struct dirent *e;
    char *child;
    int rc = 0;

#if !defined(_WIN32)
    struct stat st;
    if (lstat(path, &st) != 0) return -1;
    if (!S_ISDIR(st.st_mode)) return remove(path) == 0 ? 0 : -1;
#else
    if (!plum_is_dir(path)) return remove(path) == 0 ? 0 : -1;
#endif

    d = opendir(path);
    if (d == NULL) return -1;
    child = (char *)malloc(PLUM_PATH_MAX);
    if (child == NULL) { closedir(d); return -1; }

    while ((e = readdir(d)) != NULL) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
        if (plum_join(child, PLUM_PATH_MAX, path, e->d_name) != 0) { rc = -1; continue; }
        if (plum_remove_tree(child) != 0) rc = -1;
    }
    free(child);
    closedir(d);
    if (PLUM_RMDIR(path) != 0) rc = -1;
    return rc;
}

long long os_remove_tree(const char *path) {
    return plum_remove_tree(path) == 0 ? 0 : -1;
}

static int plum_copy_file(const char *src, const char *dst) {
    FILE *in, *out;
    char buf[65536];
    size_t n;
    int rc = 0;

    in = fopen(src, "rb");
    if (in == NULL) return -1;
    out = fopen(dst, "wb");
    if (out == NULL) { fclose(in); return -1; }
    while ((n = fread(buf, 1, sizeof(buf), in)) > 0) {
        if (fwrite(buf, 1, n, out) != n) { rc = -1; break; }
    }
    if (ferror(in)) rc = -1;
    if (fclose(out) != 0) rc = -1;
    fclose(in);
    return rc;
}

// Copies the CONTENTS of `src` into `dst`, which must already exist --
// the semantics of `cp -r src/. dst`, which is what this replaced, and
// deliberately not `cp -r src dst` (that would nest a directory).
//
// File MODES are not preserved. The one caller copies a project into a
// scratch directory to type-check it, where nothing is executed; if a
// caller ever needs the executable bit, this is the line to revisit
// rather than a thing to assume.
static int plum_copy_tree(const char *src, const char *dst) {
    DIR *d;
    struct dirent *e;
    char *s;
    char *t;
    int rc = 0;

    d = opendir(src);
    if (d == NULL) return -1;
    s = (char *)malloc(PLUM_PATH_MAX);
    t = (char *)malloc(PLUM_PATH_MAX);
    if (s == NULL || t == NULL) { free(s); free(t); closedir(d); return -1; }

    while ((e = readdir(d)) != NULL) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
        if (plum_join(s, PLUM_PATH_MAX, src, e->d_name) != 0) { rc = -1; continue; }
        if (plum_join(t, PLUM_PATH_MAX, dst, e->d_name) != 0) { rc = -1; continue; }
        if (plum_is_dir(s)) {
            if (PLUM_MKDIR(t) != 0 && !plum_is_dir(t)) { rc = -1; continue; }
            if (plum_copy_tree(s, t) != 0) rc = -1;
        } else {
            if (plum_copy_file(s, t) != 0) rc = -1;
        }
    }
    free(s);
    free(t);
    closedir(d);
    return rc;
}

long long os_copy_tree(const char *src, const char *dst) {
    return plum_copy_tree(src, dst) == 0 ? 0 : -1;
}

// --- File metadata (issue #24) ---
//
// `stat(2)`, the rest of it. `path_is_dir` in `dir_shim.c` was the first
// caller and stays where it is; these are its siblings, here because
// this is the filesystem shim.
//
// Loaded into a thread-local record and then read field by field, so
// `Os.stat` costs ONE `stat` rather than one per field. Plum's extern
// surface has no multi-value return, which is the same constraint
// `tcp_recv_n`/`tcp_recv_data` and `file_read_n`/`file_read_data` work
// around the same way.
//
// Thread-local rather than plain static: `net_shim.c` learned what a
// shared buffer costs when two threads use it at once, and a per-thread
// record is cheap enough not to repeat that.
#include <errno.h>
#include <time.h>

#if defined(_MSC_VER)
#define PLUM_OS_TLS __declspec(thread)
#else
#define PLUM_OS_TLS _Thread_local
#endif

static PLUM_OS_TLS long long stat_size = 0;
static PLUM_OS_TLS long long stat_mtime = 0;
static PLUM_OS_TLS long long stat_is_dir = 0;

// 0 on success, -1 if the path cannot be stat'ed at all (most commonly
// because it is not there). Follows symlinks, matching `path_is_dir`.
long long os_stat_load(const char *path) {
    struct stat st;
    if (stat(path, &st) != 0) {
        stat_size = 0;
        stat_mtime = 0;
        stat_is_dir = 0;
        return -1;
    }
    stat_size = (long long)st.st_size;
    stat_mtime = (long long)st.st_mtime;
    stat_is_dir = S_ISDIR(st.st_mode) ? 1 : 0;
    return 0;
}

long long os_stat_size(void) { return stat_size; }
long long os_stat_mtime(void) { return stat_mtime; }
long long os_stat_is_dir(void) { return stat_is_dir; }

// Three-way, and the third case is the point: 1 present, 0 absent, -1
// CANNOT SAY.
//
// "Does this exist" and "can I see whether this exists" are different
// questions, and collapsing them is how a permission error becomes a
// silent "no". Only `ENOENT` and `ENOTDIR` mean absent -- the latter
// because a path under a non-directory (`a/b` where `a` is a file)
// cannot exist either. Anything else is a real failure to report.
long long os_path_exists(const char *path) {
    struct stat st;
    if (stat(path, &st) == 0) {
        return 1;
    }
    if (errno == ENOENT || errno == ENOTDIR) {
        return 0;
    }
    return -1;
}

// --- Working directory (issue #17) ---
//
// Its own buffer rather than `plum_os_buf`: that one holds the result of
// `os_temp_dir`/`os_self_exe`, and a caller holding one of those across
// a `cwd()` would silently get the wrong string back.
static char plum_cwd_buf[PLUM_PATH_MAX];

// "" on failure, never NULL -- a null `CStr` return is a hard runtime
// abort under Plum's FFI semantics, the same trade every other shim
// here makes.
const char *os_cwd(void) {
#if defined(_WIN32)
    if (_getcwd(plum_cwd_buf, (int)sizeof(plum_cwd_buf)) == NULL) plum_cwd_buf[0] = '\0';
#else
    if (getcwd(plum_cwd_buf, sizeof(plum_cwd_buf)) == NULL) plum_cwd_buf[0] = '\0';
#endif
    return plum_cwd_buf;
}

long long os_chdir(const char *path) {
#if defined(_WIN32)
    return _chdir(path) == 0 ? 0 : -1;
#else
    return chdir(path) == 0 ? 0 : -1;
#endif
}
