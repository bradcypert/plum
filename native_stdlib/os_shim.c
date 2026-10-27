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
// Path results borrow fixed-size thread-local buffers. The Plum side
// copies immediately into a string cell (`plum_str_new`), before another
// native call on that thread can overwrite the result. Other threads
// cannot overwrite it during that copy. No per-call malloc or thread-exit
// destructor is needed: the buffers themselves have thread storage
// duration, rather than pointing at separately allocated heap memory.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <dirent.h>
#include <stdint.h>
#include <stddef.h>
#include <wchar.h>
#include <ctype.h>
#include <errno.h>
#include <time.h>

#if defined(_WIN32)
#include <windows.h>
#include <winternl.h>
#include <direct.h>
#include <io.h>
#define PLUM_MKDIR(p) _mkdir(p)
#define PLUM_RMDIR(p) _rmdir(p)
#else
#include <unistd.h>
#include <fcntl.h>
#include <signal.h>
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

#if defined(_MSC_VER)
#define PLUM_OS_TLS __declspec(thread)
#else
#define PLUM_OS_TLS _Thread_local
#endif

// temp_dir and self_exe share scratch space only within one thread.
// Each simultaneous temporary-directory call has its own template.
static PLUM_OS_TLS char plum_os_buf[PLUM_PATH_MAX];

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

