#!/usr/bin/env python3
"""Measure case conversion using actual compiler builds and byte-checked output."""
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
extern int64_t plum_heap_snapshot(void),plum_heap_snapshot_field(int64_t,int64_t);
static int64_t before[6];
void case_before(void){int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i);}
void case_after(const unsigned char *p,int64_t length,int64_t n,int64_t kind){
 const unsigned char *want=kind%2?(const unsigned char *)"a\xc3\xa9\0\xf0\x9f\x99\x82":(const unsigned char *)"A\xc3\x89\0\xf0\x9f\x99\x82";
 if(length!=8*n || p[length])abort();
 for(int64_t i=0;i<length;i++)if(p[i]!=want[i%8])abort();
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"peak_bytes\":%lld,\"output_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)v[5],(long long)length);
}
'''
SOURCE = r'''extern "C" {fn case_before()->Unit;fn case_after(s:CStr,length:Int,n:Int,kind:Int)->Unit;}
let main():Unit={
 let n=args()[0].parse_int().unwrap_or(0);let kind=args()[1].parse_int().unwrap_or(0);
 let unit=if kind==0 || kind==3 {"AÉ\x00🙂"} else {"aé\x00🙂"};
 let text=unit.repeat(n);
 unsafe {case_before();};
 let out=if kind==0 || kind==2 {text.to_upper()} else {text.to_lower()};
 unsafe {case_after(out.as_cstr(),out.len(),n,kind);};
 assert_eq(text.len(),unit.len()*n)
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
    with tempfile.TemporaryDirectory(prefix='plum-case-measure-') as temp:
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
                    for kind in range(4):
                        result = subprocess.run([str(binary), str(n), str(kind)], env=env,
                                                check=True, capture_output=True, text=True,
                                                encoding='utf-8', timeout=60)
                        row = dict(compiler=label, mode=mode, n=n, kind=kind, **json.loads(result.stdout))
                        rows.append(row)
                        print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Ordinary builds; allocation-free C observer verifies every byte, length and NUL. Input stays live. Traffic/live deltas exclude setup; peak covers the whole process. Requested heap bytes, not RSS or stack. No timing claim.',
        cases={'0':'unchanged upper', '1':'unchanged lower', '2':'changed upper', '3':'changed lower'},
        results=rows), indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
