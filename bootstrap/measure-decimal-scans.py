#!/usr/bin/env python3
"""Compare real fixed-formatting storage and source-level decimal-read alternatives."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

SOURCE = r'''extern "C" {
 fn decimal_value(id:Int)->Float;fn decimal_before()->Unit;
 fn decimal_after(text:CStr,length:Int)->Unit;
}
let main():Unit={
 let a=args();let id=a[0].parse_int().unwrap_or(0);let d=a[1].parse_int().unwrap_or(0);
 assert_eq(id.to_string(),a[0]);assert_eq(d.to_string(),a[1]);
 let x=unsafe{decimal_value(id)};unsafe{decimal_before();};
 let text=convert(x,d);unsafe{decimal_after(text.as_cstr(),text.len());};
 assert_eq(a.len(),2);assert_eq(text.len()>0,true)
}
'''

OBSERVER = r'''#include <float.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
extern int64_t plum_heap_snapshot(void),plum_heap_snapshot_field(int64_t,int64_t);
static int64_t before[6];
double decimal_value(int64_t id){
 const double values[]={0.5,1.5,2.5,3.5,0.375,9.99,-0.001,1.0/3.0,DBL_MAX,-DBL_MAX,
 DBL_MIN,-DBL_MIN,0x1p-1074,-0x1p-1074,INFINITY,-INFINITY,NAN,-NAN,-0.0,0.0,
 0.5000000000000001,0.49999999999999994,1.4999999999999998,1.5000000000000002,
 99.5,999.5,9.999999999999998,0.125};
 if(id<0 || (uint64_t)id>=sizeof values/sizeof *values)abort();return values[id];
}
void decimal_before(void){int64_t t=plum_heap_snapshot();if(!t)abort();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i);}
void decimal_after(const unsigned char*s,int64_t n){
 if(n<1 || n>512 || s[n])abort();
 for(int64_t i=0;i<n;i++)if(s[i]<32 || s[i]=='"' || s[i]=='\\')abort();
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"text\":\"%.*s\",\"allocations\":%lld,\"allocated_bytes\":%lld,\"result_live_bytes\":%lld,\"peak_bytes\":%lld,\"before_peak_bytes\":%lld}\n",
 (int)n,s,(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)v[5],(long long)before[5]);
}
'''

OPTION_DIGIT = '''let plum__digit_at (s: String) (i: Int): Int = {
    let v = Option.unwrap_or(Bytes.at(s.as_bytes(), i), 0);
    if v >= 48 && v <= 57 { v - 48 } else { 0 }
}'''
CHECKED_DIGIT = '''let plum__digit_at (s: String) (i: Int): Int =
    if i < 0 || i >= s.len() { 0 } else {
        let v = bytes_at_raw(bytes_of_str_raw(s), i);
        if v >= 48 && v <= 57 { v - 48 } else { 0 }
    }'''


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('before')
    ap.add_argument('after')
    ap.add_argument('--out', required=True)
    ap.add_argument('--compare-designs', action='store_true')
    ap.add_argument('--sanitize', action='store_true')
    args = ap.parse_args()
    root = Path(__file__).resolve().parent.parent
    binaries = {'before': str(Path(args.before).resolve()), 'after': str(Path(args.after).resolve())}
    legacy_path = root / 'bootstrap/profiling/decimal-scan-legacy.plum.txt'
    legacy = legacy_path.read_text(encoding='utf-8')
    api = 'let convert(x:Float)(d:Int):String=Float.to_fixed(x,d)\n'
    variants = [('before', binaries['before'], api), ('after', binaries['after'], api)]
    if args.compare_designs:
        old = 'let plum__digit_at (s: String) (i: Int): Int =\n    Option.unwrap_or(String.index_of("0123456789", String.slice(s, i, i + 1)), 0)'
        if legacy.count(old) != 1:
            raise RuntimeError('legacy digit source no longer matches')
        variants.append(('legacy-source', binaries['before'], legacy + '\nlet convert(x:Float)(d:Int):String=model(x,d)\n'))
        for name, body in [('option-digit', OPTION_DIGIT), ('checked-digit', CHECKED_DIGIT)]:
            code = re.sub(r'String.char_len\((s|body|fp|bumped|kept)\)', r'\1.len()', legacy.replace(old, body))
            variants.append((name, binaries['before'], code + '\nlet convert(x:Float)(d:Int):String=model(x,d)\n'))
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1', ASAN_OPTIONS='detect_leaks=1:halt_on_error=1', LSAN_OPTIONS='exitcode=23')
    env.pop('PLUM_CC', None)

    def run(command):
        result = subprocess.run([str(x) for x in command], cwd=root, env=env,
                                capture_output=True, text=True, encoding='utf-8', timeout=300)
        if result.returncode:
            raise RuntimeError(f'{command}: exit {result.returncode}\n{result.stdout}\n{result.stderr}')
        return result.stdout

    rows, expected = [], {}
    precisions = [-1, 0, 1, 2, 8, 16, 17, 18, -(2**63), 2**63 - 1]
    focus = [(0, 0), (1, 0), (2, 0), (3, 0), (4, 2), (5, 1), (6, 2), (7, 2), (8, 17)]
    with tempfile.TemporaryDirectory(prefix='plum-decimal-measure-') as directory:
        work = Path(directory)
        (work / 'native').mkdir()
        (work / 'native/probe.c').write_text(OBSERVER, encoding='utf-8')
        for label, compiler, code in variants:
            (work / 'main.plum').write_text(code + SOURCE, encoding='utf-8')
            for release in (False, True):
                binary = work / 'program.exe'
                run([compiler, 'build', work, '--memory-stats', *(['--release'] if release else []),
                     *(['--sanitize=address'] if args.sanitize else []), '-o', binary])
                cases = [(i, d) for i in range(28) for d in precisions] if label in ('before', 'after') else focus
                for value, precision in cases:
                    observation = json.loads(run([binary, value, precision]))
                    key = (release, value, precision)
                    if label == 'before':
                        expected[key] = observation['text']
                    elif observation['text'] != expected[key]:
                        raise RuntimeError(f'{label}: changed formatting for {key}: {observation["text"]} != {expected[key]}')
                    rows.append(dict(observation, compiler=label, release=release, value=value, precision=precision))
                print(f'decimal scans: {label} release={release} verified', flush=True)
    digest = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
    metadata = {'binaries': {label: {'path': path, 'sha256': digest(path)} for label, path in binaries.items()},
                'harness_sha256': digest(__file__), 'legacy_sha256': digest(legacy_path), 'sanitized': args.sanitize}
    method = 'Ordinary public API builds with diagnostics for before/after; optional frozen source models compiled with the before compiler recreate existing private prelude primitives explicitly. No LLVM substitution or C float-formatting oracle. Before/after texts must match for every case; models match the baseline on focus cases. C observer validates stored length/terminator and captures requested Plum-cell traffic/live, excluding setup. Peak is registered Plum heap across process setup, excluding diagnostic bookkeeping and libc internals; not RSS or speed.'
    Path(args.out).write_text(json.dumps({'method': method, 'metadata': metadata,
                                        'values': ['half-even down', 'half-even up', 'even tie', 'odd tie',
                                                   'fraction tie', 'carry', 'negative rounded zero', 'third',
                                                   'max finite', 'negative max', 'min normal', 'negative min normal',
                                                   'min subnormal', 'negative min subnormal', 'infinity', 'negative infinity',
                                                   'NaN', 'negative NaN', 'negative zero', 'zero',
                                                   'above half', 'below half', 'below 1.5', 'above 1.5',
                                                   'carry 99.5', 'carry 999.5', 'below ten', 'fraction tie down'],
                                        'records': rows}, indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    main()
