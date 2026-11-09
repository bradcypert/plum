#!/usr/bin/env python3
"""Measure actual Array.join programs, without rewriting emitted LLVM."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

OBSERVER = r'''

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
extern int64_t plum_heap_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t,int64_t);
static int64_t before[6];
void join_before(void) { int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i); }
void join_after(const unsigned char *s,int64_t length,int64_t n,int64_t kind) {
 const unsigned char a[]={97},b[]={0xc3,0xa9},c[]={0},d[]={0xf0,0x9f,0x99,0x82},sep[]={60,0,62};
 const unsigned char *items[]={a,b,c,d,a};const int sizes[]={1,2,1,4,0};
 int64_t off=0;
 for(int64_t i=0;i<n;i++) {
  if(kind && i) { if(off+3>length || memcmp(s+off,sep,3))abort();off+=3; }
  int size=sizes[i%5];if(off+size>length || memcmp(s+off,items[i%5],size))abort();off+=size;
 }
 if(length!=off || s[length])abort();
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"result_request\":%lld,\"live_allocations\":%lld,\"peak_bytes\":%lld,\"logical_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)(v[2]-before[2]),(long long)v[5],(long long)length);
}
'''

SOURCE = r'''let probe_join (parts: Array[String]) (sep: String): String = parts.join(sep)
extern "C" { fn join_before() -> Unit; fn join_after(s:CStr,length:Int,n:Int,kind:Int) -> Unit; }
let main (): Unit = {
 let n=args()[0].parse_int().unwrap_or(0);
 let kind=args()[1].parse_int().unwrap_or(0);
 let pattern=["a","é","\x00","🙂",""];
 let mut parts=[];
 for i in 0..n { parts=parts.push(pattern[i%5]); };
 let sep=if kind==0 { "" } else { "<\x00>" };
 unsafe { join_before(); };
 let out=probe_join(parts,sep);
 unsafe { join_after(out.as_cstr(),out.len(),n,kind); };
 assert_eq(parts.len(),n)
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before')
    parser.add_argument('after')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    compilers = [('before', str(Path(args.before).resolve())),
                 ('after', str(Path(args.after).resolve()))]
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_CC', None)
    rows = []
    cases = [(n, kind) for n in [0, 1, 8, 64, 512, 4096, 32768] for kind in [0, 1]]
    with tempfile.TemporaryDirectory(prefix='plum-join-measure-') as temp:
        work = Path(temp)
        project = work / 'project'
        project.mkdir()
        (project / 'main.plum').write_text(SOURCE, encoding='utf-8')
        observer = work / 'observer.c'
        observer.write_text(OBSERVER)
        for label, compiler in compilers:
            for mode, flags in [('debug', []), ('release', ['--release'])]:
                binary = work / 'app.exe'
                built = subprocess.run([compiler, 'build', str(project), '--memory-stats',
                                        *flags, '--link-c', str(observer), '-o', str(binary)],
                                       cwd=repo, env=env, capture_output=True, text=True, timeout=300)
                if built.returncode:
                    raise RuntimeError(built.stdout + built.stderr)
                for n, kind in cases:
                    result = subprocess.run([str(binary), str(n), str(kind)], env=env,
                                            check=True, capture_output=True, text=True, timeout=30)
                    row = dict(compiler=label, mode=mode, n=n, kind=kind, **json.loads(result.stdout))
                    rows.append(row)
                    print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Actual unmodified compiler builds; allocation-free native observer verifies all bytes, length and NUL. Traffic/live deltas exclude input/setup; peak is whole-process. No timing/RSS inference.',
        pattern='n strings cycling a, é, NUL, 🙂, empty; kind=0 empty separator, kind=1 <NUL>',
        results=rows), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
