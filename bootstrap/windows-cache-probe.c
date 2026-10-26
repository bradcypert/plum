// Exercise the actual Windows loader adapter independently of Plum/cache keys.
#include <stdio.h>
#define PLUM_WIN_IMAGE_TRACE(path, digest) fprintf(stderr,"loader image: %ls %s\n",path,digest)
#include "../native_stdlib/os_shim.c"

int main(int argc, char **argv) {
#if defined(_WIN64)
    if (argc<2) return 2;
    HANDLE file=CreateFileA(argv[1],GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_DELETE,NULL,OPEN_EXISTING,0,NULL);
    if (file!=INVALID_HANDLE_VALUE) {
        plum_win_sha provider; plum_win_sha_open(&provider);
        char fast[65], portable[65];
        int equal=plum_win_hash(file,&provider,fast) && plum_win_hash(file,NULL,portable)
            && strcmp(fast,portable)==0;
        CloseHandle(file); plum_win_sha_close(&provider);
        if (!equal) { fprintf(stderr,"Windows SHA providers disagree\n"); return 2; }
    }
    char first[160];
    snprintf(first,sizeof(first),"%s",plum_cache_windows_fingerprint(argv[1]));
    if (argc>2) {
        DWORD before=0, after=0;
        if (!GetProcessHandleCount(GetCurrentProcess(),&before)) return 2;
        for (int i=0;i<5;i++) {
            const char *next=plum_cache_windows_fingerprint(argv[1]);
            if (strcmp(first,next)!=0) {
                fprintf(stderr,"loader fingerprint was not stable: %s -> %s\n",first,next); return 2;
            }
        }
        if (!GetProcessHandleCount(GetCurrentProcess(),&after) || after>before+2) {
            fprintf(stderr,"loader probe leaked handles: %lu -> %lu\n",before,after); return 2;
        }
    }
    puts(first);
    return first[0]=='!' ? 1 : 0;
#else
    (void)argc; (void)argv;
    puts("Windows loader probe: not applicable");
    return 0;
#endif
}
