#!/usr/bin/env python3
"""Measure real padding programs, with optional isolated ownership prototypes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

SOURCE = r'''extern "C" {
 fn pad_setup()->Unit; fn pad_before(s:CStr,n:Int,kind:Int)->Unit;
 fn pad_after(s:CStr,n:Int,width:Int,mode:Int,kind:Int,shared:Int)->Unit;
}
let growing():String={let mut s="";for i in 0..4096{s=s.concat("x");};s}
let pad(s:String)(w:Int)(fill:String)(mode:Int):String=
 if mode==0 {String.pad_left(s,w,fill)} else if mode==1 {String.pad_right(s,w,fill)} else {String.pad_center(s,w,fill)}
let main():Unit={
 let a=args();let w=a[0].parse_int().unwrap_or(0);let mode=a[1].parse_int().unwrap_or(0);
 let kind=a[2].parse_int().unwrap_or(0);let shared=a[3]=="shared";
 assert_eq(w.to_string(),a[0]);assert_eq(mode.to_string(),a[1]);assert_eq(kind.to_string(),a[2]);
 unsafe{pad_setup();};
 let input=if kind==0 {"data"} else if kind==1 {"data".repeat(1024)} else if kind==2 {growing()} else {"aé\x00🙂".repeat(1024)};
 let fill=if kind==3 {"日"} else {"."};
 unsafe{pad_before(input.as_cstr(),input.len(),kind);};
 if shared {
  let out=pad(input,w,fill,mode);
  unsafe{pad_after(out.as_cstr(),out.len(),w,mode,kind,1);};
  assert_eq(input.len(),if kind==0 {4} else if kind==3 {8192} else {4096});assert_eq(out.len()>0,true);
 } else {
  let out=pad(input,w,fill,mode);
  unsafe{pad_after(out.as_cstr(),out.len(),w,mode,kind,0);};assert_eq(out.len()>0,true);
 };
 // Keep argument storage alive throughout every snapshot.
 assert_eq(a.len(),4);
}
'''

OBSERVER = r'''#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
extern int64_t plum_heap_snapshot(void),plum_heap_snapshot_field(int64_t,int64_t);
static int64_t setup[6],before[6],source_request;static uintptr_t source_id;
static void capture(int64_t*v){int64_t t=plum_heap_snapshot();if(!t)abort();for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);}
static const unsigned char ascii[]={100,97,116,97},unicode[]={97,0xc3,0xa9,0,0xf0,0x9f,0x99,0x82},fill_unicode[]={0xe6,0x97,0xa5};
static unsigned char source_byte(int64_t i,int64_t kind){return kind==2?'x':kind==3?unicode[i%8]:ascii[i%4];}
void pad_setup(void){capture(setup);}
void pad_before(const unsigned char*s,int64_t n,int64_t kind){
 int64_t want=kind==0?4:kind==3?8192:4096;if(n!=want||s[n])abort();
 for(int64_t i=0;i<n;i++)if(s[i]!=source_byte(i,kind))abort();
 capture(before);source_request=before[3]-setup[3];source_id=(uintptr_t)s;
 if(source_request<0 || (kind==0?source_request!=0:source_request<=0))abort();
}
void pad_after(const unsigned char*s,int64_t n,int64_t width,int64_t mode,int64_t kind,int64_t shared){
 int64_t chars=kind==0?4:4096,bytes=kind==3?8192:chars,f=kind==3?3:1;
 int64_t count=width>chars?width-chars:0,left=mode==0?count:mode==1?0:count/2,lb=left*f;
 if(n!=bytes+count*f||s[n])abort();
 for(int64_t i=0;i<n;i++){
  unsigned char want=i>=lb&&i<lb+bytes?source_byte(i-lb,kind):kind==3?fill_unicode[(i<lb?i:i-lb-bytes)%3]:'.';
  if(s[i]!=want)abort();
 }
 int64_t v[6];capture(v);int same=(uintptr_t)s==source_id;
 int64_t result=same?source_request:v[3]-setup[3]-(shared?source_request:0);if(result<0)abort();
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"live_delta\":%lld,\"result_requested_bytes\":%lld,\"input_requested_bytes\":%lld,\"same_storage\":%s,\"peak_bytes\":%lld,\"before_peak_bytes\":%lld,\"output_bytes\":%lld}\n",
 (long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)result,(long long)source_request,same?"true":"false",(long long)v[5],(long long)before[5],(long long)n);
}
'''

PROTOTYPE = r'''
/* Test-only alternatives replace public padding entries, whose parameters are
   owned. This C model owns both s and fill, unlike the production kernel's
   consuming-s/borrowed-fill boundary. The byte observer remains independent. */
extern void *plum_alloc_preferred(int64_t,int64_t,int64_t);
extern void plum_rel_str(void*);
extern size_t malloc_usable_size(void*);
static int64_t units(const unsigned char*s,int64_t n){int64_t v=n>0;for(int64_t i=1;i<n;i++)if((s[i]&192)!=128)v++;return v;}
void *pad_prototype(void*s,int64_t width,void*fill,int64_t mode,int64_t reuse){
 int64_t sl=((int64_t*)s)[1],fl=((int64_t*)fill)[1],sc=units((unsigned char*)s+16,sl);
 if(width<=sc||units((unsigned char*)fill+16,fl)!=1){plum_rel_str(fill);return s;}
 int64_t count=width-sc,left=mode==0?count:mode==1?0:count/2;
 if(fl>(INT64_MAX-17-sl)/count)abort();int64_t length=sl+count*fl,size=length+17;
 int same=reuse&&((int64_t*)s)[0]==1&&s!=fill&&malloc_usable_size(s)>=(uint64_t)size;
 void*out=same?s:plum_alloc_preferred(size,size,1);char*d=(char*)out+16,*f=(char*)fill+16;
 memmove(d+left*fl,(char*)s+16,(size_t)sl);
 for(int64_t i=0;i<left;i++)memcpy(d+i*fl,f,(size_t)fl);
 for(int64_t i=0;i<count-left;i++)memcpy(d+left*fl+sl+i*fl,f,(size_t)fl);
 ((int64_t*)out)[1]=length;d[length]=0;if(!same)plum_rel_str(s);plum_rel_str(fill);return out;
}
'''

WRAPPER = r'''#!/usr/bin/env python3
import os,re,subprocess,sys
from pathlib import Path
args=sys.argv[1:]
for a in args:
 if a.endswith('.ll'):
  p=Path(a);s=p.read_text(encoding='utf-8');s+='\ndeclare ptr @pad_prototype(ptr,i64,ptr,i64,i64)\n'
  for mode,name in enumerate(['pad_left','pad_right','pad_center']):
   m=re.search(r'^define ptr (@[^\n]*String\$2e'+name+r'[^\n]*\([^\n]*\))[^\n]*\{.*?^}',s,re.M|re.S)
   if not m:raise RuntimeError('missing prototype entry: '+name)
   body='define ptr '+m[1]+' {\nentry:\n  %out = call ptr @pad_prototype(ptr %p0, i64 %p1, ptr %p2, i64 '+str(mode)+', i64 '+os.environ['PAD_PROTOTYPE_REUSE']+')\n  ret ptr %out\n}'
   s=s[:m.start()]+body+s[m.end():]
  p.write_text(s,encoding='utf-8')
raise SystemExit(subprocess.call(['clang',*args]))
'''


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('before')
    ap.add_argument('after')
    ap.add_argument('--out', required=True)
    ap.add_argument('--compare-designs', action='store_true')
    args = ap.parse_args()
    if args.compare_designs and os.name == 'nt':
        ap.error('optional prototype comparison requires the Unix CC wrapper')
    root = Path(__file__).resolve().parent.parent
    binaries = {'before': str(Path(args.before).resolve()), 'after': str(Path(args.after).resolve())}
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_CC', None)
    rows = []

    def run(command, extra=None):
        r = subprocess.run([str(x) for x in command], cwd=root, env=env | (extra or {}),
                           capture_output=True, text=True, encoding='utf-8', timeout=300)
        if r.returncode:
            raise RuntimeError(f'{command}: exit {r.returncode}\n{r.stdout}\n{r.stderr}')
        return r.stdout

    with tempfile.TemporaryDirectory(prefix='plum-pad-measure-') as directory:
        work = Path(directory)
        (work / 'native').mkdir()
        (work / 'main.plum').write_text(SOURCE, encoding='utf-8')
        wrapper = work / 'cc.py'
        wrapper.write_text(WRAPPER, encoding='utf-8')
        wrapper.chmod(0o755)
        designs = ['before', 'after'] + (['copy-prototype', 'reuse-prototype'] if args.compare_designs else [])
        for label in designs:
            prototype = label.endswith('prototype')
            (work / 'native/probe.c').write_text(OBSERVER + (PROTOTYPE if prototype else ''), encoding='utf-8')
            extra = {'PLUM_CC': str(wrapper), 'PAD_PROTOTYPE_REUSE': str(int(label == 'reuse-prototype'))} if prototype else {}
            for release in (False, True):
                if prototype and not release:
                    continue
                binary = work / (label + '-' + str(release))
                run([binaries['before' if prototype else label], 'build', work, '--memory-stats',
                     *(['--release'] if release else []), '-o', binary], extra)
                for kind in range(4):
                    widths = [0, 4, 5, 8, 1024, 262144] if kind == 0 else [4096, 4097, 5120, 262144]
                    if prototype:
                        widths = [[262144], [4097], [5120], [4097]][kind]
                    for width in widths:
                        for mode in range(3):
                            for shared in (False, True):
                                result = json.loads(run([binary, width, mode, kind, 'shared' if shared else 'unique']))
                                rows.append(dict(result, design=label, release=release, kind=kind, width=width,
                                                 alignment=['left', 'right', 'center'][mode], shared=shared))
                print(f'padding: {label} release={release} verified', flush=True)
    digest = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
    metadata = {'binaries': {label: {'path': path, 'sha256': digest(path)} for label, path in binaries.items()},
                'harness_sha256': digest(__file__)}
    method = 'Ordinary public API programs for before/after; optional C/LLVM prototypes explicitly replace only public padding entries using the before compiler. No production measurements substitute LLVM. Independent C observer verifies all bytes, lengths and NUL, including multibyte/NUL content; snapshots preserve argument lifetimes. Requested traffic/live exclude setup; peak is whole process including setup. Input uses literals, exact repetition or ordinary Perceus-growing concatenation. Result requests preserve original allocation provenance on reuse; not RSS or speed.'
    Path(args.out).write_text(json.dumps({'method': method, 'metadata': metadata, 'records': rows}, indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    main()