// Saved input paths are byte strings, just like fopen's paths. Windows
// argv uses the active ANSI code page, and POSIX filenames need not be
// UTF-8 either. Decode the hex envelope without treating it as Unicode.
static int plum_cache_hex_digit(unsigned char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

const char *plum_cache_hash_file_hex(const char *hex) {
    size_t n = strlen(hex);
    if (n == 0 || n % 2 != 0) return "";
    char *path = malloc(n / 2 + 1);
    if (!path) return "";
    for (size_t i = 0; i < n; i += 2) {
        int a = plum_cache_hex_digit((unsigned char)hex[i]);
        int b = plum_cache_hex_digit((unsigned char)hex[i + 1]);
        // Embedded NUL must not truncate a corrupt manifest's path.
        if (a < 0 || b < 0 || (a == 0 && b == 0)) {
            free(path); return "";
        }
        path[i / 2] = (char)((a << 4) | b);
    }
    path[n / 2] = '\0';
    const char *digest = plum_cache_hash_file(path);
    free(path);
    return digest;
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

#if defined(_WIN32)
// Reserve the DIRECTORY atomically. GetTempFileName followed by deleting its
// file and making a directory releases the name between those operations;
// concurrent callers can then select the same supposedly unique name.
static void plum_win_temp_dir(const char *root, const char *prefix) {
    static volatile LONG serial=0;
    size_t len=strlen(root);
    const char *separator=len && (root[len-1]=='/' || root[len-1]=='\\') ? "" : "\\";
    for (unsigned attempt=0; attempt<128; attempt++) {
        unsigned long next=(unsigned long)(ULONG)InterlockedIncrement(&serial);
        int n=snprintf(plum_os_buf,sizeof(plum_os_buf),"%s%s%s%lu-%llu-%lu",root,separator,prefix,
                       (unsigned long)GetCurrentProcessId(),(unsigned long long)GetTickCount64(),next);
        if (n<0 || n>=(int)sizeof(plum_os_buf)) break;
        if (CreateDirectoryA(plum_os_buf,NULL)) return;
        if (GetLastError()!=ERROR_ALREADY_EXISTS) break;
    }
    plum_os_buf[0]='\0';
}
#endif

const char *plum_cache_temp(const char *root) {
#if defined(_WIN32)
    plum_win_temp_dir(root,".partial-");
#else
    if (snprintf(plum_os_buf,sizeof(plum_os_buf),"%s/.partial-XXXXXX",root) >= (int)sizeof(plum_os_buf)
        || mkdtemp(plum_os_buf) == NULL) plum_os_buf[0]='\0';
#endif
    if (plum_os_buf[0]) {
        char owner[PLUM_PATH_MAX];
        if (snprintf(owner,sizeof(owner),"%s/.owner",plum_os_buf) < (int)sizeof(owner)) {
            FILE *f=fopen(owner,"w");
            if (f) {
#if defined(_WIN32)
                fprintf(f,"%lu",(unsigned long)GetCurrentProcessId());
#else
                fprintf(f,"%ld",(long)getpid());
#endif
                fclose(f);
            }
        }
    }
    return plum_os_buf;
}

// Windows native-object adapter. Ask the OS loader which images it actually
// selects instead of approximating DLL search rules with SearchPath. Support
// the MSYS2 MINGW64 layout only; unknown layouts/modules/delay imports bypass.
#if defined(_WIN64)
#include <bcrypt.h>

#define PLUM_WIN_MODULES 256
#ifndef PLUM_WIN_IMAGE_TRACE
#define PLUM_WIN_IMAGE_TRACE(path, digest) ((void)0)
#endif

typedef struct { wchar_t *path; char digest[65]; } plum_win_image;
typedef struct {
    HMODULE dll;
    BCRYPT_ALG_HANDLE alg;
    NTSTATUS (WINAPI *open)(BCRYPT_ALG_HANDLE *,LPCWSTR,LPCWSTR,ULONG);
    NTSTATUS (WINAPI *create)(BCRYPT_ALG_HANDLE,BCRYPT_HASH_HANDLE *,PUCHAR,ULONG,PUCHAR,ULONG,ULONG);
    NTSTATUS (WINAPI *update)(BCRYPT_HASH_HANDLE,PUCHAR,ULONG,ULONG);
    NTSTATUS (WINAPI *finish)(BCRYPT_HASH_HANDLE,PUCHAR,ULONG,ULONG);
    NTSTATUS (WINAPI *destroy)(BCRYPT_HASH_HANDLE);
    NTSTATUS (WINAPI *close)(BCRYPT_ALG_HANDLE,ULONG);
} plum_win_sha;

static void plum_win_sha_close(plum_win_sha *s) {
    if (s->alg) s->close(s->alg,0);
    if (s->dll) FreeLibrary(s->dll);
    memset(s,0,sizeof(*s));
}

static void plum_win_sha_open(plum_win_sha *s) {
    memset(s,0,sizeof(*s));
    s->dll=LoadLibraryExW(L"bcrypt.dll",NULL,LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!s->dll) return;
#define PLUM_WIN_PROC(member, name) do { \
    FARPROC p=GetProcAddress(s->dll,name); \
    memcpy(&s->member,&p,sizeof(p)); \
    if (!p) { plum_win_sha_close(s); return; } \
} while (0)
    PLUM_WIN_PROC(open,"BCryptOpenAlgorithmProvider");
    PLUM_WIN_PROC(create,"BCryptCreateHash");
    PLUM_WIN_PROC(update,"BCryptHashData");
    PLUM_WIN_PROC(finish,"BCryptFinishHash");
    PLUM_WIN_PROC(destroy,"BCryptDestroyHash");
    PLUM_WIN_PROC(close,"BCryptCloseAlgorithmProvider");
#undef PLUM_WIN_PROC
    if (s->open(&s->alg,BCRYPT_SHA256_ALGORITHM,NULL,0) < 0) plum_win_sha_close(s);
}

static int plum_win_hash(HANDLE f, plum_win_sha *provider, char out[65]) {
    LARGE_INTEGER zero; zero.QuadPart=0;
    if (!SetFilePointerEx(f,zero,NULL,FILE_BEGIN)) return 0;
    BCRYPT_HASH_HANDLE hash=NULL;
    int fast=provider && provider->alg && provider->create(provider->alg,&hash,NULL,0,NULL,0,0) >= 0;
    plum_cache_sha portable; plum_cache_init(&portable);
    unsigned char buf[32768], digest[32]; DWORD n;
    int good=1, retry=0;
    for (;;) {
        if (!ReadFile(f,buf,sizeof(buf),&n,NULL)) { good=0; break; }
        if (!n) break;
        if (fast) {
            if (provider->update(hash,buf,n,0) < 0) { retry=1; break; }
        } else plum_cache_update(&portable,buf,n);
    }
    if (good && fast && !retry) {
        if (provider->finish(hash,digest,sizeof(digest),0) < 0) retry=1;
        else {
            for (unsigned i=0;i<32;i++) snprintf(out+i*2,3,"%02x",(unsigned)digest[i]);
            out[64]='\0';
        }
    } else if (good && !fast) plum_cache_final(&portable,out);
    if (hash) provider->destroy(hash);
    // Restart from byte zero if the optional provider fails partway.
    return retry ? plum_win_hash(f,NULL,out) : good;
}

static int plum_win_path(HANDLE file, wchar_t path[PLUM_PATH_MAX]) {
    DWORD n=GetFinalPathNameByHandleW(file,path,PLUM_PATH_MAX,FILE_NAME_NORMALIZED|VOLUME_NAME_DOS);
    if (!n || n >= PLUM_PATH_MAX) return 0;
    if (wcsncmp(path,L"\\\\?\\",4)==0) memmove(path,path+4,(wcslen(path+4)+1)*sizeof(wchar_t));
    return wcslen(path)>3 && path[1]==L':' && path[2]==L'\\';
}

static int plum_win_in_dir(const wchar_t *path, const wchar_t *dir) {
    size_t n=wcslen(dir);
    return wcslen(path)>n && path[n]==L'\\'
        && CompareStringOrdinal(path,(int)n,dir,(int)n,TRUE)==CSTR_EQUAL;
}

static int plum_win_no_delay(HANDLE file) {
    IMAGE_DOS_HEADER dos; IMAGE_NT_HEADERS64 nt; DWORD n; LARGE_INTEGER at;
    at.QuadPart=0;
    if (!SetFilePointerEx(file,at,NULL,FILE_BEGIN)
        || !ReadFile(file,&dos,sizeof(dos),&n,NULL) || n!=sizeof(dos)
        || dos.e_magic!=IMAGE_DOS_SIGNATURE || dos.e_lfanew<0) return 0;
    at.QuadPart=dos.e_lfanew;
    if (!SetFilePointerEx(file,at,NULL,FILE_BEGIN)
        || !ReadFile(file,&nt,sizeof(nt),&n,NULL) || n!=sizeof(nt)
        || nt.Signature!=IMAGE_NT_SIGNATURE || nt.FileHeader.Machine!=IMAGE_FILE_MACHINE_AMD64
        || nt.FileHeader.SizeOfOptionalHeader!=sizeof(nt.OptionalHeader)
        || nt.OptionalHeader.Magic!=IMAGE_NT_OPTIONAL_HDR64_MAGIC
        || nt.OptionalHeader.NumberOfRvaAndSizes<=IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT) return 0;
    return nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT].Size==0
        && nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT].VirtualAddress==0;
}

