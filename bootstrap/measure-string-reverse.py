#!/usr/bin/env python3
"""Measure actual String.reverse results without replacing emitted LLVM."""
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
extern int64_t plum_heap_snapshot(void),plum_heap_snapshot_field(int64_t,int64_t);
static int64_t before[6];
void reverse_before(void){int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i);}
void reverse_after(const unsigned char *s,int64_t n,const unsigned char *out,int64_t m){
 if(n!=m || out[m])abort();
 for(int64_t start=0;start<n;){
  int64_t end=start+1;
  while(end<n && (s[end]&0xC0)==0x80)end++;
  if(memcmp(s+start,out+n-end,(size_t)(end-start)))abort();
  start=end;
 }
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"peak_bytes\":%lld,\"output_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)v[5],(long long)m);
}
'''
SOURCE = r'''extern "C" {fn reverse_before()->Unit;fn reverse_after(s:CStr,n:Int,out:CStr,m:Int)->Unit;}
let main():Unit={
 let n=args()[0].parse_int().unwrap_or(0);let kind=args()[1].parse_int().unwrap_or(0);
 let unit=if kind==0 {"abcd"} else if kind==1 {"aé\x00🙂"} else if kind==2 {"é"} else {"a"};
 let text=if kind==4 {if n==0 {""} else {String.slice("🙂x",0,1)}} else {unit.repeat(n)};
 unsafe {reverse_before();};
 let out=String.reverse(text);
 unsafe {reverse_after(text.as_cstr(),text.len(),out.as_cstr(),out.len());};
 assert_eq(out.len(),text.len());assert_eq(text,if kind==4 {if n==0 {""} else {"🙂"}} else {unit.repeat(n)})
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before')
    parser.add_argument('after')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_CC', None)
    rows = []
    with tempfile.TemporaryDirectory(prefix='plum-reverse-measure-') as temp:
        work = Path(temp)
        project = work / 'project'
        project.mkdir()
        (project / 'main.plum').write_text(SOURCE, encoding='utf-8')
        observer = work / 'observer.c'
        observer.write_text(OBSERVER, encoding='utf-8')
        for label, compiler in [('before', args.before), ('after', args.after)]:
            compiler = str(Path(compiler).resolve())
            for mode, flags in [('debug', []), ('release', ['--release'])]:
                binary = work / 'app.exe'
                built = subprocess.run([compiler, 'build', project, '--memory-stats', *flags,
                                        '--link-c', observer, '-o', binary], cwd=repo, env=env,
                                       capture_output=True, text=True, encoding='utf-8', timeout=300)
                if built.returncode:
                    raise RuntimeError(built.stdout + built.stderr)
                for n in [0, 1, 512, 4096, 32768]:
                    for kind in range(5):
                        result = subprocess.run([str(binary), str(n), str(kind)], env=env,
                                                check=True, capture_output=True, text=True,
                                                encoding='utf-8', timeout=60)
                        row = dict(compiler=label, mode=mode, n=n, kind=kind, **json.loads(result.stdout))
                        rows.append(row)
                        print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Ordinary builds; allocation-free C observer verifies every reversed byte group, output length and NUL. Input stays live. Traffic/live deltas exclude setup; peak covers the whole process. Requested heap bytes, not RSS or throughput.',
        cases={'0':'ASCII', '1':'Unicode/NUL', '2':'combining marks (codepoints, not graphemes)', '3':'repeated identical codepoints', '4':'single heap-allocated supplementary codepoint (n>0)'},
        results=rows), indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
