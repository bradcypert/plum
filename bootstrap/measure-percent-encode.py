#!/usr/bin/env python3
"""Measure actual Encoding.percent_encode programs, without rewriting emitted LLVM."""
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
void percent_before(void) { int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i); }
void percent_after(const char *s,int64_t length,int64_t n,int64_t kind) {
 const char *patterns[]={"azAZ09-._~","a%C3%A9%00%F0%9F%99%82","%2B%20%2F%25%3F%23"};
 const int sizes[]={10,22,18};
 int size=sizes[kind];if(length!=size*n || s[length])abort();
 for(int64_t i=0;i<length;i++)if(s[i]!=patterns[kind][i%size])abort();
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"live_allocations\":%lld,\"peak_bytes\":%lld,\"logical_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)(v[2]-before[2]),(long long)v[5],(long long)length);
}
'''

SOURCE = r'''use Encoding;
let probe_percent (b:Bytes):String=Encoding.percent_encode(b)
extern "C" { fn percent_before() -> Unit; fn percent_after(s:CStr,length:Int,n:Int,kind:Int) -> Unit; }
let main ():Unit={
 let n=args()[0].parse_int().unwrap_or(0);
 let kind=args()[1].parse_int().unwrap_or(0);
 let unit=if kind==0 { "azAZ09-._~" } else if kind==1 { "aé\x00🙂" } else { "+ /%?#" };
 let input=unit.repeat(n).as_bytes();
 unsafe { percent_before(); };
 let out=probe_percent(input);
 unsafe { percent_after(out.as_cstr(),out.len(),n,kind); };
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
    compilers = [('before', str(Path(args.before).resolve())),
                 ('after', str(Path(args.after).resolve()))]
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_CC', None)
    rows = []
    cases = [(n, kind) for n in [0, 1, 64, 512, 4096, 32768] for kind in [0, 1, 2]]
    with tempfile.TemporaryDirectory(prefix='plum-percent-measure-') as temp:
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
        method='Actual unmodified compiler builds; allocation-free native observer verifies all bytes, length and NUL. Traffic/live deltas exclude input/setup; identity output shares input already live in the before snapshot, so zero additional live bytes does not mean a zero-size owner. Peak is whole-process. No timing/RSS inference.',
        patterns={'0':'unreserved azAZ09-._~', '1':'a + é + NUL + 🙂', '2':'+ /%?#'},
        results=rows), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