static int plum_win_image_add(plum_win_image *images, size_t *count, HANDLE file,
                             const wchar_t *bin, const wchar_t *system, plum_win_sha *sha,
                             int *llvm, char reason[160]) {
    wchar_t path[PLUM_PATH_MAX];
    if (!file) { strcpy(reason,"!loader event has no file handle"); return 0; }
    if (!plum_win_path(file,path)) { strcpy(reason,"!loader image path unavailable"); return 0; }
    int private_image=plum_win_in_dir(path,bin);
    if (!private_image && !plum_win_in_dir(path,system)) {
        strcpy(reason,"!DLL outside supported toolchain/system directories"); return 0;
    }
    if (private_image && !plum_win_no_delay(file)) {
        strcpy(reason,"!toolchain image has delay imports or unsupported PE headers"); return 0;
    }
    for (size_t i=0;i<*count;i++) if (wcscmp(images[i].path,path)==0) return 1;
    if (*count>=PLUM_WIN_MODULES) return 0;
    size_t len=wcslen(path);
    wchar_t *copy=malloc((len+1)*sizeof(wchar_t));
    if (!copy) return 0;
    memcpy(copy,path,(len+1)*sizeof(wchar_t));
    if (!plum_win_hash(file,sha,images[*count].digest)) { free(copy); return 0; }
    images[*count].path=copy; (*count)++;
    const wchar_t *name=wcsrchr(path,L'\\');
    if (private_image && name && _wcsnicmp(name+1,L"libLLVM",7)==0) *llvm=1;
    return 1;
}

static int plum_win_image_compare(const void *a, const void *b) {
    return wcscmp(((const plum_win_image *)a)->path,((const plum_win_image *)b)->path);
}
#endif

