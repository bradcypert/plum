#!/usr/bin/env python3
"""Measure production radix, bit-pattern and rounding storage; compare designs optionally."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

OBSERVER = r'''#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
extern int64_t plum_heap_snapshot(void),plum_heap_snapshot_field(int64_t,int64_t);
static int64_t b[6];
void radix_before(void){int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)b[i]=plum_heap_snapshot_field(t,i);}
void radix_after(const char *s,int64_t len,int64_t n,int64_t r,int64_t kind){
 const char *digits="0123456789abcdefghijklmnopqrstuvwxyz";char buf[66];int end=66,pos=end;
 if(kind==3){
  const char *want[]={"0","2","2","4","0.38"};if(n<0 || n>4)abort();
  int count=(int)strlen(want[n]);pos=end-count;memcpy(buf+pos,want[n],(size_t)count);
 } else if(kind){
  unsigned width=kind==1?16:64,shift=kind==1?4:1,mask=kind==1?15:1;
  for(unsigned i=0;i<width;i++)buf[--pos]=digits[((uint64_t)n>>(i*shift))&mask];
 } else {
  if(r<2)r=2;if(r>36)r=36;
  uint64_t m=n<0?(uint64_t)(-(n+1))+1:(uint64_t)n;
  do{buf[--pos]=digits[m%(uint64_t)r];m/=(uint64_t)r;}while(m);
  if(n<0)buf[--pos]='-';
 }
 if(len!=end-pos || memcmp(s,buf+pos,(size_t)len) || s[len])abort();
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"peak_bytes\":%lld,\"output_bytes\":%lld}\n",(long long)(v[0]-b[0]),(long long)(v[1]-b[1]),(long long)(v[3]-b[3]),(long long)v[5],(long long)len);
}
'''

SOURCE = r'''extern "C" {fn radix_before()->Unit;fn radix_after(s:CStr,length:Int,n:Int,r:Int,kind:Int)->Unit;}
let main():Unit={
 let input=args();let n=input[0].parse_int().unwrap_or(0);let r=input[1].parse_int().unwrap_or(0);let kind=input[2].parse_int().unwrap_or(0);
 // Reject an unexpected parser fallback rather than measuring another value.
 assert_eq(n.to_string(),input[0]);assert_eq(r.to_string(),input[1]);assert_eq(kind.to_string(),input[2]);
 unsafe {radix_before();};
 let out=if kind==0 {convert(n,r)} else if kind==1 {Int.to_hex_bits(n)} else if kind==2 {Int.to_bits(n)} else {
  let value=if n==0 {0.5} else if n==1 {1.5} else if n==2 {2.5} else if n==3 {3.5} else {0.375};
  Float.to_fixed(value,r)
 };
 unsafe {radix_after(out.as_cstr(),out.len(),n,r,kind);};assert_eq(out.len()>0,true)
}
'''

STATIC_DIGITS = r'''let digit(v:Int):String=match v {
    0 => "0",
    1 => "1",
    2 => "2",
    3 => "3",
    4 => "4",
    5 => "5",
    6 => "6",
    7 => "7",
    8 => "8",
    9 => "9",
    10 => "a",
    11 => "b",
    12 => "c",
    13 => "d",
    14 => "e",
    15 => "f",
    16 => "g",
    17 => "h",
    18 => "i",
    19 => "j",
    20 => "k",
    21 => "l",
    22 => "m",
    23 => "n",
    24 => "o",
    25 => "p",
    26 => "q",
    27 => "r",
    28 => "s",
    29 => "t",
    30 => "u",
    31 => "v",
    32 => "w",
    33 => "x",
    34 => "y",
    35 => "z",
    _ => "?",
}
let accum(n:Int)(r:Int)(acc:String):String=if n==0 {acc} else {accum(n/r,r,digit(n-(n/r)*r).concat(acc))}
let convert(n:Int)(base:Int):String={let r=Int.clamp(base,2,36);if n==0 {"0"} else if n<0 {"-".concat(accum(0-(n/r),r,digit(0-(n-(n/r)*r))))} else {accum(n,r,"")}}
'''

APPEND_REVERSE = r'''let digit(v:Int):String=match v {
    0 => "0",
    1 => "1",
    2 => "2",
    3 => "3",
    4 => "4",
    5 => "5",
    6 => "6",
    7 => "7",
    8 => "8",
    9 => "9",
    10 => "a",
    11 => "b",
    12 => "c",
    13 => "d",
    14 => "e",
    15 => "f",
    16 => "g",
    17 => "h",
    18 => "i",
    19 => "j",
    20 => "k",
    21 => "l",
    22 => "m",
    23 => "n",
    24 => "o",
    25 => "p",
    26 => "q",
    27 => "r",
    28 => "s",
    29 => "t",
    30 => "u",
    31 => "v",
    32 => "w",
    33 => "x",
    34 => "y",
    35 => "z",
    _ => "?",
}
let accum(n:Int)(r:Int)(acc:String):String=if n==0 {acc} else {accum(n/r,r,acc.concat(digit(n-(n/r)*r)))}
let convert(n:Int)(base:Int):String={
 let r=Int.clamp(base,2,36);if n==0 {"0"} else {
  let q=n/r;let tail=n-q*r;let d=digit(if n<0 {0-tail} else {tail});
  if q==0 {if n<0 {"-".concat(d)} else {d}} else {
   let back=accum(if n<0 {0-q} else {q},r,d);
   String.reverse(if n<0 {back.concat("-")} else {back})
  }
 }
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before')
    parser.add_argument('after')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--compare-designs', action='store_true')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_CC', None)
    rows = []
    variants = [('before', args.before, 'let convert(n:Int)(r:Int):String=Int.to_radix(n,r)\n'),
                ('after', args.after, 'let convert(n:Int)(r:Int):String=Int.to_radix(n,r)\n')]
    if args.compare_designs:
        variants += [('prototype-static-digits', args.before, STATIC_DIGITS),
                     ('prototype-append-reverse', args.before, APPEND_REVERSE)]
    values = [0, 1, 255, 9223372036854775807, -9223372036854775808]
    with tempfile.TemporaryDirectory(prefix='plum-radix-measure-') as temp:
        work = Path(temp)
        project = work / 'project'
        project.mkdir()
        observer = work / 'observer.c'
        observer.write_text(OBSERVER, encoding='utf-8')
        for label, compiler, converter in variants:
            compiler = str(Path(compiler).resolve())
            (project / 'main.plum').write_text(converter + SOURCE, encoding='utf-8')
            for mode, flags in [('debug', []), ('release', ['--release'])]:
                binary = work / 'app.exe'
                built = subprocess.run([compiler, 'build', project, '--memory-stats', *flags,
                                        '--link-c', observer, '-o', binary], cwd=repo, env=env,
                                       capture_output=True, text=True, encoding='utf-8', timeout=300)
                if built.returncode:
                    raise RuntimeError(built.stdout + built.stderr)
                cases = [(n, base, 0) for n in values for base in [2, 10, 16, 36, 1, 40]]
                if not label.startswith('prototype-'):
                    cases += [(n, 16, 1) for n in values] + [(n, 2, 2) for n in values]
                    cases += [(n, 2 if n == 4 else 0, 3) for n in range(5)]
                for n, base, kind in cases:
                    result = subprocess.run([str(binary), str(n), str(base), str(kind)], env=env,
                                            check=True, capture_output=True, text=True,
                                            encoding='utf-8', timeout=60)
                    row = dict(compiler=label, mode=mode, n=n, base=base, kind=kind, **json.loads(result.stdout))
                    rows.append(row)
                    print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Ordinary compiler builds; no LLVM substitution. Before/after use public APIs. Optional source prototypes use the before compiler and alter only radix conversion. An allocation-free C observer independently verifies every byte, length and NUL with unsigned full-range arithmetic; rounding uses explicit half-even examples. Parsing is checked before observation. Traffic/live deltas exclude setup; peak is whole-process and can be dominated by setup. Requested bytes, not RSS or throughput.',
        cases={'0':'signed radix conversion; radix clamps to 2..36', '1':'fixed-width hexadecimal bits',
               '2':'fixed-width binary bits, unchanged control', '3':'half-even fixed-decimal rounding examples'},
        results=rows), indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
