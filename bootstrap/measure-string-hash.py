#!/usr/bin/env python3
"""Compare named hashing, a direct-byte prototype, and the existing method path."""
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
extern int64_t plum_heap_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t,int64_t);
static int64_t before[6];
int64_t hash_before(const unsigned char *s,int64_t length){
 uint64_t h=UINT64_C(14695981039346656037);
 for(int64_t i=0;i<length;i++)h=(h^s[i])*UINT64_C(1099511628211);
 int64_t token=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(token,i);
 return (int64_t)(h&UINT64_C(9223372036854775807));
}
void hash_after(void){
 int64_t token=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(token,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"peak_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)v[5]);
}
'''
SOURCE = r'''let probe_hash(s:String):Int=String.hash(s)
extern "C" {fn hash_before(s:CStr,length:Int)->Int;fn hash_after()->Unit;}
let main():Unit={
 let n=args()[0].parse_int().unwrap_or(0);let kind=args()[1].parse_int().unwrap_or(0);
 let reps=args()[2].parse_int().unwrap_or(1);
 let unit=if kind==0 {"a"} else if kind==1 {"aé\x00🙂"} else {"abc\n\t\r"};
 let text=unit.repeat(n);
 let expected=unsafe {hash_before(text.as_cstr(),text.len())};
 for i in 0..reps {assert_eq(probe_hash(text),expected);};
 unsafe {hash_after();};
 assert_eq(text.len(),n*unit.len())
}
'''
RAW_ALTERNATIVE = r'''let bytes_at_raw(b:Bytes)(i:Int):Int=0
let raw_scan(b:Bytes)(i:Int)(n:Int)(h:Int):Int=
 if i>=n {h&9223372036854775807}
 else {raw_scan(b,i+1,n,Int.wrapping_mul(h^bytes_at_raw(b,i),1099511628211))}
let raw_hash(s:String):Int={let b=s.as_bytes();raw_scan(b,0,b.len(),0-3750763034362895579)}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before')
    parser.add_argument('after')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    before, after = str(Path(args.before).resolve()), str(Path(args.after).resolve())
    cases = [(n, kind, 1000 if n < 2 else 10 if n < 65536 else 1)
             for n in [0, 1, 512, 4096, 65536] for kind in range(3)]
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_CC', None)
    rows = []
    with tempfile.TemporaryDirectory(prefix='plum-string-hash-measure-') as temp:
        work = Path(temp)
        project = work / 'project'
        project.mkdir()
        observer = work / 'observer.c'
        observer.write_text(OBSERVER)
        for label, compiler in [('before', before), ('raw', before), ('method', before), ('after', after)]:
            source = SOURCE
            if label == 'raw':
                source = source.replace('String.hash(s)', 'raw_hash(s)') + RAW_ALTERNATIVE
            elif label == 'method':
                source = source.replace('String.hash(s)', 's.hash()')
            (project / 'main.plum').write_text(source)
            for mode, flags in [('debug', []), ('release', ['--release'])]:
                binary = work / 'app.exe'
                result = subprocess.run([compiler, 'build', project, '--memory-stats', *flags,
                                         '--link-c', observer, '-o', binary], cwd=repo, env=env,
                                        capture_output=True, text=True, timeout=300)
                if result.returncode:
                    raise RuntimeError(result.stdout + result.stderr)
                for n, kind, reps in cases:
                    result = subprocess.run([str(binary), str(n), str(kind), str(reps)], env=env,
                                            check=True, capture_output=True, text=True, timeout=60)
                    width = [1, 8, 6][kind]
                    row = dict(compiler=label, mode=mode, n=n, kind=kind, repetitions=reps,
                               input_bytes=width*n, **json.loads(result.stdout))
                    rows.append(row)
                    print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Ordinary compiler builds. Before/after call String.hash; raw is a source-level direct-byte FNV prototype built by before; method calls the already allocation-free s.hash() on before. Native independent uint64 FNV verifies every returned hash, including NUL, before recording. Input is kept live; traffic/live deltas exclude setup and whole-process peak includes setup. No timing/RSS inference.',
        patterns={'0':'ASCII a', '1':'a + é + NUL + 🙂', '2':'abc + newline + tab + carriage return'},
        results=rows), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