const char *plum_cache_windows_fingerprint(const char *tool) {
    static PLUM_OS_TLS char result[160];
    strcpy(result,"!unsupported Windows toolchain");
#if defined(_WIN64)
    // The process ABI currently uses ANSI paths. Until that ABI is UTF-8
    // end-to-end, do not certify ambiguous non-ASCII executable spellings.
    size_t len=strlen(tool);
    if (!len || len>PLUM_PATH_MAX-32) return result;
    for (size_t i=0;i<len;i++) if ((unsigned char)tool[i]>=128 || tool[i]=='"') return result;
    wchar_t exe[PLUM_PATH_MAX], bin[PLUM_PATH_MAX]={0}, system[PLUM_PATH_MAX];
    if (!MultiByteToWideChar(CP_ACP,0,tool,-1,exe,PLUM_PATH_MAX)) return result;
    HANDLE original=CreateFileW(exe,GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_DELETE,NULL,OPEN_EXISTING,0,NULL);
    if (original==INVALID_HANDLE_VALUE) return result;
    BY_HANDLE_FILE_INFORMATION identity;
    int valid=plum_win_path(original,bin) && GetFileInformationByHandle(original,&identity);
    wchar_t *base=wcsrchr(bin,L'\\');
    if (!valid || !base || _wcsicmp(base+1,L"clang.exe")!=0) { CloseHandle(original); return result; }
    *base=L'\0';
    size_t dirlen=wcslen(bin);
    if (dirlen<12 || _wcsicmp(bin+dirlen-12,L"\\mingw64\\bin")!=0) { CloseHandle(original); return result; }
    DWORD sn=GetSystemDirectoryW(system,PLUM_PATH_MAX);
    SYSTEM_INFO machine; GetNativeSystemInfo(&machine);
    if (!sn || sn>=PLUM_PATH_MAX || machine.wProcessorArchitecture!=PROCESSOR_ARCHITECTURE_AMD64) {
        CloseHandle(original); return result;
    }
    wchar_t command[PLUM_PATH_MAX+32];
    _snwprintf(command,sizeof(command)/sizeof(command[0]),L"\"%ls\" --version",exe);
    SECURITY_ATTRIBUTES sa={sizeof(sa),NULL,TRUE};
    HANDLE sink=CreateFileW(L"NUL",GENERIC_READ|GENERIC_WRITE,FILE_SHARE_READ|FILE_SHARE_WRITE,&sa,OPEN_EXISTING,0,NULL);
    if (sink==INVALID_HANDLE_VALUE) { CloseHandle(original); return result; }
    STARTUPINFOW startup; PROCESS_INFORMATION process;
    memset(&startup,0,sizeof(startup)); memset(&process,0,sizeof(process));
    startup.cb=sizeof(startup); startup.dwFlags=STARTF_USESTDHANDLES;
    startup.hStdInput=sink; startup.hStdOutput=sink; startup.hStdError=sink;
    BOOL launched=CreateProcessW(exe,command,NULL,NULL,TRUE,DEBUG_ONLY_THIS_PROCESS|CREATE_NO_WINDOW,NULL,NULL,&startup,&process);
    CloseHandle(sink);
    if (!launched) { CloseHandle(original); strcpy(result,"!Windows loader probe could not start"); return result; }
    plum_win_image images[PLUM_WIN_MODULES]; memset(images,0,sizeof(images));
    size_t count=0; int llvm=0, exited=0, failed=0, breakpoint=0;
    DWORD exit_code=1;
    plum_win_sha sha; plum_win_sha_open(&sha);
    ULONGLONG deadline=GetTickCount64()+10000;
    while (!exited && !failed && GetTickCount64()<deadline) {
        DEBUG_EVENT event; memset(&event,0,sizeof(event));
        if (!WaitForDebugEvent(&event,100)) {
            if (GetLastError()==ERROR_SEM_TIMEOUT) continue;
            failed=1; break;
        }
        DWORD action=DBG_CONTINUE;
        switch (event.dwDebugEventCode) {
        case CREATE_PROCESS_DEBUG_EVENT: {
            HANDLE f=event.u.CreateProcessInfo.hFile;
            BY_HANDLE_FILE_INFORMATION actual;
            if (!f || !GetFileInformationByHandle(f,&actual)
                || actual.dwVolumeSerialNumber!=identity.dwVolumeSerialNumber
                || actual.nFileIndexHigh!=identity.nFileIndexHigh || actual.nFileIndexLow!=identity.nFileIndexLow
                || !plum_win_image_add(images,&count,f,bin,system,&sha,&llvm,result)) failed=1;
            if (f) CloseHandle(f);
            break;
        }
        case LOAD_DLL_DEBUG_EVENT:
            if (!plum_win_image_add(images,&count,event.u.LoadDll.hFile,bin,system,&sha,&llvm,result)) failed=1;
            if (event.u.LoadDll.hFile) CloseHandle(event.u.LoadDll.hFile);
            break;
        case EXCEPTION_DEBUG_EVENT:
            if (event.u.Exception.ExceptionRecord.ExceptionCode==EXCEPTION_BREAKPOINT && !breakpoint) breakpoint=1;
            else { failed=1; action=DBG_EXCEPTION_NOT_HANDLED; }
            break;
        case EXIT_PROCESS_DEBUG_EVENT: exit_code=event.u.ExitProcess.dwExitCode; exited=1; break;
        case RIP_EVENT: failed=1; break;
        default: break;
        }
        if (!ContinueDebugEvent(event.dwProcessId,event.dwThreadId,action)) failed=1;
    }
    if (!exited) {
        TerminateProcess(process.hProcess,1);
        // Drain exit events so Windows releases debugger-owned thread/process
        // handles too. A failed probe never leaves a live compiler behind.
        ULONGLONG end=GetTickCount64()+2000;
        while (GetTickCount64()<end) {
            DEBUG_EVENT event;
            if (!WaitForDebugEvent(&event,100)) continue;
            if (event.dwDebugEventCode==LOAD_DLL_DEBUG_EVENT && event.u.LoadDll.hFile) CloseHandle(event.u.LoadDll.hFile);
            if (event.dwDebugEventCode==CREATE_PROCESS_DEBUG_EVENT && event.u.CreateProcessInfo.hFile) CloseHandle(event.u.CreateProcessInfo.hFile);
            int done=event.dwDebugEventCode==EXIT_PROCESS_DEBUG_EVENT;
            ContinueDebugEvent(event.dwProcessId,event.dwThreadId,DBG_CONTINUE);
            if (done) break;
        }
        DebugActiveProcessStop(process.dwProcessId);
    }
    CloseHandle(process.hThread); CloseHandle(process.hProcess); CloseHandle(original);
    plum_win_sha_close(&sha);
    if (exited && !failed && exit_code==0 && llvm && count>1) {
        qsort(images,count,sizeof(images[0]),plum_win_image_compare);
        plum_cache_sha total; plum_cache_init(&total);
        const char tag[]="plum-msys2-mingw64-loader-v1";
        plum_cache_update(&total,(const unsigned char *)tag,sizeof(tag));
        for (size_t i=0;i<count;i++) {
            PLUM_WIN_IMAGE_TRACE(images[i].path,images[i].digest);
            uint64_t bytes=wcslen(images[i].path)*sizeof(wchar_t);
            plum_cache_update(&total,(const unsigned char *)&bytes,sizeof(bytes));
            plum_cache_update(&total,(const unsigned char *)images[i].path,(size_t)bytes);
            plum_cache_update(&total,(const unsigned char *)images[i].digest,64);
        }
        plum_cache_final(&total,result);
    } else if (!failed) {
        snprintf(result,sizeof(result),"!loader probe incomplete (exit=%lu, LLVM=%d, images=%lu)",
                 (unsigned long)exit_code,llvm,(unsigned long)count);
    }
    for (size_t i=0;i<count;i++) free(images[i].path);
#else
    (void)tool;
#endif
    return result;
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

// Windows headers contain unused inline assembly and produce enormous JSON
// ASTs. Inspect the emitted C IR at the SAME target/optimization settings.
// Assembly tokens remain forbidden. Reject quoted globals too: escaped asm
// labels can otherwise inject assembler directives through emitted symbols.
// This is deliberately stricter than accepting arbitrary LLVM symbol names.
long long plum_cache_safe_c_ir(const char *ir) {
    if (!plum_cache_safe_ir(ir)) return 0;
    const char *p=ir;
    while (*p) {
        if (*p==';') { while (*p && *p!='\n') p++; }
        else if (*p=='@' && p[1]=='"') return 0;
        else if (*p=='"') {
            p++;
            while (*p && *p!='"') {
                if (*p=='\\') { p++; if (!*p) return 0; }
                p++;
            }
            if (!*p) return 0;
            p++;
        } else p++;
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
    plum_win_temp_dir(base,"plum-");
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
    // readlink does not terminate its output and a full buffer may be
    // truncated. Report failure rather than a successful path prefix.
    if (n < 0 || (size_t)n >= sizeof(plum_os_buf) - 1) {
        plum_os_buf[0] = '\0';
    } else { plum_os_buf[n] = '\0'; }
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
#if defined(_WIN32)
    // The CRT rename refuses an existing destination. Do not enable
    // MOVEFILE_COPY_ALLOWED: cross-volume copy/delete is not replacement.
    int nf = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, from, -1, NULL, 0);
    int nt = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, to, -1, NULL, 0);
    if (nf <= 0 || nt <= 0) return -1;
    wchar_t *wf = malloc((size_t)nf * sizeof(wchar_t));
    wchar_t *wt = malloc((size_t)nt * sizeof(wchar_t));
    if (wf == NULL || wt == NULL) { free(wf); free(wt); return -1; }
    int ok = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, from, -1, wf, nf) &&
             MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, to, -1, wt, nt) &&
             MoveFileExW(wf, wt, MOVEFILE_REPLACE_EXISTING);
    free(wf);
    free(wt);
    return ok ? 0 : -1;
