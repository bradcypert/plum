#!/usr/bin/env python3
"""Measure actual Encoding.base64_decode programs, without rewriting emitted LLVM."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

OBSERVER = r'''

#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
extern int64_t plum_heap_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t,int64_t);
static int64_t before[6];
void decode_before(void){int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i);}
void decode_after(const unsigned char *p,int64_t length,int64_t n,int64_t kind,bool ok){
 if(ok!=(kind<5))abort();
 if(ok){
  int tail=kind==0?0:kind<3?1:2;
  if(length!=3*n+tail || p[length])abort();
  for(int64_t i=0;i<3*n;i++)if(p[i]!=(unsigned char)"abc"[i%3])abort();
  if(tail && p[3*n]!=251)abort();if(tail==2 && p[3*n+1]!=255)abort();
 } else {
  char error[128];
  if(kind==5)snprintf(error,sizeof(error),"Encoding.base64_decode: invalid character at byte %lld",(long long)(n*4));
  else strcpy(error,kind==6?"Encoding.base64_decode: nonzero pad bits":"Encoding.base64_decode: truncated input");
  if(length!=(int64_t)strlen(error)||strcmp((const char*)p,error))abort();
 }
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"peak_bytes\":%lld,\"payload_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)v[5],(long long)length);
}
'''

SOURCE = r'''use Encoding;
let probe_decode(s:String):Result[Bytes,String]=Encoding.base64_decode(s)
extern "C" {fn decode_before()->Unit;fn decode_after(s:CStr,length:Int,n:Int,kind:Int,ok:Bool)->Unit;}
let main():Unit={
 let n=args()[0].parse_int().unwrap_or(0);let kind=args()[1].parse_int().unwrap_or(0);
 let suffix=if kind==1 {"+w=="} else if kind==2 {"+w"} else if kind==3 {"+/8="} else if kind==4 {"-_8"} else if kind==5 {"!"} else if kind==6 {"AB"} else if kind==7 {"A"} else {""};
 let text="YWJj".repeat(n).concat(suffix);
 unsafe {decode_before();};
 match probe_decode(text) {
  Ok(out)=>unsafe {decode_after(out.as_cstr(),out.len(),n,kind,true);},
  Err(e)=>unsafe {decode_after(e.as_cstr(),e.len(),n,kind,false);},
 };
 assert_eq(text.len(),n*4+suffix.len())
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
    cases = [(n, kind) for n in [0, 1, 512, 4096, 32768] for kind in range(8)]
    with tempfile.TemporaryDirectory(prefix='plum-base64-decode-measure-') as temp:
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
        method='Actual unmodified compiler builds; allocation-free native observer verifies all decoded bytes or the exact diagnostic, length and NUL. Public call traffic includes Result bookkeeping; snapshot is taken inside the matched arm. Traffic/live deltas exclude input/setup; Peak is whole-process. No timing/RSS inference.',
        pattern='YWJj repeated n times; kind 0 none, 1 +w==, 2 +w, 3 +/8=, 4 -_8, 5 !, 6 AB, 7 A',
        results=rows), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
