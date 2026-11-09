#!/usr/bin/env python3
"""Measure actual Bytes.to_hex programs, without rewriting emitted LLVM."""
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
void hex_before(void) { int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i); }
void hex_after(const char *s,int64_t length,int64_t n) {
 const char pattern[]="61c3a900f09f9982";
 if(length!=n*16 || s[length])abort();
 for(int64_t i=0;i<length;i++)if(s[i]!=pattern[i%16])abort();
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"result_request\":%lld,\"live_allocations\":%lld,\"peak_bytes\":%lld,\"input_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)(v[2]-before[2]),(long long)v[5],(long long)(n*8));
}
'''

SOURCE = r'''let probe_hex (b: Bytes): String = b.to_hex()
extern "C" { fn hex_before() -> Unit; fn hex_after(s:CStr,length:Int,n:Int) -> Unit; }
let main (): Unit = {
 let n=args()[0].parse_int().unwrap_or(0);
 let bytes="aé\x00🙂".repeat(n).as_bytes();
 unsafe { hex_before(); };
 let out=probe_hex(bytes);
 unsafe { hex_after(out.as_cstr(),out.len(),n); };
 assert_eq(bytes.len(),n*8)
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
    cases = [0, 1, 4, 32, 512, 4096, 32768]
    with tempfile.TemporaryDirectory(prefix='plum-hex-measure-') as temp:
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
                for n in cases:
                    result = subprocess.run([str(binary), str(n)], env=env,
                                            check=True, capture_output=True, text=True, timeout=30)
                    row = dict(compiler=label, mode=mode, n=n, **json.loads(result.stdout))
                    rows.append(row)
                    print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Actual unmodified compiler builds; allocation-free native observer verifies all bytes, length and NUL. Traffic/live deltas exclude input/setup; peak is whole-process. No timing/RSS inference.',
        pattern='a + é + NUL + 🙂 (8 bytes repeated n times)',
        results=rows), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
