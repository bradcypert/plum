#!/usr/bin/env python3
"""Measure actual Encoding.base64_encode programs, without rewriting emitted LLVM."""
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
extern int64_t plum_heap_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t,int64_t);
static int64_t before[6];
void base64_before(void){int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i);}
void base64_after(const char *p,int64_t length,int64_t n,int64_t tail,bool url){
 const char *suffix=tail==0?"":tail==1?(url?"-w":"+w=="):(url?"-_8":"+/8=");
 int64_t width=tail==0?0:url?tail+1:4;
 if(length!=4*n+width || p[length])abort();
 for(int64_t i=0;i<4*n;i++)if(p[i]!="YWJj"[i%4])abort();
 for(int64_t i=0;i<width;i++)if(p[4*n+i]!=suffix[i])abort();
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"live_allocations\":%lld,\"peak_bytes\":%lld,\"logical_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)(v[2]-before[2]),(long long)v[5],(long long)length);
}
'''

SOURCE = r'''use Encoding;
let probe_base64 (b:Bytes) (url:Bool):String=if url { Encoding.base64_url_encode(b) } else { Encoding.base64_encode(b) }
extern "C" {fn base64_before()->Unit;fn base64_after(s:CStr,length:Int,n:Int,tail:Int,url:Bool)->Unit;}
let main ():Unit={
 let n=args()[0].parse_int().unwrap_or(0); let tail=args()[1].parse_int().unwrap_or(0); let url=args()[2]=="1";
 let input="abc".repeat(n).as_bytes().concat(Bytes.from_array(if tail==1 {[251]} else if tail==2 {[251,255]} else {[]}));
 unsafe {base64_before();};let out=probe_base64(input,url);
 unsafe {base64_after(out.as_cstr(),out.len(),n,tail,url);};assert_eq(input.len(),n*3+tail)
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
    cases = [(n, tail, url) for n in [0, 1, 512, 4096, 32768] for tail in [0, 1, 2] for url in [0, 1]]
    with tempfile.TemporaryDirectory(prefix='plum-base64-measure-') as temp:
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
                for n, tail, url in cases:
                    result = subprocess.run([str(binary), str(n), str(tail), str(url)], env=env,
                                            check=True, capture_output=True, text=True, timeout=30)
                    row = dict(compiler=label, mode=mode, n=n, tail=tail, url=url, **json.loads(result.stdout))
                    rows.append(row)
                    print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Actual unmodified compiler builds; allocation-free native observer verifies all bytes, length and NUL. Traffic/live deltas exclude input/setup; Peak is whole-process. No timing/RSS inference.',
        pattern='abc repeated n times, followed by zero/one/two bytes from FB FF; both alphabet modes',
        results=rows), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