#else
    return rename(from, to) == 0 ? 0 : -1;
#endif
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

// Removal has a deliberately separate traversal from copying: every child
// operation is relative to a held directory, never a reconstructed pathname.
// A bounded depth also bounds native stack, enumeration storage and handles.
// Exhaustion is an error, never a reason to fall back to path-based removal.
#define PLUM_REMOVE_DEPTH 128

#if !defined(_WIN32)
static int plum_remove_at(int parent, const char *name, unsigned depth) {
    struct stat st;
    if (fstatat(parent, name, &st, AT_SYMLINK_NOFOLLOW) != 0) return -1;
    if (!S_ISDIR(st.st_mode)) return unlinkat(parent, name, 0);
    if (depth >= PLUM_REMOVE_DEPTH) return -1;

    int fd = openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) return -1;
    // Pin the checked object. A replacement directory is not the directory
    // we inspected; a replacement symlink cannot pass O_NOFOLLOW at all.
    struct stat opened;
    if (fstat(fd, &opened) != 0 || opened.st_dev != st.st_dev || opened.st_ino != st.st_ino) {
        close(fd);
        return -1;
    }
    DIR *dir = fdopendir(fd);
    if (dir == NULL) { close(fd); return -1; }
    int rc = 0;
    for (;;) {
        errno = 0;
        struct dirent *entry = readdir(dir);
        if (entry == NULL) { if (errno != 0) rc = -1; break; }
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) continue;
        if (plum_remove_at(fd, entry->d_name, depth + 1) != 0) { rc = -1; break; }
    }
    // Detect replacement after walking the original. The final unlinkat
    // never follows a symlink, including a swap after this identity check.
    if (rc == 0 && (fstatat(parent, name, &st, AT_SYMLINK_NOFOLLOW) != 0 ||
                   st.st_dev != opened.st_dev || st.st_ino != opened.st_ino)) rc = -1;
    if (rc == 0 && unlinkat(parent, name, AT_REMOVEDIR) != 0) rc = -1;
    if (closedir(dir) != 0) rc = -1; // owns fd after fdopendir succeeds
    return rc;
}

