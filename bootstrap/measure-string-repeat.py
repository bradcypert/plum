#!/usr/bin/env python3
"""Measure actual String.repeat programs, without rewriting emitted LLVM."""
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
void repeat_before(void) { int64_t t=plum_heap_snapshot(); for(int i=0;i<6;i++) before[i]=plum_heap_snapshot_field(t,i); }
void repeat_after(const unsigned char *p,int64_t length,int64_t n,int64_t kind) {
 const unsigned char a[]={97,98,99},b[]={97,0xc3,0xa9,0,0xf0,0x9f,0x99,0x82};
 int64_t unit=kind==0?3:kind==1?8:0, expected=n>0?n*unit:0;
 if(length!=expected || p[length]) abort();
 for(int64_t i=0;i<length;i++) if(p[i]!=(kind==0?a[i%3]:b[i%8])) abort();
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"live_allocations\":%lld,\"result_request\":%lld,\"peak_bytes\":%lld,\"logical_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[2]-before[2]),(long long)(v[3]-before[3]),(long long)v[5],(long long)length);
}
'''

SOURCE = '''extern "C" { fn repeat_before() -> Unit; fn repeat_after(s:CStr,length:Int,n:Int,kind:Int) -> Unit; }
let probe_repeat (s: String) (n: Int): String = s.repeat(n)
let main (): Unit = {
 let n=args()[0].parse_int().unwrap_or(0);
 let kind=args()[1].parse_int().unwrap_or(0);
 let s=if kind==0 { "abc" } else if kind==1 { "aé\\x00🙂" } else { "" };
 unsafe { repeat_before(); };
 let text=probe_repeat(s,n);
 unsafe { repeat_after(text.as_cstr(),text.len(),n,kind); };
 assert_eq(s.len(),if kind==0 {3} else if kind==1 {8} else {0})
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
    cases = [(0, n) for n in [-1, 0, 1, 2, 512, 1024, 2048, 4096, 5460, 16384, 65536]]
    cases += [(1, n) for n in [1, 512, 4096, 8191]] + [(2, 65536)]
    with tempfile.TemporaryDirectory(prefix='plum-repeat-measure-') as temp:
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
                for kind, n in cases:
                    result = subprocess.run([str(binary), str(n), str(kind)], env=env,
                                            check=True, capture_output=True, text=True, timeout=30)
                    row = dict(compiler=label, mode=mode, kind=kind, n=n, **json.loads(result.stdout))
                    rows.append(row)
                    print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Actual unmodified compiler builds; allocation-free native observer verifies all bytes, length and NUL. Traffic/live deltas exclude input/setup; peak is whole-process. No timing/RSS inference.',
        patterns={'0': 'abc', '1': 'a + é + NUL + 🙂 (8 bytes)', '2': 'empty'},
        results=rows), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
