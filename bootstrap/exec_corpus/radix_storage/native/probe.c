#include <stdint.h>
#include <stdlib.h>
#include <string.h>
void radix_verify(const char *s,int64_t length,int64_t n,int64_t radix,int64_t kind) {
    const char *digits="0123456789abcdefghijklmnopqrstuvwxyz";char out[66];int at=66;
    if(kind) {
        unsigned width=kind==1?16:64,shift=kind==1?4:1,mask=kind==1?15:1;
        for(unsigned i=0;i<width;i++)out[--at]=digits[((uint64_t)n>>(i*shift))&mask];
    } else {
        uint64_t r=radix<2?2:radix>36?36:(uint64_t)radix;
        uint64_t v=n<0?(uint64_t)(-(n+1))+1:(uint64_t)n;
        do{out[--at]=digits[v%r];v/=r;}while(v);
        if(n<0)out[--at]='-';
    }
    if(length!=66-at || memcmp(s,out+at,(size_t)length) || s[length])abort();
}