static int plum_remove_tree(const char *path) {
    size_t n = strlen(path);
    if (n == 0) return -1;
    char *copy = (char *)malloc(n + 1);
    if (copy == NULL) return -1;
    memcpy(copy, path, n + 1);
    // Only trim separators. realpath/lexical dot elimination would resolve
    // the link before we can remove it. Never turn '/' into an empty path.
    while (n > 1 && copy[n - 1] == '/') copy[--n] = 0;
    char *slash = strrchr(copy, '/');
    const char *name = slash ? slash + 1 : copy;
    if (*name == 0 || strcmp(name, ".") == 0 || strcmp(name, "..") == 0) {
        free(copy);
        return -1;
    }
    // Ancestors in the caller's path resolve once to acquire this parent.
    // All traversal below it uses single-component names and held fds.
    int parent;
    if (slash == NULL) parent = open(".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    else if (slash == copy) parent = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    else {
        *slash = 0;
        parent = open(copy, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    }
    int rc = parent < 0 ? -1 : plum_remove_at(parent, name, 0);
    if (parent >= 0 && close(parent) != 0) rc = -1;
    free(copy);
    return rc;
}
#else
// NtCreateFile supplies the directory-relative open missing from CreateFile.
// Resolve it per call, without a mutable global or a new linker dependency.
typedef NTSTATUS (NTAPI *PlumRemoveOpen)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES,
    PIO_STATUS_BLOCK, PLARGE_INTEGER, ULONG, ULONG, ULONG, ULONG, PVOID, ULONG);

static HANDLE plum_remove_child(PlumRemoveOpen open_child, HANDLE parent,
                                WCHAR *name, DWORD bytes, ULONG object_flags) {
    if (bytes == 0 || bytes > 65534 || bytes % sizeof(WCHAR) != 0) return INVALID_HANDLE_VALUE;
    UNICODE_STRING text = {(USHORT)bytes, (USHORT)bytes, name};
    OBJECT_ATTRIBUTES attrs = {0};
    attrs.Length = sizeof(attrs);
    attrs.RootDirectory = parent;
    attrs.Attributes = object_flags;
    attrs.ObjectName = &text;
    IO_STATUS_BLOCK status;
    HANDLE child = INVALID_HANDLE_VALUE;
    // Denying write/delete sharing pins the entry and prevents changing an
    // opened ordinary directory into a reparse point while we enumerate it.
    NTSTATUS rc = open_child(&child, DELETE | FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        &attrs, &status, NULL, 0, FILE_SHARE_READ, FILE_OPEN, FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT, NULL, 0);
    return rc < 0 ? INVALID_HANDLE_VALUE : child;
}

static int plum_remove_handle(PlumRemoveOpen open_child, HANDLE handle, unsigned depth) {
    FILE_ATTRIBUTE_TAG_INFO attrs;
    if (!GetFileInformationByHandleEx(handle, FileAttributeTagInfo, &attrs, sizeof(attrs))) return -1;
    if ((attrs.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) &&
        !(attrs.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) {
        if (depth >= PLUM_REMOVE_DEPTH) return -1;
        // malloc is suitably aligned for FILE_ID_BOTH_DIR_INFO. One bounded
        // buffer per active directory, not one allocation per entry.
        unsigned char *buffer = (unsigned char *)malloc(4096);
        if (buffer == NULL) return -1;
        int rc = 0;
        for (;;) {
            if (!GetFileInformationByHandleEx(handle, FileIdBothDirectoryInfo, buffer, 4096)) {
                if (GetLastError() != ERROR_NO_MORE_FILES) rc = -1;
                break;
            }
            DWORD offset = 0;
            for (;;) {
                FILE_ID_BOTH_DIR_INFO *entry = (FILE_ID_BOTH_DIR_INFO *)(buffer + offset);
                DWORD bytes = entry->FileNameLength;
                if (bytes == 0 || bytes % sizeof(WCHAR) != 0 ||
                    bytes > 4096 - offset - offsetof(FILE_ID_BOTH_DIR_INFO, FileName)) { rc = -1; break; }
                WCHAR *name = entry->FileName;
                if (!((bytes == 2 && name[0] == L'.') ||
                      (bytes == 4 && name[0] == L'.' && name[1] == L'.'))) {
                    HANDLE child = plum_remove_child(open_child, handle, name, bytes, 0);
                    if (child == INVALID_HANDLE_VALUE) { rc = -1; break; }
                    if (plum_remove_handle(open_child, child, depth + 1) != 0) rc = -1;
                    if (!CloseHandle(child)) rc = -1;
                    if (rc != 0) break;
                }
                if (entry->NextEntryOffset == 0) break;
                DWORD next = entry->NextEntryOffset;
                if (next % 8 != 0 || next < offsetof(FILE_ID_BOTH_DIR_INFO, FileName) + bytes ||
                    next > 4096 - offset - sizeof(FILE_ID_BOTH_DIR_INFO)) { rc = -1; break; }
                offset += next;
            }
            if (rc != 0) break;
        }
        free(buffer);
        if (rc != 0) return -1;
    }
    FILE_DISPOSITION_INFO disposition = {TRUE};
    return SetFileInformationByHandle(handle, FileDispositionInfo, &disposition, sizeof(disposition)) ? 0 : -1;
}

static int plum_remove_tree(const char *path) {
    HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
    PlumRemoveOpen open_child = ntdll ? (PlumRemoveOpen)(void *)GetProcAddress(ntdll, "NtCreateFile") : NULL;
    if (open_child == NULL) return -1;
    int n = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, NULL, 0);
    if (n <= 1) return -1;
    WCHAR *wide = (WCHAR *)malloc((size_t)n * sizeof(WCHAR));
    if (wide == NULL) return -1;
    if (!MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, wide, n)) { free(wide); return -1; }
    --n;
    while (n > 0 && (wide[n - 1] == L'/' || wide[n - 1] == L'\\')) wide[--n] = 0;
    WCHAR *name = wide + n;
    while (name > wide && name[-1] != L'/' && name[-1] != L'\\') --name;
    if (name == wide && n > 2 && wide[1] == L':') name = wide + 2;
    if (n == 0 || wide[n - 1] == L':' || wcscmp(name, L".") == 0 || wcscmp(name, L"..") == 0) {
        free(wide);
        return -1;
    }
    // Split before acquiring the root too: a single native child name avoids
    // Win32's trailing-dot/space normalization and follows no final reparse
    // point. Only the caller-supplied ancestors resolve normally.
    DWORD bytes = (DWORD)((wide + n - name) * sizeof(WCHAR));
    WCHAR first = *name;
    *name = 0;
    HANDLE parent = CreateFileW(name == wide ? L"." : wide,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_SHARE_READ,
        NULL, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, NULL);
    *name = first;
    if (parent == INVALID_HANDLE_VALUE) { free(wide); return -1; }
    // Keep Win32 root-name lookup case-insensitive. Enumeration supplies
    // exact child names, so those opens also support case-sensitive trees.
    HANDLE handle = plum_remove_child(open_child, parent, name, bytes, OBJ_CASE_INSENSITIVE);
    int parent_closed = CloseHandle(parent);
    free(wide);
    if (handle == INVALID_HANDLE_VALUE) return -1;
    if (!parent_closed) { CloseHandle(handle); return -1; }
    int rc = plum_remove_handle(open_child, handle, 0);
    if (!CloseHandle(handle)) rc = -1;
    return rc;
}
#endif

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

