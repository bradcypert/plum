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
#include <winioctl.h>
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

static int plum_join(char *out, size_t cap, const char *a, const char *b) {
    int n = snprintf(out, cap, "%s/%s", a, b);
    return (n > 0 && (size_t)n < cap) ? 0 : -1;
}

// Capture failures before cleanup can change errno/GetLastError. Storage is
// thread-local and borrowed only until the next filesystem operation.
static PLUM_OS_TLS char plum_tree_error_text[512];
void plum_tree_error_clear(void) {
    plum_tree_error_text[0] = 0;
    errno = 0;
#if defined(_WIN32)
    SetLastError(0);
#endif
}
static void plum_tree_error(const char *reason) {
    snprintf(plum_tree_error_text, sizeof(plum_tree_error_text), "%s", reason);
}
static void plum_tree_os_error(const char *operation) {
#if defined(_WIN32)
    DWORD code = GetLastError();
    if (!code) { snprintf(plum_tree_error_text, sizeof(plum_tree_error_text), "%s failed (no OS error provided)", operation); return; }
    WCHAR wide[256]; char message[384] = "";
    DWORD count = FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
        NULL, code, 0, wide, 256, NULL);
    if (count) WideCharToMultiByte(CP_UTF8, 0, wide, -1, message, sizeof(message), NULL, NULL);
    size_t n = strlen(message);
    while (n && (message[n - 1] == '\r' || message[n - 1] == '\n')) message[--n] = 0;
    snprintf(plum_tree_error_text, sizeof(plum_tree_error_text), "%s: Windows error %lu%s%s",
        operation, (unsigned long)code, *message ? ": " : "", message);
#else
    int code = errno;
    if (!code) { snprintf(plum_tree_error_text, sizeof(plum_tree_error_text), "%s failed (no OS error provided)", operation); return; }
    snprintf(plum_tree_error_text, sizeof(plum_tree_error_text), "%s: errno %d (%s)", operation, code, strerror(code));
#endif
}
#if defined(_WIN32)
static void plum_tree_nt_error(const char *operation, NTSTATUS status) {
    snprintf(plum_tree_error_text, sizeof(plum_tree_error_text), "%s: NTSTATUS 0x%08lx", operation, (unsigned long)status);
}
#endif
const char *plum_tree_last_error(void) {
    if (!*plum_tree_error_text) {
#if defined(_WIN32)
        if (GetLastError()) plum_tree_os_error("filesystem operation failed");
#else
        if (errno) plum_tree_os_error("filesystem operation failed");
#endif
        else plum_tree_error("filesystem operation failed without OS error detail");
    }
    return plum_tree_error_text;
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
    if (rc < 0) plum_tree_nt_error("open entry for removal", rc);
    return rc < 0 ? INVALID_HANDLE_VALUE : child;
}

