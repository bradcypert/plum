#!/usr/bin/env python3
"""Measure actual Bytes.to_array allocation traffic, live bytes and peak heap."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

OBSERVER = r'''
#include <stdint.h>
#include <stdio.h>
#include "memory_shim.h"
extern int64_t plum_heap_snapshot(void),plum_heap_snapshot_field(int64_t,int64_t);
extern int64_t plum_heap_origin_snapshot(void),plum_heap_origin_field(int64_t,int64_t,int64_t);
static int64_t before[6],array_live;
void barray_before(void){int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i);array_live=plum_heap_origin_field(plum_heap_origin_snapshot(),PLUM_ORIGIN_ARRAY,3);}
void barray_after(int64_t length){
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 int64_t arrays=plum_heap_origin_field(plum_heap_origin_snapshot(),PLUM_ORIGIN_ARRAY,3)-array_live;
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"array_live_bytes\":%lld,\"peak_bytes\":%lld,\"output_elements\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)arrays,(long long)v[5],(long long)length);
}
'''
SOURCE = r'''extern "C" {fn barray_before()->Unit;fn barray_after(length:Int)->Unit;}
let expected(kind:Int)(i:Int):Int=
 if kind==1 {97+i%4} else if kind==2 {0} else {
  let k=i%8;
  if k==0 {97} else if k==1 {195} else if k==2 {169} else if k==3 {0}
  else if k==4 {240} else if k==5 {159} else if k==6 {153} else {130}
 }
let main():Unit={
 let n=args()[0].parse_int().unwrap_or(0);let kind=args()[1].parse_int().unwrap_or(0);
 let unit=if kind==0 {"aé\x00🙂"} else if kind==1 {"abcd"} else {"\x00"};
 let input=unit.repeat(n).as_bytes();
 unsafe {barray_before();};
 let out=input.to_array();
 unsafe {barray_after(out.len());};
 // Verify after observing: no verification allocations enter the measurement.
 assert_eq(out.len(),unit.len()*n);
 for i in 0..out.len() {assert_eq(out[i],expected(kind,i));};
 assert_eq(input.len(),unit.len()*n)
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
    with tempfile.TemporaryDirectory(prefix='plum-barray-measure-') as temp:
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
                    for kind in range(3):
                        result = subprocess.run([str(binary), str(n), str(kind)], env=env,
                                                check=True, capture_output=True, text=True,
                                                encoding='utf-8', timeout=60)
                        row = dict(compiler=label, mode=mode, n=n, kind=kind, **json.loads(result.stdout))
                        rows.append(row)
                        print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Ordinary builds; allocation-free native observer snapshots before verification. Every result element is checked against an independent unsigned numeric pattern after observation. Input and result remain live. Traffic/live deltas exclude setup; peak covers the whole process. Array origin is recorded separately. Requested heap bytes, not RSS or throughput.',
        cases={'0':'Unicode/NUL bytes', '1':'ASCII bytes', '2':'repeated zero bytes'},
        results=rows), indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