// Resolve an existing directory to its filesystem path. Resolving the
// directory itself (rather than only normalising its spelling) catches
// aliases such as `.` and symlinks before a copy can write through them.
static int plum_resolve_dir(const char *path, char *out, size_t cap) {
#if defined(_WIN32)
    HANDLE h = CreateFileA(path, FILE_READ_ATTRIBUTES,
                           FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           NULL, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, NULL);
    DWORD n;
    if (h == INVALID_HANDLE_VALUE) return -1;
    n = GetFinalPathNameByHandleA(h, out, (DWORD)cap, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS);
    CloseHandle(h);
    if (n == 0 || n >= cap) return -1;
    for (char *p = out; *p; p++) {
        if (*p == '\\') *p = '/';
        *p = (char)tolower((unsigned char)*p);
    }
    return 0;
#else
    return realpath(path, out) != NULL ? 0 : -1;
#endif
}

// Directories overlap when either is the other or an ancestor of it.
// The separators at the prefix boundary avoid confusing `/a/bc` with `/a/b`.
static int plum_paths_overlap(const char *a, const char *b) {
    char resolved_a[PLUM_PATH_MAX];
    char resolved_b[PLUM_PATH_MAX];
    size_t a_len, b_len;
    if (plum_resolve_dir(a, resolved_a, sizeof(resolved_a)) != 0 ||
        plum_resolve_dir(b, resolved_b, sizeof(resolved_b)) != 0) return -1;
    a_len = strlen(resolved_a);
    b_len = strlen(resolved_b);
#if defined(_WIN32)
    int same = _stricmp(resolved_a, resolved_b) == 0;
    int a_prefix = _strnicmp(resolved_a, resolved_b, a_len) == 0 &&
                   (resolved_b[a_len] == '\0' || resolved_a[a_len - 1] == '/' || resolved_b[a_len] == '/');
    int b_prefix = _strnicmp(resolved_b, resolved_a, b_len) == 0 &&
                   (resolved_a[b_len] == '\0' || resolved_b[b_len - 1] == '/' || resolved_a[b_len] == '/');
#else
    int same = strcmp(resolved_a, resolved_b) == 0;
    int a_prefix = strncmp(resolved_a, resolved_b, a_len) == 0 &&
                   (resolved_b[a_len] == '\0' || resolved_a[a_len - 1] == '/' || resolved_b[a_len] == '/');
    int b_prefix = strncmp(resolved_b, resolved_a, b_len) == 0 &&
                   (resolved_a[b_len] == '\0' || resolved_b[b_len - 1] == '/' || resolved_a[b_len] == '/');
#endif
    return same || a_prefix || b_prefix;
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

    // Refuse self-copy and either direction of overlap before reading or
    // creating any destination entry. Otherwise recursion can be unbounded,
    // and a file copied onto itself is truncated by fopen("wb").
    if (plum_paths_overlap(src, dst) != 0) return -1;

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

// Retention is best effort and runs after compilation. Only complete hash
// entries are eviction candidates; staging is protected by its live owner.
// Cache roots must be private to this host/user (not a shared network store).
typedef struct { char *path; uint64_t bytes; time_t age; } plum_cache_entry;

static int plum_cache_stat_private(const char *path, struct stat *st) {
#if defined(_WIN32)
    DWORD attrs=GetFileAttributesA(path);
    if (attrs == INVALID_FILE_ATTRIBUTES || (attrs & FILE_ATTRIBUTE_REPARSE_POINT)) return -1;
    return stat(path,st);
#else
    return lstat(path,st);
#endif
}

static uint64_t plum_cache_tree_bytes(const char *path) {
    struct stat st;
    if (plum_cache_stat_private(path,&st) != 0) return 0;
    if (S_ISREG(st.st_mode)) return st.st_size > 0 ? (uint64_t)st.st_size : 0;
    if (!S_ISDIR(st.st_mode)) return 0;
    DIR *d=opendir(path); if (!d) return 0;
    uint64_t bytes=0; struct dirent *e; char child[PLUM_PATH_MAX];
    while ((e=readdir(d)) != NULL) {
        if (!strcmp(e->d_name,".") || !strcmp(e->d_name,"..")) continue;
        if (plum_join(child,sizeof(child),path,e->d_name) == 0) bytes+=plum_cache_tree_bytes(child);
    }
    closedir(d); return bytes;
}

static int plum_cache_owner_gone(const char *path) {
    char owner[PLUM_PATH_MAX]; long pid=0;
    if (plum_join(owner,sizeof(owner),path,".owner") != 0) return 0;
    FILE *f=fopen(owner,"r"); if (!f) return 0;
    int parsed=fscanf(f,"%ld",&pid); fclose(f);
    if (parsed != 1 || pid <= 0 || pid > 2147483647L) return 0;
#if defined(_WIN32)
    HANDLE process=OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,(DWORD)pid);
    if (!process) return GetLastError() == ERROR_INVALID_PARAMETER;
    DWORD code=STILL_ACTIVE;
    int gone=GetExitCodeProcess(process,&code) && code != STILL_ACTIVE;
    CloseHandle(process); return gone;
#else
    return kill((pid_t)pid,0) != 0 && errno == ESRCH;
#endif
}

