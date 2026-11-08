#!/usr/bin/env python3
"""Compare actual compiled append allocations on Linux/glibc; no IR rewriting."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


def program(kind, n):
    return '''extern "C" { fn begin() -> Unit; fn finish(s: CStr, length: Int) -> Unit; }
let build (s: String) (n: Int): String = if n <= 0 { s } else { build(s.concat("abc"), n - 1) }
let main (): Unit = unsafe {
 begin();
'''+(f' let s = build("", {n});\n' if kind=='repeat' else f''' let mut s = "";
 for i in 0..{n} {{ s = s.concat(if i % 2 == 0 {{ "a" }} else {{ "é\\x00🙂" }}); }};
''')+''' finish(s.as_cstr(), s.len())
}
'''


def observer(kind, n):
    return '''#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <malloc.h>
extern int64_t plum_heap_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t,int64_t);
static int64_t before[6];
void begin(void) { int64_t t=plum_heap_snapshot(); for(int i=0;i<6;i++) before[i]=plum_heap_snapshot_field(t,i); }
void finish(const unsigned char *s,int64_t length) {
 const unsigned char pattern[]='''+('{97,98,99}' if kind=='repeat' else '{97,0xc3,0xa9,0,0xf0,0x9f,0x99,0x82}')+''';
 int64_t expected='''+str(n*3 if kind=='repeat' else n*4)+''';
 if(length!=expected || s[length]) abort();
 for(int64_t i=0;i<length;i++) if(s[i]!=pattern[i%sizeof(pattern)]) abort();
 int64_t t=plum_heap_snapshot(),v[6]; for(int i=0;i<6;i++) v[i]=plum_heap_snapshot_field(t,i);
 if(v[2]-before[2]!=1) abort();
 printf("{\\"allocations\\":%lld,\\"allocated_bytes\\":%lld,\\"result_request\\":%lld,\\"usable\\":%llu,\\"peak_bytes\\":%lld,\\"logical_bytes\\":%lld}\\n",
 (long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(unsigned long long)malloc_usable_size((void *)(s-16)),(long long)v[5],(long long)length);
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before', help='baseline compiler binary')
    parser.add_argument('after', help='compiler built from current seed and source')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    compilers = [('main', str(Path(args.before).resolve())),
                 ('production', str(Path(args.after).resolve()))]
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_CC', None)
    results = []
    with tempfile.TemporaryDirectory(prefix='plum-string-measure-') as temp:
        for kind, n in [('repeat', 4096), ('repeat', 5460), ('mixed', 8192)]:
            project = Path(temp) / f'{kind}{n}'
            (project / 'native').mkdir(parents=True)
            (project / 'main.plum').write_text(program(kind, n), encoding='utf-8')
            (project / 'native/probe.c').write_text(observer(kind, n))
            for label, compiler in compilers:
                for release in (False, True):
                    binary = project / 'app'
                    flags = ['--release'] if release else []
                    built = subprocess.run([compiler, 'build', str(project), '--memory-stats',
                                            *flags, '-o', str(binary)], cwd=root, env=env,
                                           capture_output=True, text=True, timeout=300)
                    if built.returncode:
                        raise RuntimeError(built.stdout + built.stderr)
                    result = subprocess.run([str(binary)], check=True, env=env,
                                            capture_output=True, text=True, timeout=30)
                    row = dict(policy=label, mode='release' if release else 'debug',
                               kind=kind, n=n, **json.loads(result.stdout))
                    results.append(row)
                    print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        description='Actual unmodified compiler output, Linux/glibc; allocation-free native '
                    'observer; deltas exclude args setup; peak is whole-process; no timing/RSS inference',
        results=results), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