static int plum_remove_handle(PlumRemoveOpen open_child, HANDLE handle, unsigned depth) {
    FILE_ATTRIBUTE_TAG_INFO attrs;
    if (!GetFileInformationByHandleEx(handle, FileAttributeTagInfo, &attrs, sizeof(attrs))) { plum_tree_os_error("inspect entry for removal"); return -1; }
    if ((attrs.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) &&
        !(attrs.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) {
        if (depth >= PLUM_REMOVE_DEPTH) { plum_tree_error("removal depth exceeds 128"); return -1; }
        // malloc is suitably aligned for FILE_ID_BOTH_DIR_INFO. One bounded
        // buffer per active directory, not one allocation per entry.
        unsigned char *buffer = (unsigned char *)malloc(4096);
        if (buffer == NULL) { plum_tree_error("cannot allocate removal enumeration buffer"); return -1; }
        int rc = 0;
        for (;;) {
            if (!GetFileInformationByHandleEx(handle, FileIdBothDirectoryInfo, buffer, 4096)) {
                if (GetLastError() != ERROR_NO_MORE_FILES) { plum_tree_os_error("enumerate removal directory"); rc = -1; }
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
                    if (!CloseHandle(child)) { if (!*plum_tree_error_text) plum_tree_os_error("close removed child"); rc = -1; }
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
    if (SetFileInformationByHandle(handle, FileDispositionInfo, &disposition, sizeof(disposition))) return 0;
    plum_tree_os_error("mark entry for deletion"); return -1;
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
    if (parent == INVALID_HANDLE_VALUE) { plum_tree_os_error("open removal parent"); free(wide); return -1; }
    // Keep Win32 root-name lookup case-insensitive. Enumeration supplies
    // exact child names, so those opens also support case-sensitive trees.
    HANDLE handle = plum_remove_child(open_child, parent, name, bytes, OBJ_CASE_INSENSITIVE);
    int parent_closed = CloseHandle(parent);
    if (!parent_closed && !*plum_tree_error_text) plum_tree_os_error("close removal parent");
    free(wide);
    if (handle == INVALID_HANDLE_VALUE) return -1;
    if (!parent_closed) { CloseHandle(handle); return -1; }
    int rc = plum_remove_handle(open_child, handle, 0);
    if (!CloseHandle(handle)) { if (!*plum_tree_error_text) plum_tree_os_error("close removed root"); rc = -1; }
    return rc;
}
#endif

long long os_remove_tree(const char *path) {
    plum_tree_error_clear();
    int result = plum_remove_tree(path);
    if (result && !*plum_tree_error_text) plum_tree_os_error("remove tree");
    return result == 0 ? 0 : -1;
}

#define PLUM_COPY_DEPTH 128
#define PLUM_COPY_BUFFER 65536

// A dangling link may be stored and hashed, but even its missing suffix must
// not contain a lexical ascent above the held root. Future reads resolve the
// path again with the ordinary strict resolver, never trust this validation.
static int plum_tree_missing_tail(const char *path, unsigned depth) {
    const char *p = path;
    while (*p) {
        const char *end = strchr(p, '/');
        size_t n = end ? (size_t)(end - p) : strlen(p);
        if (n == 2 && p[0] == '.' && p[1] == '.') {
            if (!depth) { plum_tree_error("path escapes filesystem root"); return -1; }
            depth--;
        } else if (n && !(n == 1 && p[0] == '.')) {
            if (++depth >= PLUM_COPY_DEPTH) { plum_tree_error("directory depth exceeds 128"); return -1; }
        }
        if (!end) break;
        p = end + 1;
    }
    return 0;
}
#if defined(_WIN32)
// Windows adapters use native relative opens. Share-read-only handles pin
// directories and reject writers/reparse changes while an operation owns them.
static PlumRemoveOpen plum_tree_win_open_api(void) {
    HMODULE module = GetModuleHandleW(L"ntdll.dll");
    return module ? (PlumRemoveOpen)(void *)GetProcAddress(module, "NtCreateFile") : NULL;
}
static HANDLE plum_tree_win_child(HANDLE parent, const WCHAR *name, DWORD bytes,
                                  ACCESS_MASK access, ULONG disposition, ULONG options,
                                  NTSTATUS *failure) {
    PlumRemoveOpen api = plum_tree_win_open_api();
    if (!api || bytes > 65534 || bytes % sizeof(WCHAR)) { plum_tree_error("native relative open unavailable or invalid component length"); return INVALID_HANDLE_VALUE; }
    UNICODE_STRING text = {(USHORT)bytes, (USHORT)bytes, (WCHAR *)name};
    OBJECT_ATTRIBUTES attrs = {0};
    attrs.Length = sizeof(attrs); attrs.RootDirectory = parent; attrs.ObjectName = &text;
    IO_STATUS_BLOCK status;
    HANDLE h = INVALID_HANDLE_VALUE;
    NTSTATUS rc = api(&h, access | FILE_READ_ATTRIBUTES | SYNCHRONIZE, &attrs, &status,
        NULL, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, disposition,
        options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT, NULL, 0);
    if (failure) *failure = rc;
    if (rc < 0) plum_tree_nt_error("open path component", rc);
    return rc < 0 ? INVALID_HANDLE_VALUE : h;
}
// Enumeration and link targets use UTF-8. Existing Windows CLI arguments
// can also carry native ACP bytes (the runtime still exposes narrow argv).
// Preserve that established filename-byte contract at the adapter boundary
// when strict UTF-8 decoding fails; never use replacement-character decoding.
static int plum_tree_win_path(const char *path, WCHAR *wide, int capacity) {
    int count = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, wide, capacity);
    if (!count && GetLastError() == ERROR_NO_UNICODE_TRANSLATION)
        count = MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, path, -1, wide, capacity);
    return count;
}
static HANDLE plum_copy_root(const char *path) {
    if (!*path) { plum_tree_error("empty root path"); return INVALID_HANDLE_VALUE; }
    int n = plum_tree_win_path(path, NULL, 0);
    if (n <= 1) { plum_tree_os_error("decode root path"); return INVALID_HANDLE_VALUE; }
    WCHAR *wide = malloc((size_t)n * sizeof(WCHAR));
    if (!wide) { plum_tree_error("cannot allocate root path"); return INVALID_HANDLE_VALUE; }
    if (!plum_tree_win_path(path, wide, n)) {
        plum_tree_os_error("decode root path"); free(wide); return INVALID_HANDLE_VALUE;
    }
    // Preserve a drive root's separator, but not a trailing slash on a link.
    while (n > 2 && (wide[n - 2] == L'/' || wide[n - 2] == L'\\') &&
           !(n == 4 && wide[1] == L':')) wide[--n - 1] = 0;
    HANDLE h = CreateFileW(wide, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_SHARE_READ, NULL, OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, NULL);
    if (h == INVALID_HANDLE_VALUE) plum_tree_os_error("open root directory");
    free(wide);
    FILE_ATTRIBUTE_TAG_INFO info;
    if (h != INVALID_HANDLE_VALUE) {
        if (!GetFileInformationByHandleEx(h, FileAttributeTagInfo, &info, sizeof(info))) {
            plum_tree_os_error("inspect root directory"); CloseHandle(h); h = INVALID_HANDLE_VALUE;
        } else if (!(info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) || (info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) {
            plum_tree_error("root must be an ordinary directory, not a link or reparse point");
            CloseHandle(h); h = INVALID_HANDLE_VALUE;
        }
    }
    return h;
}
static int plum_tree_win_attrs(HANDLE h, FILE_ATTRIBUTE_TAG_INFO *info) {
    if (GetFileInformationByHandleEx(h, FileAttributeTagInfo, info, sizeof(*info))) return 0;
    plum_tree_os_error("inspect entry attributes"); return -1;
}
// Return the stored relative substitute name of an ordinary symbolic link.
// Junctions and other reparse tags never become traversal instructions.
static int plum_tree_win_link(HANDLE h, WCHAR *target, size_t capacity, int allow_absolute) {
    _Alignas(max_align_t) unsigned char data[MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    DWORD bytes;
    if (!DeviceIoControl(h, FSCTL_GET_REPARSE_POINT, NULL, 0, data, sizeof(data), &bytes, NULL)) { plum_tree_os_error("read symbolic link"); return -1; }
    struct LinkData {
        DWORD tag; WORD length, reserved;
        WORD substitute_offset, substitute_length, print_offset, print_length;
        ULONG flags;
        WCHAR path[1];
    };
    struct LinkData *link = (struct LinkData *)data;
    size_t prefix = offsetof(struct LinkData, path);
    if (bytes >= prefix && link->tag != IO_REPARSE_TAG_SYMLINK) { plum_tree_error("unsupported reparse point (only symbolic links are supported)"); return -1; }
    if (bytes >= prefix && !allow_absolute && link->flags == 0) { plum_tree_error("absolute link target refused inside filesystem root"); return -1; }
    if (bytes < prefix || link->tag != IO_REPARSE_TAG_SYMLINK || (link->flags != 1 && !(allow_absolute && link->flags == 0)) ||
        (size_t)link->length + 8 > bytes || (size_t)link->length + 8 < prefix ||
        link->substitute_offset % sizeof(WCHAR) || link->substitute_length % sizeof(WCHAR) ||
        (size_t)link->substitute_offset + link->substitute_length > (size_t)link->length + 8 - prefix) { plum_tree_error("malformed symbolic link reparse data"); return -1; }
    size_t count = link->substitute_length / sizeof(WCHAR);
    if (!count || count >= capacity) { plum_tree_error("symbolic link target is empty or exceeds resolver capacity"); return -1; }
    memcpy(target, (unsigned char *)link->path + link->substitute_offset, link->substitute_length);
    target[count] = 0;
    for (size_t i = 0; i < count; i++) {
        if (!target[i] || (!allow_absolute && target[i] == L':')) { plum_tree_error("invalid or absolute symbolic link target"); return -1; }
        if (target[i] == L'\\') target[i] = L'/';
    }
    if (!allow_absolute && target[0] == L'/') { plum_tree_error("absolute link target refused inside filesystem root"); return -1; }
    return 0;
}
typedef struct { HANDLE parent; WCHAR name[PLUM_PATH_MAX]; int missing; } plum_tree_location;
static HANDLE plum_tree_resolve_at(HANDLE root, const char *relative, int reject_links, plum_tree_location *location, int allow_missing) {
    if (location) { location->parent = INVALID_HANDLE_VALUE; location->missing = 0; }
    WCHAR pending[PLUM_PATH_MAX], target[PLUM_PATH_MAX];
    if (!*relative) { plum_tree_error("empty relative path"); return INVALID_HANDLE_VALUE; }
    int n = plum_tree_win_path(relative, pending, PLUM_PATH_MAX);
    if (n <= 1) { plum_tree_os_error("decode relative path"); return INVALID_HANDLE_VALUE; }
    for (int i = 0; i < n - 1; i++) {
        if (pending[i] == L':') { plum_tree_error("drive or stream path refused inside filesystem root"); return INVALID_HANDLE_VALUE; }
        if (pending[i] == L'\\') pending[i] = L'/';
    }
    if (pending[0] == L'/') { plum_tree_error("absolute path refused inside filesystem root"); return INVALID_HANDLE_VALUE; }
    HANDLE parents[PLUM_COPY_DEPTH], result = INVALID_HANDLE_VALUE;
    unsigned depth = 0, links = 0;
    if (!DuplicateHandle(GetCurrentProcess(), root, GetCurrentProcess(), &parents[0], 0, FALSE, DUPLICATE_SAME_ACCESS))
        { plum_tree_os_error("duplicate root handle"); return INVALID_HANDLE_VALUE; }
    for (;;) {
        WCHAR *slash = wcschr(pending, L'/');
        WCHAR *rest = slash ? slash + 1 : pending + wcslen(pending);
        int more = slash != NULL;
        if (slash) *slash = 0;
        if (!*pending || !wcscmp(pending, L".")) {
            if (!more) { result = parents[depth]; parents[depth] = INVALID_HANDLE_VALUE; break; }
            memmove(pending, rest, (wcslen(rest) + 1) * sizeof(WCHAR)); continue;
        }
        if (!wcscmp(pending, L"..")) {
            if (!depth) { plum_tree_error("path escapes filesystem root"); break; }
            CloseHandle(parents[depth--]);
            if (!more) { result = parents[depth]; parents[depth] = INVALID_HANDLE_VALUE; break; }
            memmove(pending, rest, (wcslen(rest) + 1) * sizeof(WCHAR)); continue;
        }
        NTSTATUS failure = 0;
        HANDLE child = plum_tree_win_child(parents[depth], pending,
            (DWORD)(wcslen(pending) * sizeof(WCHAR)), GENERIC_READ, FILE_OPEN, 0, &failure);
        if (child == INVALID_HANDLE_VALUE) {
            if (location && allow_missing && !more && failure == (NTSTATUS)0xc0000034) {
                location->parent = parents[depth]; parents[depth] = INVALID_HANDLE_VALUE;
                location->missing = 1; wcscpy(location->name, pending);
            }
            if (reject_links == 2 && failure == (NTSTATUS)0xc0000034) {
                char tail[PLUM_PATH_MAX * 4];
                if (slash) *slash = L'/';
                if (WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, pending, -1, tail, sizeof(tail), NULL, NULL) &&
                    !plum_tree_missing_tail(tail, depth)) {
                    result = parents[depth]; parents[depth] = INVALID_HANDLE_VALUE;
                }
            }
            break;
        }
        FILE_ATTRIBUTE_TAG_INFO info;
        if (plum_tree_win_attrs(child, &info)) { CloseHandle(child); break; }
        if (info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) {
            int failed = 1;
            if (reject_links == 1) plum_tree_error("refused link by project policy");
            else if (++links > 40) plum_tree_error("symlink expansion exceeds 40 (cycle or excessive chain)");
            else failed = plum_tree_win_link(child, target, PLUM_PATH_MAX, 0);
            CloseHandle(child);
            if (failed) break;
            size_t count = wcslen(target), suffix = wcslen(rest);
            if (count + (more ? suffix + 1 : 0) >= PLUM_PATH_MAX) { plum_tree_error("expanded link path exceeds resolver capacity"); break; }
            if (more) { target[count++] = L'/'; memcpy(target + count, rest, (suffix + 1) * sizeof(WCHAR)); }
            wcscpy(pending, target); continue;
        }
        if (!more) {
            if (location) {
                if (!DuplicateHandle(GetCurrentProcess(), parents[depth], GetCurrentProcess(),
                    &location->parent, 0, FALSE, DUPLICATE_SAME_ACCESS)) { plum_tree_os_error("duplicate target parent"); CloseHandle(child); break; }
                wcscpy(location->name, pending);
            }
            result = child; break;
        }
        if (!(info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY)) { plum_tree_error("non-directory path component"); CloseHandle(child); break; }
        if (depth + 1 >= PLUM_COPY_DEPTH) { plum_tree_error("directory depth exceeds 128"); CloseHandle(child); break; }
        parents[++depth] = child;
        memmove(pending, rest, (wcslen(rest) + 1) * sizeof(WCHAR));
    }
    for (unsigned i = 0; i <= depth; i++) if (parents[i] != INVALID_HANDLE_VALUE) CloseHandle(parents[i]);
    return result;
}
static HANDLE plum_tree_resolve(HANDLE root, const char *relative, int reject_links) {
    return plum_tree_resolve_at(root, relative, reject_links, NULL, 0);
}
long long plum_tree_root_open(const char *path) {
    plum_tree_error_clear();
    HANDLE h = plum_copy_root(path);
    return h == INVALID_HANDLE_VALUE ? 0 : (long long)(intptr_t)h;
}
long long plum_tree_entry_open(long long root, const char *relative, long long reject_links) {
    plum_tree_error_clear();
    if (!root || root == -1) { plum_tree_error("invalid filesystem root handle"); return 0; }
    HANDLE h = plum_tree_resolve((HANDLE)(intptr_t)root, relative, reject_links != 0);
    return h == INVALID_HANDLE_VALUE ? 0 : (long long)(intptr_t)h;
}
void plum_tree_entry_close(long long entry) {
    if (entry && entry != -1) CloseHandle((HANDLE)(intptr_t)entry);
}

static int plum_tree_win_overlap(HANDLE a, HANDLE b) {
    WCHAR pa[PLUM_PATH_MAX], pb[PLUM_PATH_MAX];
    DWORD na = GetFinalPathNameByHandleW(a, pa, PLUM_PATH_MAX, FILE_NAME_NORMALIZED | VOLUME_NAME_GUID);
    DWORD nb = GetFinalPathNameByHandleW(b, pb, PLUM_PATH_MAX, FILE_NAME_NORMALIZED | VOLUME_NAME_GUID);
    if (!na || !nb || na >= PLUM_PATH_MAX || nb >= PLUM_PATH_MAX) return -1;
    while (na && pa[na - 1] == L'\\') pa[--na] = 0;
    while (nb && pb[nb - 1] == L'\\') pb[--nb] = 0;
    return (na <= nb && !_wcsnicmp(pa, pb, na) && (!pb[na] || pb[na] == L'\\')) ||
           (nb <= na && !_wcsnicmp(pb, pa, nb) && (!pa[nb] || pa[nb] == L'\\'));
}
// Inspection allows absence only for the native object-not-found status.
static int plum_tree_win_replaceable(HANDLE parent, const WCHAR *name, DWORD bytes) {
    NTSTATUS status = 0;
    HANDLE h = plum_tree_win_child(parent, name, bytes, FILE_READ_ATTRIBUTES, FILE_OPEN, 0, &status);
    if (h == INVALID_HANDLE_VALUE) {
        if (status == (NTSTATUS)0xc0000034) { plum_tree_error_clear(); return 0; }
        return -1;
    }
    FILE_ATTRIBUTE_TAG_INFO info;
    int rc = plum_tree_win_attrs(h, &info);
    if (!rc && (info.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT))) {
        plum_tree_error("destination entry is not a regular file (links and reparse points are refused)"); rc = -1;
    }
    if (!CloseHandle(h)) rc = -1;
    return rc;
}
static int plum_tree_win_publish_as(HANDLE temporary, const WCHAR *name, DWORD bytes, int replace) {
    typedef NTSTATUS (NTAPI *Rename)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, FILE_INFORMATION_CLASS);
    Rename api = (Rename)(void *)GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtSetInformationFile");
    if (!api) return -1;
    size_t size = sizeof(FILE_RENAME_INFORMATION) + bytes;
    FILE_RENAME_INFORMATION *info = calloc(1, size);
    if (!info) return -1;
    info->ReplaceIfExists = replace ? TRUE : FALSE;
    // A bare name renames within the existing handle's parent directory.
    info->RootDirectory = NULL; info->FileNameLength = bytes;
    memcpy(info->FileName, name, bytes);
    IO_STATUS_BLOCK result;
    NTSTATUS status = api(temporary, &result, info, (ULONG)size, FileRenameInformation);
    if (status < 0) plum_tree_nt_error("publish replacement", status);
    free(info);
    return status < 0 ? -1 : 0;
}
static int plum_tree_win_publish(HANDLE temporary, const WCHAR *name, DWORD bytes) {
    return plum_tree_win_publish_as(temporary, name, bytes, 1);
}
static int plum_tree_win_leaf(HANDLE input, HANDLE parent, const WCHAR *name,
                              DWORD bytes, const FILE_ATTRIBUTE_TAG_INFO *attrs,
                              unsigned char *buffer) {
    int link = !!(attrs->FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT);
    DWORD link_bytes = 0;
    if (link && (attrs->ReparseTag != IO_REPARSE_TAG_SYMLINK ||
        !DeviceIoControl(input, FSCTL_GET_REPARSE_POINT, NULL, 0, buffer,
                         MAXIMUM_REPARSE_DATA_BUFFER_SIZE, &link_bytes, NULL))) return -1;
    if (plum_tree_win_replaceable(parent, name, bytes)) return -1;
    HANDLE output = INVALID_HANDLE_VALUE;
    for (unsigned attempt = 0; attempt < 1024; attempt++) {
        WCHAR temp[80];
        int chars = swprintf(temp, 80, L".plum-copy-%lu-%u", (unsigned long)GetCurrentProcessId(), attempt);
        if (chars <= 0) return -1;
        if ((DWORD)chars * sizeof(WCHAR) == bytes && !memcmp(temp, name, bytes)) continue;
        NTSTATUS status = 0;
        output = plum_tree_win_child(parent, temp, (DWORD)chars * sizeof(WCHAR),
            GENERIC_WRITE | DELETE, FILE_CREATE,
            (link && (attrs->FileAttributes & FILE_ATTRIBUTE_DIRECTORY)) ? FILE_DIRECTORY_FILE : FILE_NON_DIRECTORY_FILE,
            &status);
        if (output != INVALID_HANDLE_VALUE) break;
        if (status != (NTSTATUS)0xc0000035) return -1; // name collision
    }
    if (output == INVALID_HANDLE_VALUE) return -1;
    int rc = 0;
    if (link) {
        DWORD ignored;
        if (!DeviceIoControl(output, FSCTL_SET_REPARSE_POINT, buffer, link_bytes,
                             NULL, 0, &ignored, NULL)) rc = -1;
    } else {
        for (;;) {
            DWORD n = 0;
            if (!ReadFile(input, buffer, PLUM_COPY_BUFFER, &n, NULL)) { rc = -1; break; }
            if (!n) break;
            DWORD offset = 0;
            while (offset < n) {
                DWORD wrote = 0;
                if (!WriteFile(output, buffer + offset, n - offset, &wrote, NULL) || !wrote) { rc = -1; break; }
                offset += wrote;
            }
            if (rc) break;
        }
    }
    if (!rc && !link && !FlushFileBuffers(output)) rc = -1;
    if (!rc && plum_tree_win_replaceable(parent, name, bytes)) rc = -1;
    if (!rc && plum_tree_win_publish(output, name, bytes)) rc = -1;
    if (rc) {
        if (!*plum_tree_error_text) plum_tree_os_error("copy entry");
        FILE_DISPOSITION_INFO remove = {TRUE};
        SetFileInformationByHandle(output, FileDispositionInfo, &remove, sizeof(remove));
    }
    if (!CloseHandle(output)) rc = -1;
    return rc;
}
typedef struct {
    HANDLE source, destination;
    unsigned char *entries;
    DWORD offset;
    int refill;
} plum_copy_frame;

static int plum_copy_tree(const char *src, const char *dst) {
    HANDLE source = plum_copy_root(src);
    if (source == INVALID_HANDLE_VALUE) return -1;
    HANDLE destination = plum_copy_root(dst);
    if (destination == INVALID_HANDLE_VALUE) { CloseHandle(source); return -1; }
    int rc = -1;
    int overlap = plum_tree_win_overlap(source, destination);
    if (overlap) {
        if (overlap > 0) plum_tree_error("source and destination roots overlap");
        else plum_tree_os_error("inspect copy root ancestry");
        goto roots;
    }
    plum_copy_frame *frames = calloc(PLUM_COPY_DEPTH, sizeof(*frames));
    unsigned char *buffer = malloc(PLUM_COPY_BUFFER);
    unsigned char *entries = malloc(4096);
    if (!frames || !buffer || !entries) { plum_tree_error("cannot allocate copy traversal stack or transfer buffer"); free(frames); free(buffer); free(entries); goto roots; }
    frames[0] = (plum_copy_frame){source, destination, entries, 0, 1};
    unsigned count = 1;
    rc = 0;
    while (count && !rc) {
        plum_copy_frame *frame = &frames[count - 1];
        if (frame->refill) {
            if (!GetFileInformationByHandleEx(frame->source, FileIdBothDirectoryInfo, frame->entries, 4096)) {
                if (GetLastError() != ERROR_NO_MORE_FILES) { rc = -1; break; }
                if (!CloseHandle(frame->source)) rc = -1;
                if (!CloseHandle(frame->destination)) rc = -1;
                free(frame->entries); count--; continue;
            }
            frame->offset = 0; frame->refill = 0;
        }
        FILE_ID_BOTH_DIR_INFO *entry = (FILE_ID_BOTH_DIR_INFO *)(frame->entries + frame->offset);
        DWORD bytes = entry->FileNameLength;
        if (!bytes || bytes % sizeof(WCHAR) || bytes > 4096 - frame->offset - offsetof(FILE_ID_BOTH_DIR_INFO, FileName)) { rc = -1; break; }
        WCHAR *name = entry->FileName;
        DWORD next = entry->NextEntryOffset;
        if (next) {
            if (next % 8 || next < offsetof(FILE_ID_BOTH_DIR_INFO, FileName) + bytes ||
                next > 4096 - frame->offset - sizeof(FILE_ID_BOTH_DIR_INFO)) { rc = -1; break; }
            frame->offset += next;
        } else frame->refill = 1;
        if ((bytes == 2 && name[0] == L'.') || (bytes == 4 && name[0] == L'.' && name[1] == L'.')) continue;
        HANDLE child = plum_tree_win_child(frame->source, name, bytes, GENERIC_READ, FILE_OPEN, 0, NULL);
        if (child == INVALID_HANDLE_VALUE) { rc = -1; break; }
        FILE_ATTRIBUTE_TAG_INFO attrs;
        if (plum_tree_win_attrs(child, &attrs)) { CloseHandle(child); rc = -1; break; }
        if (!(attrs.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) || (attrs.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) {
            rc = plum_tree_win_leaf(child, frame->destination, name, bytes, &attrs, buffer);
            if (!CloseHandle(child)) rc = -1;
            continue;
        }
        if (count >= PLUM_COPY_DEPTH) { plum_tree_error("copy directory depth exceeds 128"); CloseHandle(child); rc = -1; break; }
        HANDLE output = plum_tree_win_child(frame->destination, name, bytes,
            FILE_LIST_DIRECTORY, FILE_OPEN_IF, FILE_DIRECTORY_FILE, NULL);
        if (output == INVALID_HANDLE_VALUE) { CloseHandle(child); rc = -1; break; }
        FILE_ATTRIBUTE_TAG_INFO output_attrs;
        if (plum_tree_win_attrs(output, &output_attrs) ||
            (output_attrs.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) {
            CloseHandle(output); CloseHandle(child); rc = -1; break;
        }
        entries = malloc(4096);
        if (!entries) { CloseHandle(output); CloseHandle(child); rc = -1; break; }
        frames[count++] = (plum_copy_frame){child, output, entries, 0, 1};
    }
    if (rc && !*plum_tree_error_text) plum_tree_os_error("copy directory entry");
    while (count) {
        count--;
        CloseHandle(frames[count].source); CloseHandle(frames[count].destination);
        free(frames[count].entries);
    }
    free(frames); free(buffer);
    return rc;
roots:
    CloseHandle(source); CloseHandle(destination);
    return rc;
}

#else
// POSIX copy traversal owns every directory descriptor it descends through.
// Only the caller-supplied root ancestors are resolved as ordinary paths.
static int plum_copy_same(const struct stat *a, const struct stat *b) {
    return a->st_dev == b->st_dev && a->st_ino == b->st_ino;
}

static int plum_copy_root(const char *path) {
    size_t n = strlen(path);
    if (!n) { plum_tree_error("empty root path"); return -1; }
    char *name = strdup(path);
    if (!name) { plum_tree_os_error("allocate root path"); return -1; }
    while (n > 1 && name[n - 1] == '/') name[--n] = 0;
    int fd = open(name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) plum_tree_os_error("open root directory without following links");
    free(name);
    return fd;
}

// Root-relative resolver shared by compiler consumers. The returned fd owns
// the resolved entry; callers must use that fd, never reopen the input path.
// Links are interpreted ourselves, so absolute targets and attempts to climb
// above the held root cannot escape through OS pathname resolution.
typedef struct { int parent; char name[PLUM_PATH_MAX]; int missing; } plum_tree_location;
static int plum_tree_resolve_at(int root, const char *relative, int reject_links, plum_tree_location *location, int allow_missing) {
    if (location) { location->parent = -1; location->missing = 0; }
    int parents[PLUM_COPY_DEPTH];
    unsigned depth = 0, links = 0;
    int result = -1;
    char pending[PLUM_PATH_MAX], target[PLUM_PATH_MAX];
    size_t length = strlen(relative);
    if (!length) { plum_tree_error("empty relative path"); return -1; }
    if (length >= sizeof(pending)) { plum_tree_error("relative path exceeds resolver capacity"); return -1; }
    if (relative[0] == '/') { plum_tree_error("absolute path refused inside filesystem root"); return -1; }
    memcpy(pending, relative, length + 1);
    parents[0] = openat(root, ".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (parents[0] < 0) { plum_tree_os_error("open root directory"); return -1; }
    for (;;) {
        char *slash = strchr(pending, '/');
        char *rest = slash ? slash + 1 : pending + strlen(pending);
        int more = slash != NULL;
        if (slash) *slash = 0;
        if (!*pending || !strcmp(pending, ".")) {
            if (!more) {
                result = parents[depth]; parents[depth] = -1; break;
            }
            memmove(pending, rest, strlen(rest) + 1);
            continue;
        }
        if (!strcmp(pending, "..")) {
            if (!depth) { plum_tree_error("path escapes filesystem root"); break; }
            close(parents[depth--]);
            if (!more) { result = parents[depth]; parents[depth] = -1; break; }
            memmove(pending, rest, strlen(rest) + 1);
            continue;
        }
        struct stat before, after;
        if (fstatat(parents[depth], pending, &before, AT_SYMLINK_NOFOLLOW)) {
            plum_tree_os_error("inspect path component");
            if (location && allow_missing && !more && errno == ENOENT) {
                location->parent = parents[depth]; parents[depth] = -1;
                location->missing = 1; strcpy(location->name, pending);
            }
            if (reject_links == 2 && errno == ENOENT) {
                if (slash) *slash = '/';
                if (!plum_tree_missing_tail(pending, depth)) {
                    result = parents[depth]; parents[depth] = -1;
                }
            }
            break;
        }
        if (S_ISLNK(before.st_mode)) {
            if (reject_links == 1) { plum_tree_error("refused link by project policy"); break; }
            if (++links > 40) { plum_tree_error("symlink expansion exceeds 40 (cycle or excessive chain)"); break; }
            ssize_t n = readlinkat(parents[depth], pending, target, sizeof(target));
            if (n < 0) { plum_tree_os_error("read symbolic link"); break; }
            if (!n || (size_t)n >= sizeof(target)) { plum_tree_error("symbolic link target is empty or exceeds resolver capacity"); break; }
            if (target[0] == '/') { plum_tree_error("absolute link target refused inside filesystem root"); break; }
            if (fstatat(parents[depth], pending, &after, AT_SYMLINK_NOFOLLOW)) { plum_tree_os_error("reinspect symbolic link"); break; }
            if (!plum_copy_same(&before, &after)) { plum_tree_error("symbolic link identity changed during resolution"); break; }
            size_t suffix = strlen(rest);
            if ((size_t)n + (more ? suffix + 1 : 0) >= sizeof(target)) { plum_tree_error("expanded link path exceeds resolver capacity"); break; }
            if (more) {
                target[n++] = '/';
                memcpy(target + n, rest, suffix + 1);
            } else target[n] = 0;
            strcpy(pending, target);
            continue;
        }
        if (!S_ISREG(before.st_mode) && !S_ISDIR(before.st_mode)) { plum_tree_error("unsupported filesystem entry (expected regular file or directory)"); break; }
        int flags = O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC;
        if (more || S_ISDIR(before.st_mode)) flags |= O_DIRECTORY;
        int child = openat(parents[depth], pending, flags);
        if (child < 0) { plum_tree_os_error("open path component"); break; }
        if (fstat(child, &after)) { plum_tree_os_error("inspect opened entry"); close(child); break; }
        if (!plum_copy_same(&before, &after)) { plum_tree_error("entry identity changed during resolution"); close(child); break; }
        if (!more) {
            if (location) {
                location->parent = openat(parents[depth], ".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
                if (location->parent < 0) { plum_tree_os_error("open target parent"); close(child); break; }
                strcpy(location->name, pending);
            }
            result = child; break;
        }
        if (depth + 1 >= PLUM_COPY_DEPTH) { plum_tree_error("directory depth exceeds 128"); close(child); break; }
        parents[++depth] = child;
        memmove(pending, rest, strlen(rest) + 1);
    }
    for (unsigned i = 0; i <= depth; i++) if (parents[i] >= 0) close(parents[i]);
    return result;
}

// Native owned-entry ABI. Zero is failure; successful values are fd + 1.
// These are internal compiler primitives, not additions to the public Os API.
static int plum_tree_resolve(int root, const char *relative, int reject_links) {
    return plum_tree_resolve_at(root, relative, reject_links, NULL, 0);
}
long long plum_tree_root_open(const char *path) {
    plum_tree_error_clear();
    int fd = plum_copy_root(path);
    return fd < 0 ? 0 : (long long)fd + 1;
}
long long plum_tree_entry_open(long long root, const char *relative, long long reject_links) {
    plum_tree_error_clear();
    if (root <= 0 || root > INT32_MAX) { plum_tree_error("invalid filesystem root handle"); return 0; }
    int fd = plum_tree_resolve((int)(root - 1), relative, reject_links != 0);
    return fd < 0 ? 0 : (long long)fd + 1;
}
void plum_tree_entry_close(long long entry) {
    if (entry > 0 && entry <= INT32_MAX) close((int)(entry - 1));
}

// Compare actual opened roots and their ancestors, before writing anything.
static int plum_copy_ancestor(int root, int child) {
    struct stat wanted, current, parent;
    if (fstat(root, &wanted)) return -1;
    int fd = openat(child, ".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) return -1;
    int rc = -1;
    for (unsigned depth = 0; depth < 4096; depth++) {
        if (fstat(fd, &current)) break;
        if (plum_copy_same(&wanted, &current)) { rc = 1; break; }
        int next = openat(fd, "..", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
        if (next < 0) break;
        if (fstat(next, &parent)) { close(next); break; }
        if (plum_copy_same(&current, &parent)) { close(next); rc = 0; break; }
        close(fd);
        fd = next;
    }
    close(fd);
    return rc;
}

// Only ordinary files can be replaced. In particular, never open an existing
// destination for writing: it may be a hard link into an unrelated tree.
static int plum_copy_replaceable(int dst, const char *name) {
    struct stat st;
    if (fstatat(dst, name, &st, AT_SYMLINK_NOFOLLOW) == 0) {
        if (S_ISREG(st.st_mode)) return 0;
        plum_tree_error("destination entry is not a regular file (links are refused)"); return -1;
    }
    if (errno == ENOENT) { plum_tree_error_clear(); return 0; }
    plum_tree_os_error("inspect copy destination"); return -1;
}

static int plum_copy_leaf(int src, int dst, const char *name,
                          const struct stat *expected, char *buffer) {
    int in = -1, out = -1, rc = -1;
    char temporary[80], target[PLUM_PATH_MAX];
    int is_link = S_ISLNK(expected->st_mode);
    if (!is_link && !S_ISREG(expected->st_mode)) { plum_tree_error("unsupported copy source entry"); return -1; }
    if (plum_copy_replaceable(dst, name)) return -1;
    if (is_link) {
        ssize_t n = readlinkat(src, name, target, sizeof(target));
        if (n < 0) { plum_tree_os_error("read copy source link"); return -1; }
        if ((size_t)n >= sizeof(target)) { plum_tree_error("copy source link exceeds capacity"); return -1; }
        target[n] = 0;
        struct stat after;
        if (fstatat(src, name, &after, AT_SYMLINK_NOFOLLOW)) { plum_tree_os_error("reinspect copy source link"); return -1; }
        if (!plum_copy_same(expected, &after)) { plum_tree_error("source link identity changed during copying"); return -1; }
    } else {
        in = openat(src, name, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC);
        if (in < 0) return -1;
        struct stat opened;
        if (fstat(in, &opened)) { plum_tree_os_error("inspect opened copy source"); goto done; }
        if (!S_ISREG(opened.st_mode) || !plum_copy_same(expected, &opened)) { plum_tree_error("source identity changed during copying"); goto done; }
    }
    // No shared counter and no overwrite of another operation's temporary.
    // Exclusive creation makes collisions harmless, including concurrent calls.
    unsigned attempt;
    for (attempt = 0; attempt < 1024; attempt++) {
        snprintf(temporary, sizeof(temporary), ".plum-copy-%ld-%u", (long)getpid(), attempt);
        if (strcmp(temporary, name) == 0) continue;
        if (is_link) {
            if (symlinkat(target, dst, temporary) == 0) break;
        } else {
            out = openat(dst, temporary, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600);
            if (out >= 0) break;
        }
        if (errno != EEXIST) goto done;
    }
    if (attempt == 1024) goto done;
    rc = 0;
    if (!is_link) {
        for (;;) {
            ssize_t n = read(in, buffer, PLUM_COPY_BUFFER);
            if (n < 0 && errno == EINTR) continue;
            if (n < 0) { rc = -1; break; }
            if (!n) break;
            ssize_t offset = 0;
            while (offset < n) {
                ssize_t wrote = write(out, buffer + offset, (size_t)(n - offset));
                if (wrote < 0 && errno == EINTR) continue;
                if (wrote <= 0) { rc = -1; break; }
                offset += wrote;
            }
            if (rc) break;
        }
        if (close(out)) rc = -1;
        out = -1;
        if (close(in)) rc = -1;
        in = -1;
    }
    if (!rc && plum_copy_replaceable(dst, name)) rc = -1;
    if (!rc && renameat(dst, temporary, dst, name)) rc = -1;
    if (rc && !*plum_tree_error_text) plum_tree_os_error("copy entry");
    if (rc) unlinkat(dst, temporary, 0);
done:
    if (rc && !*plum_tree_error_text) plum_tree_os_error("copy entry");
    if (in >= 0) close(in);
    if (out >= 0) close(out);
    return rc;
}

typedef struct { DIR *source; int destination; } plum_copy_frame;

static int plum_copy_tree(const char *src, const char *dst) {
    int source = plum_copy_root(src);
    if (source < 0) return -1;
    int destination = plum_copy_root(dst);
    if (destination < 0) { close(source); return -1; }
    int rc = -1;
    int ancestry = plum_copy_ancestor(source, destination);
    if (!ancestry) ancestry = plum_copy_ancestor(destination, source);
    if (ancestry) {
        if (ancestry > 0) plum_tree_error("source and destination roots overlap");
        else plum_tree_os_error("inspect copy root ancestry");
        goto roots;
    }
    plum_copy_frame *frames = calloc(PLUM_COPY_DEPTH, sizeof(*frames));
    char *buffer = malloc(PLUM_COPY_BUFFER);
    if (!frames || !buffer) { plum_tree_error("cannot allocate copy traversal stack or transfer buffer"); free(frames); free(buffer); goto roots; }
    frames[0].source = fdopendir(source);
    if (!frames[0].source) { free(frames); free(buffer); goto roots; }
    frames[0].destination = destination;
    unsigned count = 1;
    rc = 0;
    while (count && !rc) {
        plum_copy_frame *frame = &frames[count - 1];
        errno = 0;
        struct dirent *entry = readdir(frame->source);
        if (!entry) {
            if (errno) { rc = -1; break; }
            if (closedir(frame->source)) rc = -1;
            if (close(frame->destination)) rc = -1;
            count--;
            continue;
        }
        const char *name = entry->d_name;
        if (!strcmp(name, ".") || !strcmp(name, "..")) continue;
        int parent = dirfd(frame->source);
        struct stat st;
        if (fstatat(parent, name, &st, AT_SYMLINK_NOFOLLOW)) { rc = -1; break; }
        if (!S_ISDIR(st.st_mode)) {
            rc = plum_copy_leaf(parent, frame->destination, name, &st, buffer);
            continue;
        }
        if (count >= PLUM_COPY_DEPTH) { plum_tree_error("directory depth exceeds 128"); rc = -1; break; }
        int child = openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
        if (child < 0) { rc = -1; break; }
        struct stat opened;
        if (fstat(child, &opened)) { plum_tree_os_error("inspect copy source directory"); close(child); rc = -1; break; }
        if (!plum_copy_same(&st, &opened)) {
            plum_tree_error("source directory identity changed during copying"); close(child); rc = -1; break;
        }
        if (mkdirat(frame->destination, name, 0700) && errno != EEXIST) {
            close(child); rc = -1; break;
        }
        int output = openat(frame->destination, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
        if (output < 0) { close(child); rc = -1; break; }
        DIR *stream = fdopendir(child);
        if (!stream) { close(child); close(output); rc = -1; break; }
        frames[count++] = (plum_copy_frame){stream, output};
    }
    if (rc && !*plum_tree_error_text) plum_tree_os_error("copy directory entry");
    while (count) {
        count--;
        closedir(frames[count].source);
        close(frames[count].destination);
    }
    free(buffer);
    free(frames);
    return rc;
roots:
    close(source);
    close(destination);
    return rc;
}
#endif

long long plum_tree_validate_link(long long root, const char *relative) {
    plum_tree_error_clear();
    if (!root) return -1;
#if defined(_WIN32)
    HANDLE h = plum_tree_resolve((HANDLE)(intptr_t)root, relative, 2);
    if (h == INVALID_HANDLE_VALUE) return -1;
    return CloseHandle(h) ? 0 : -1;
#else
    int fd = plum_tree_resolve((int)(root - 1), relative, 2);
    if (fd < 0) return -1;
    return close(fd) ? -1 : 0;
#endif
}

// Compiler filesystem consumers work on opened entries. Iterators and readers
// consume an entry even on allocation failure, and own all borrowed buffers.
long long plum_tree_entry_kind(long long entry) {
    if (!entry) return -1;
#if defined(_WIN32)
    FILE_ATTRIBUTE_TAG_INFO info;
    if (plum_tree_win_attrs((HANDLE)(intptr_t)entry, &info)) return -1;
    if (info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) return 3;
    if (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) return 2;
    return GetFileType((HANDLE)(intptr_t)entry) == FILE_TYPE_DISK ? 1 : 4;
#else
    struct stat info;
    if (fstat((int)(entry - 1), &info)) return -1;
    return S_ISREG(info.st_mode) ? 1 : S_ISDIR(info.st_mode) ? 2 : S_ISLNK(info.st_mode) ? 3 : 4;
#endif
}
// Inspect one child without following it. Missing is distinct from refusal
// or I/O failure; notably a dangling link is present. No directory inventory
// or source-content allocation is needed for manifest lookup.
long long plum_tree_child_kind(long long root, const char *name) {
    plum_tree_error_clear();
    if (!root || !name || !*name || !strcmp(name, ".") || !strcmp(name, "..") || strchr(name, '/')) { plum_tree_error("child lookup requires a valid root and one ordinary path component"); return -1; }
#if defined(_WIN32)
    if (strchr(name, '\\') || strchr(name, ':')) return -1;
    WCHAR wide[PLUM_PATH_MAX];
    int count = plum_tree_win_path(name, wide, PLUM_PATH_MAX);
    if (count <= 1) return -1;
    NTSTATUS status = 0;
    HANDLE child = plum_tree_win_child((HANDLE)(intptr_t)root, wide,
        (DWORD)((count - 1) * sizeof(WCHAR)), FILE_READ_ATTRIBUTES, FILE_OPEN, 0, &status);
    if (child == INVALID_HANDLE_VALUE) return status == (NTSTATUS)0xc0000034 ? 0 : -1;
    long long kind = plum_tree_entry_kind((long long)(intptr_t)child);
    if (!CloseHandle(child)) return -1;
    return kind;
#else
    struct stat info;
    if (fstatat((int)(root - 1), name, &info, AT_SYMLINK_NOFOLLOW)) {
        if (errno == ENOENT) return 0;
        plum_tree_os_error("inspect child entry"); return -1;
    }
    return S_ISREG(info.st_mode) ? 1 : S_ISDIR(info.st_mode) ? 2 : S_ISLNK(info.st_mode) ? 3 : 4;
#endif
}

const char *plum_tree_entry_identity(long long entry) {
    static PLUM_OS_TLS char identity[80];
    identity[0] = 0;
    if (!entry) return identity;
#if defined(_WIN32)
    BY_HANDLE_FILE_INFORMATION info;
    if (GetFileInformationByHandle((HANDLE)(intptr_t)entry, &info))
        snprintf(identity, sizeof(identity), "%08lx:%08lx%08lx", (unsigned long)info.dwVolumeSerialNumber,
                 (unsigned long)info.nFileIndexHigh, (unsigned long)info.nFileIndexLow);
#else
    struct stat info;
    if (!fstat((int)(entry - 1), &info))
        snprintf(identity, sizeof(identity), "%llx:%llx", (unsigned long long)info.st_dev, (unsigned long long)info.st_ino);
#endif
    return identity;
}
// Replace the resolved regular target, leaving any traversed symlink intact.
// Both temporary creation and publication use the held target parent. The
// expected identity prevents writing a file replaced since discovery/read.
// An empty expected identity requests exclusive creation of a missing target.
long long plum_tree_write(long long root, const char *relative, long long reject_links,
                          const char *expected, const char *bytes, long long size) {
    plum_tree_error_clear();
    if (!root || !expected || size < 0 || (unsigned long long)size > SIZE_MAX) return -1;
    plum_tree_location location;
    int rc = -1;
#if defined(_WIN32)
    HANDLE input = plum_tree_resolve_at((HANDLE)(intptr_t)root, relative, reject_links != 0, &location, !*expected);
    HANDLE output = INVALID_HANDLE_VALUE;
    if (input == INVALID_HANDLE_VALUE && !location.missing) return -1;
    if (location.parent == INVALID_HANDLE_VALUE || (!location.missing &&
        (plum_tree_entry_kind((long long)(intptr_t)input) != 1 ||
         strcmp(plum_tree_entry_identity((long long)(intptr_t)input), expected)))) { plum_tree_error("source target identity or type changed before replacement"); goto win_done; }
    plum_tree_error_clear();
    DWORD name_bytes = (DWORD)(wcslen(location.name) * sizeof(WCHAR));
    for (unsigned attempt = 0; attempt < 1024; attempt++) {
        WCHAR temporary[80];
        int count = swprintf(temporary, 80, L".plum-write-%lu-%u", (unsigned long)GetCurrentProcessId(), attempt);
        if (count <= 0) goto win_done;
        if (!wcscmp(temporary, location.name)) continue;
        NTSTATUS status = 0;
        output = plum_tree_win_child(location.parent, temporary, (DWORD)count * sizeof(WCHAR),
            GENERIC_WRITE | DELETE, FILE_CREATE, FILE_NON_DIRECTORY_FILE, &status);
        if (output != INVALID_HANDLE_VALUE) break;
        if (status != (NTSTATUS)0xc0000035) goto win_done;
    }
    if (output == INVALID_HANDLE_VALUE) goto win_done;
    plum_tree_error_clear();
    for (size_t offset = 0; offset < (size_t)size;) {
        size_t count = (size_t)size - offset;
        if (count > PLUM_COPY_BUFFER) count = PLUM_COPY_BUFFER;
        DWORD wrote;
        if (!WriteFile(output, bytes + offset, (DWORD)count, &wrote, NULL) || !wrote) goto win_done;
        offset += wrote;
    }
    if (!FlushFileBuffers(output)) goto win_done;
    // The read handle pins the original until the complete replacement exists.
    // Windows requires releasing its delete-denying share before publication.
    if (input != INVALID_HANDLE_VALUE && !CloseHandle(input)) { input = INVALID_HANDLE_VALUE; goto win_done; }
    input = INVALID_HANDLE_VALUE;
    if ((!location.missing && plum_tree_win_replaceable(location.parent, location.name, name_bytes)) ||
        plum_tree_win_publish_as(output, location.name, name_bytes, !location.missing)) goto win_done;
    rc = 0;
win_done:
    if (rc && !*plum_tree_error_text) plum_tree_os_error("replace source target");
    if (output != INVALID_HANDLE_VALUE) {
        if (rc) {
            FILE_DISPOSITION_INFO remove = {TRUE};
            SetFileInformationByHandle(output, FileDispositionInfo, &remove, sizeof(remove));
        }
        if (!CloseHandle(output)) rc = -1;
    }
    if (input != INVALID_HANDLE_VALUE) CloseHandle(input);
    if (location.parent != INVALID_HANDLE_VALUE) CloseHandle(location.parent);
#else
    if (root < 1 || root > INT32_MAX) return -1;
    int input = plum_tree_resolve_at((int)(root - 1), relative, reject_links != 0, &location, !*expected);
    if (input < 0 && !location.missing) return -1;
    int output = -1;
    char temporary[80] = {0};
    struct stat info, current;
    if (location.parent < 0 || (!location.missing &&
        (fstat(input, &info) || !S_ISREG(info.st_mode) ||
         strcmp(plum_tree_entry_identity((long long)input + 1), expected)))) { plum_tree_error("source target identity or type changed before replacement"); goto posix_done; }
    plum_tree_error_clear();
    for (unsigned attempt = 0; attempt < 1024; attempt++) {
        snprintf(temporary, sizeof(temporary), ".plum-write-%ld-%u", (long)getpid(), attempt);
        if (!strcmp(temporary, location.name)) continue;
        output = openat(location.parent, temporary, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600);
        if (output >= 0) break;
        if (errno != EEXIST) goto posix_done;
    }
    if (output < 0) goto posix_done;
    plum_tree_error_clear();
    for (size_t offset = 0; offset < (size_t)size;) {
        size_t count = (size_t)size - offset;
        if (count > PLUM_COPY_BUFFER) count = PLUM_COPY_BUFFER;
        ssize_t wrote = write(output, bytes + offset, count);
        if (wrote < 0 && errno == EINTR) continue;
        if (wrote <= 0) goto posix_done;
        offset += (size_t)wrote;
    }
    if ((!location.missing && fchmod(output, info.st_mode & 0777)) || fsync(output)) goto posix_done;
    if (location.missing) {
        // linkat publishes only if the leaf is still absent. A racing creator
        // must never be overwritten by an unsaved editor buffer.
        if (linkat(location.parent, temporary, location.parent, location.name, 0) ||
            unlinkat(location.parent, temporary, 0)) goto posix_done;
    } else {
        if (fstatat(location.parent, location.name, &current, AT_SYMLINK_NOFOLLOW) ||
            !S_ISREG(current.st_mode) || !plum_copy_same(&info, &current)) { plum_tree_error("source target identity or type changed before publication"); goto posix_done; }
        if (renameat(location.parent, temporary, location.parent, location.name)) goto posix_done;
    }
    rc = 0;
posix_done:
    if (rc && !*plum_tree_error_text) plum_tree_os_error("replace source target");
    // Only an exclusively created temporary belongs to this operation.
    if (output >= 0) {
        if (rc) unlinkat(location.parent, temporary, 0);
        if (close(output)) rc = -1;
    }
    if (input >= 0) close(input);
    if (location.parent >= 0) close(location.parent);
#endif
    return rc;
}
long long plum_tree_entry_size(long long entry) {
    if (plum_tree_entry_kind(entry) != 1) { plum_tree_error("expected a regular file"); return -1; }
#if defined(_WIN32)
    LARGE_INTEGER size;
    if (GetFileSizeEx((HANDLE)(intptr_t)entry, &size)) return size.QuadPart;
    plum_tree_os_error("read file size"); return -1;
#else
    struct stat st;
    if (!fstat((int)(entry - 1), &st)) return st.st_size;
    plum_tree_os_error("read file size"); return -1;
#endif
}
static int plum_tree_rewind(long long entry) {
#if defined(_WIN32)
    LARGE_INTEGER zero; zero.QuadPart = 0;
    if (SetFilePointerEx((HANDLE)(intptr_t)entry, zero, NULL, FILE_BEGIN)) return 0;
    plum_tree_os_error("rewind file"); return -1;
#else
    if (lseek((int)(entry - 1), 0, SEEK_SET) >= 0) return 0;
    plum_tree_os_error("rewind file"); return -1;
#endif
}
static long long plum_tree_read(long long entry, unsigned char *buffer, size_t capacity) {
#if defined(_WIN32)
    DWORD got;
    if (ReadFile((HANDLE)(intptr_t)entry, buffer, (DWORD)capacity, &got, NULL)) return (long long)got;
    plum_tree_os_error("read file bytes"); return -1;
#else
    ssize_t got;
    do { got = read((int)(entry - 1), buffer, capacity); } while (got < 0 && errno == EINTR);
    return got;
#endif
}
typedef struct { long long entry; unsigned char bytes[PLUM_COPY_BUFFER]; } plum_tree_reader;
long long plum_tree_reader_open(long long entry) {
    if (!entry) return 0;
    plum_tree_reader *reader = NULL;
    if (plum_tree_entry_kind(entry) == 1) reader = malloc(sizeof(*reader));
    if (!reader) { plum_tree_entry_close(entry); return 0; }
    reader->entry = entry;
    return (long long)(intptr_t)reader;
}
long long plum_tree_reader_next(long long handle) {
    plum_tree_reader *reader = (plum_tree_reader *)(intptr_t)handle;
    return reader ? plum_tree_read(reader->entry, reader->bytes, sizeof(reader->bytes)) : -1;
}
const char *plum_tree_reader_data(long long handle) {
    plum_tree_reader *reader = (plum_tree_reader *)(intptr_t)handle;
    return reader ? (const char *)reader->bytes : "";
}
void plum_tree_reader_close(long long handle) {
    plum_tree_reader *reader = (plum_tree_reader *)(intptr_t)handle;
    if (reader) { plum_tree_entry_close(reader->entry); free(reader); }
}
// Parser input owns one contiguous allocation; the descriptor is consumed on
// every path. Unlike hashing, parsing requires the complete source text.
typedef struct { size_t size; unsigned char bytes[]; } plum_tree_text;
long long plum_tree_text_open(long long entry) {
    plum_tree_error_clear();
    long long size = plum_tree_entry_size(entry);
    plum_tree_text *text = NULL;
    if (size < 0 || (unsigned long long)size > SIZE_MAX - sizeof(*text) - 1 ||
        plum_tree_rewind(entry)) goto done;
    text = malloc(sizeof(*text) + (size_t)size + 1);
    if (!text) { plum_tree_error("cannot allocate complete source text"); goto done; }
    text->size = (size_t)size;
    size_t offset = 0;
    while (offset < text->size) {
        size_t count = text->size - offset;
        if (count > PLUM_COPY_BUFFER) count = PLUM_COPY_BUFFER;
        long long got = plum_tree_read(entry, text->bytes + offset, count);
        if (got <= 0) { if (!got) plum_tree_error("file shortened while reading source text"); goto fail; }
        offset += (size_t)got;
    }
    unsigned char extra;
    if (plum_tree_read(entry, &extra, 1) != 0 || plum_tree_entry_size(entry) != size) {
        if (!*plum_tree_error_text) plum_tree_error("file size changed while reading source text");
        goto fail;
    }
    text->bytes[text->size] = 0;
    goto done;
fail:
    free(text); text = NULL;
done:
    plum_tree_entry_close(entry);
    return (long long)(intptr_t)text;
}
long long plum_tree_text_size(long long raw) {
    plum_tree_text *text = (plum_tree_text *)(intptr_t)raw;
    return text ? (long long)text->size : -1;
}
const char *plum_tree_text_data(long long raw) {
    plum_tree_text *text = (plum_tree_text *)(intptr_t)raw;
    return text ? (const char *)text->bytes : "";
}
void plum_tree_text_close(long long raw) { free((void *)(intptr_t)raw); }

// Discovery's identity index grows geometrically, independently of language
// array/map copy-on-write behavior. Values retain the first logical path for
// duplicate diagnostics. No descriptors are retained by this index.
typedef struct { char *identity; char *path; } plum_tree_seen_slot;
typedef struct {
    plum_tree_seen_slot *slots;
    size_t capacity, count;
    const char *previous;
} plum_tree_seen;
static size_t plum_tree_seen_hash(const char *key) {
    size_t hash = 5381;
    for (; *key; key++) hash = (hash * 33) ^ (unsigned char)*key;
    return hash;
}
long long plum_tree_seen_open(void) {
    return (long long)(intptr_t)calloc(1, sizeof(plum_tree_seen));
}
long long plum_tree_seen_find(long long raw, const char *identity) {
    plum_tree_seen *seen = (plum_tree_seen *)(intptr_t)raw;
    if (!seen || !identity || !*identity) return -1;
    seen->previous = NULL;
    if (seen->capacity) {
        size_t at = plum_tree_seen_hash(identity) & (seen->capacity - 1);
        while (seen->slots[at].identity) {
            if (!strcmp(seen->slots[at].identity, identity)) {
                seen->previous = seen->slots[at].path;
                return 1;
            }
            at = (at + 1) & (seen->capacity - 1);
        }
    }
    return 0;
}
long long plum_tree_seen_add(long long raw, const char *identity, const char *path) {
    if (!path) return -1;
    long long found = plum_tree_seen_find(raw, identity);
    if (found != 0) return found;
    plum_tree_seen *seen = (plum_tree_seen *)(intptr_t)raw;
    if (seen->count >= seen->capacity / 2) {
        size_t capacity = seen->capacity ? seen->capacity * 2 : 32;
        if (capacity < seen->capacity || capacity > SIZE_MAX / sizeof(plum_tree_seen_slot)) return -1;
        plum_tree_seen_slot *slots = calloc(capacity, sizeof(*slots));
        if (!slots) return -1;
        for (size_t i = 0; i < seen->capacity; i++) {
            plum_tree_seen_slot slot = seen->slots[i];
            if (!slot.identity) continue;
            size_t at = plum_tree_seen_hash(slot.identity) & (capacity - 1);
            while (slots[at].identity) at = (at + 1) & (capacity - 1);
            slots[at] = slot;
        }
        free(seen->slots); seen->slots = slots; seen->capacity = capacity;
    }
    char *key = malloc(strlen(identity) + 1), *value = malloc(strlen(path) + 1);
    if (!key || !value) { free(key); free(value); return -1; }
    strcpy(key, identity); strcpy(value, path);
    size_t at = plum_tree_seen_hash(identity) & (seen->capacity - 1);
    while (seen->slots[at].identity) at = (at + 1) & (seen->capacity - 1);
    seen->slots[at].identity = key; seen->slots[at].path = value; seen->count++;
    return 0;
}
const char *plum_tree_seen_previous(long long raw) {
    plum_tree_seen *seen = (plum_tree_seen *)(intptr_t)raw;
    return seen && seen->previous ? seen->previous : "";
}
void plum_tree_seen_close(long long raw) {
    plum_tree_seen *seen = (plum_tree_seen *)(intptr_t)raw;
    if (!seen) return;
    for (size_t i = 0; i < seen->capacity; i++) {
        free(seen->slots[i].identity); free(seen->slots[i].path);
    }
    free(seen->slots); free(seen);
}
// Hash the already-open file in bounded memory. Legacy framing remains byte
// identical to the original Plum implementation; version 2 uses a content hash.
const char *plum_tree_file_hash(long long entry, const char *relative, long long version) {
    plum_tree_error_clear();
    static PLUM_OS_TLS char digest[65];
    digest[0] = 0;
    long long size = plum_tree_entry_size(entry);
    if (size < 0 || (version != 1 && version != 2) || plum_tree_rewind(entry)) return digest;
    plum_cache_sha hash; plum_cache_init(&hash);
    if (version == 1) {
        char number[32];
        int n = snprintf(number, sizeof(number), "%llu\n", (unsigned long long)strlen(relative));
        plum_cache_update(&hash, (unsigned char *)number, (size_t)n);
        plum_cache_update(&hash, (const unsigned char *)relative, strlen(relative));
        n = snprintf(number, sizeof(number), "\n%lld\n", size);
        plum_cache_update(&hash, (unsigned char *)number, (size_t)n);
    }
    unsigned char buffer[PLUM_COPY_BUFFER];
    long long total = 0;
    for (;;) {
        long long n = plum_tree_read(entry, buffer, sizeof(buffer));
        if (n < 0) return digest;
        if (n > size - total) { plum_tree_error("file grew while hashing"); return digest; }
        if (!n) break;
        total += n;
        plum_cache_update(&hash, buffer, (size_t)n);
    }
    if (total != size || plum_tree_entry_size(entry) != size) {
        if (!*plum_tree_error_text) plum_tree_error("file size changed while hashing");
        return digest;
    }
    plum_cache_final(&hash, digest);
    return digest;
}
// Incremental final tree hash: callers never build an Array[Int] of all file
// digests. Handles own their state; finish borrows the state-owned hex string.
typedef struct { plum_cache_sha hash; char digest[65]; int finished; } plum_tree_hasher;
long long plum_tree_hash_open(void) {
    plum_tree_hasher *h = calloc(1, sizeof(*h));
    if (!h) return 0;
    plum_cache_init(&h->hash);
    return (long long)(intptr_t)h;
}
long long plum_tree_hash_update(long long handle, const char *bytes, long long size) {
    plum_tree_hasher *h = (plum_tree_hasher *)(intptr_t)handle;
    if (!h || h->finished || size < 0 || (uint64_t)size > SIZE_MAX) return -1;
    plum_cache_update(&h->hash, (const unsigned char *)bytes, (size_t)size);
    return 0;
}
const char *plum_tree_hash_finish(long long handle) {
    plum_tree_hasher *h = (plum_tree_hasher *)(intptr_t)handle;
    if (!h) return "";
    if (!h->finished) { plum_cache_final(&h->hash, h->digest); h->finished = 1; }
    return h->digest;
}
void plum_tree_hash_close(long long handle) { free((void *)(intptr_t)handle); }

typedef struct {
#if defined(_WIN32)
    HANDLE entry;
    _Alignas(max_align_t) unsigned char entries[4096];
    DWORD offset;
    int refill, first;
#else
    DIR *stream;
#endif
    char name[PLUM_PATH_MAX], link[PLUM_PATH_MAX];
    int kind, read_links;
} plum_tree_directory;
long long plum_tree_dir_open_mode(long long entry, long long read_links) {
    if (!entry) return 0;
    plum_tree_error_clear();
    plum_tree_directory *dir = NULL;
    if (plum_tree_entry_kind(entry) == 2) dir = calloc(1, sizeof(*dir));
    if (!dir) { plum_tree_error("expected directory or unable to allocate directory iterator"); plum_tree_entry_close(entry); return 0; }
    dir->read_links = read_links != 0;
#if defined(_WIN32)
    // An empty NT relative name opens the held directory itself with a new
    // file object (and independent enumeration position). ReOpenFile rejects
    // directory handles on some Windows filesystems/runners.
    dir->entry = plum_tree_win_child((HANDLE)(intptr_t)entry, L"", 0,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES, FILE_OPEN, FILE_DIRECTORY_FILE, NULL);
    plum_tree_entry_close(entry);
    if (dir->entry == INVALID_HANDLE_VALUE) { free(dir); return 0; }
    dir->first = 1; dir->refill = 1;
#else
    dir->stream = fdopendir((int)(entry - 1));
    if (!dir->stream) { plum_tree_os_error("open directory iterator"); plum_tree_entry_close(entry); free(dir); return 0; }
#endif
    return (long long)(intptr_t)dir;
}
long long plum_tree_dir_open(long long entry) { return plum_tree_dir_open_mode(entry, 1); }
long long plum_tree_dir_next(long long handle) {
    plum_tree_error_clear();
    plum_tree_directory *dir = (plum_tree_directory *)(intptr_t)handle;
    if (!dir) return -1;
    dir->name[0] = 0; dir->link[0] = 0; dir->kind = 0;
    for (;;) {
#if defined(_WIN32)
        if (dir->refill) {
            FILE_INFO_BY_HANDLE_CLASS kind = dir->first ? FileIdBothDirectoryRestartInfo : FileIdBothDirectoryInfo;
            if (!GetFileInformationByHandleEx(dir->entry, kind, dir->entries, sizeof(dir->entries))) {
                if (GetLastError() == ERROR_NO_MORE_FILES) return 0;
                plum_tree_os_error("enumerate directory"); return -1;
            }
            dir->first = 0; dir->refill = 0; dir->offset = 0;
        }
        FILE_ID_BOTH_DIR_INFO *item = (FILE_ID_BOTH_DIR_INFO *)(dir->entries + dir->offset);
        DWORD bytes = item->FileNameLength, next = item->NextEntryOffset;
        if (!bytes || bytes % sizeof(WCHAR) || bytes > sizeof(dir->entries) - dir->offset - offsetof(FILE_ID_BOTH_DIR_INFO, FileName)) { plum_tree_error("malformed directory entry name"); return -1; }
        if (next) {
            if (next % 8 || next < offsetof(FILE_ID_BOTH_DIR_INFO, FileName) + bytes ||
                next > sizeof(dir->entries) - dir->offset - sizeof(FILE_ID_BOTH_DIR_INFO)) { plum_tree_error("malformed directory entry offset"); return -1; }
            dir->offset += next;
        } else dir->refill = 1;
        WCHAR *name = item->FileName;
        if ((bytes == 2 && name[0] == L'.') || (bytes == 4 && name[0] == L'.' && name[1] == L'.')) continue;
        int n = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, name, (int)(bytes / sizeof(WCHAR)),
            dir->name, sizeof(dir->name) - 1, NULL, NULL);
        if (n <= 0) { plum_tree_os_error("decode directory entry name"); return -1; }
        dir->name[n] = 0;
        if (!dir->read_links) {
            // Enumeration metadata is enough for exclusions; resolve selected
            // entries later through the held root, with fresh type checks.
            dir->kind = (item->FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) ? 3 :
                (item->FileAttributes & FILE_ATTRIBUTE_DIRECTORY) ? 2 : 1;
            return dir->kind;
        }
        HANDLE child = plum_tree_win_child(dir->entry, name, bytes, FILE_READ_ATTRIBUTES, FILE_OPEN, 0, NULL);
        if (child == INVALID_HANDLE_VALUE) return -1;
        FILE_ATTRIBUTE_TAG_INFO attrs;
        if (plum_tree_win_attrs(child, &attrs)) { CloseHandle(child); return -1; }
        if (attrs.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) {
            WCHAR target[PLUM_PATH_MAX];
            if (dir->read_links && (plum_tree_win_link(child, target, PLUM_PATH_MAX, 1) ||
                !WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, target, -1, dir->link, sizeof(dir->link), NULL, NULL))) {
                if (!*plum_tree_error_text) plum_tree_os_error("decode symbolic link target");
                CloseHandle(child); return -1;
            }
            dir->kind = 3;
        } else dir->kind = (attrs.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) ? 2 : 1;
        if (!CloseHandle(child)) return -1;
#else
        errno = 0;
        struct dirent *item = readdir(dir->stream);
        if (!item) { if (!errno) return 0; plum_tree_os_error("enumerate directory"); return -1; }
        if (!strcmp(item->d_name, ".") || !strcmp(item->d_name, "..")) continue;
        size_t n = strlen(item->d_name);
        if (n >= sizeof(dir->name)) { plum_tree_error("directory entry name exceeds inventory capacity"); return -1; }
        memcpy(dir->name, item->d_name, n + 1);
        struct stat st;
        int parent = dirfd(dir->stream);
        if (fstatat(parent, dir->name, &st, AT_SYMLINK_NOFOLLOW)) { plum_tree_os_error("inspect enumerated entry"); return -1; }
        dir->kind = S_ISREG(st.st_mode) ? 1 : S_ISDIR(st.st_mode) ? 2 : S_ISLNK(st.st_mode) ? 3 : 4;
        if (dir->kind == 3 && dir->read_links) {
            ssize_t count = readlinkat(parent, dir->name, dir->link, sizeof(dir->link));
            if (count < 0) { plum_tree_os_error("read enumerated symbolic link"); return -1; }
            if ((size_t)count >= sizeof(dir->link)) { plum_tree_error("symbolic link target exceeds inventory capacity"); return -1; }
            dir->link[count] = 0;
            struct stat after;
            if (fstatat(parent, dir->name, &after, AT_SYMLINK_NOFOLLOW)) { plum_tree_os_error("reinspect enumerated link"); return -1; }
            if (!plum_copy_same(&st, &after)) { plum_tree_error("symbolic link identity changed during inventory"); return -1; }
        }
#endif
        return dir->kind;
    }
}
const char *plum_tree_dir_name(long long handle) {
    plum_tree_directory *dir = (plum_tree_directory *)(intptr_t)handle;
    return dir ? dir->name : "";
}
const char *plum_tree_dir_link(long long handle) {
    plum_tree_directory *dir = (plum_tree_directory *)(intptr_t)handle;
    return dir ? dir->link : "";
}
void plum_tree_dir_close(long long handle) {
    plum_tree_directory *dir = (plum_tree_directory *)(intptr_t)handle;
    if (!dir) return;
#if defined(_WIN32)
    CloseHandle(dir->entry);
#else
    closedir(dir->stream);
#endif
    free(dir);
}

long long os_copy_tree(const char *src, const char *dst) {
    plum_tree_error_clear();
    int result = plum_copy_tree(src, dst);
    if (result && !*plum_tree_error_text) plum_tree_os_error("copy tree");
    return result == 0 ? 0 : -1;
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
    const char *kinds[]={"ir","obj","shims","staging"};
    plum_cache_entry *entries=NULL; size_t count=0, capacity=0;
    uint64_t total=0; long long removed=0; time_t now=time(NULL);
    char parent[PLUM_PATH_MAX], path[PLUM_PATH_MAX]; struct stat st;
    for (size_t k=0;k<sizeof(kinds)/sizeof(kinds[0]);k++) {
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