static int plum_cache_oldest(const void *a, const void *b) {
    const plum_cache_entry *x=a, *y=b;
    if (x->age != y->age) return x->age < y->age ? -1 : 1;
    return strcmp(x->path,y->path);
}

long long plum_cache_prune(const char *root, long long limit) {
    if (limit < 0) return 0;
    const char *kinds[]={"ir","obj","shims"};
    plum_cache_entry *entries=NULL; size_t count=0, capacity=0;
    uint64_t total=0; long long removed=0; time_t now=time(NULL);
    char parent[PLUM_PATH_MAX], path[PLUM_PATH_MAX]; struct stat st;
    for (size_t k=0;k<3;k++) {
        if (plum_join(parent,sizeof(parent),root,kinds[k]) != 0
            || plum_cache_stat_private(parent,&st) != 0 || !S_ISDIR(st.st_mode)) continue;
        DIR *d=opendir(parent); if (!d) continue;
        struct dirent *e;
        while ((e=readdir(d)) != NULL) {
            if (plum_join(path,sizeof(path),parent,e->d_name) != 0
                || plum_cache_stat_private(path,&st) != 0 || !S_ISDIR(st.st_mode)) continue;
            // Keep recognizing legacy Windows pcb*.tmp staging entries too.
            if (!strncmp(e->d_name,".partial-",9) || !strncmp(e->d_name,"pcb",3)) {
                if (now > st.st_mtime && now-st.st_mtime > 86400 && plum_cache_owner_gone(path)) {
                    if (plum_remove_tree(path) == 0) removed++;
                }
                continue;
            }
            if (strlen(e->d_name) != 64) continue;
            int hex=1;
            for (size_t i=0;i<64;i++) {
                if (!((e->d_name[i]>='0' && e->d_name[i]<='9')
                    || (e->d_name[i]>='a' && e->d_name[i]<='f'))) { hex=0; break; }
            }
            if (!hex) continue;
            if (count == capacity) {
                size_t next=capacity ? capacity*2 : 32;
                plum_cache_entry *grown=realloc(entries,next*sizeof(*entries));
                if (!grown) break;
                entries=grown; capacity=next;
            }
            char *saved=strdup(path); if (!saved) break;
            uint64_t bytes=plum_cache_tree_bytes(path);
            entries[count++]=(plum_cache_entry){saved,bytes,st.st_mtime}; total+=bytes;
        }
        closedir(d);
    }
    if (count > 1) qsort(entries,count,sizeof(*entries),plum_cache_oldest);
    for (size_t i=0;i<count;i++) {
        if (total > (uint64_t)limit && plum_remove_tree(entries[i].path) == 0) {
            total=total >= entries[i].bytes ? total-entries[i].bytes : 0; removed++;
        }
        free(entries[i].path);
    }
    free(entries); return removed;
}

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
static PLUM_OS_TLS char plum_cwd_buf[PLUM_PATH_MAX];

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
