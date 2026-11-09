#!/usr/bin/env python3
"""Measure actual Encoding.hex_decode programs, without rewriting emitted LLVM."""
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
#include <string.h>
extern int64_t plum_heap_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t,int64_t);
static int64_t before[6];
void decode_before(void){int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i);}
void decode_after(const unsigned char *p,int64_t length,int64_t n,int64_t kind,bool ok){
 if(ok!=(kind<3))abort();
 if(ok){
  const unsigned char pattern[]={97,0,255,43};
  if(length!=4*n || p[length])abort();
  for(int64_t i=0;i<length;i++)if(p[i]!=pattern[i%4])abort();
 }else{
  char error[128];
  if(kind==5 || kind==6)strcpy(error,"Encoding.hex_decode: odd number of hex digits");
  else snprintf(error,sizeof(error),"Encoding.hex_decode: not a hex digit at byte %lld",(long long)(kind==7?0:n*8));
  if(length!=(int64_t)strlen(error)||strcmp((const char*)p,error))abort();
 }
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"peak_bytes\":%lld,\"payload_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)v[5],(long long)length);
}
'''

SOURCE = r'''use Encoding;
let probe_decode(s:String):Result[Bytes,String]=Encoding.hex_decode(s)
extern "C" {fn decode_before()->Unit;fn decode_after(s:CStr,length:Int,n:Int,kind:Int,ok:Bool)->Unit;}
let main():Unit={
 let n=args()[0].parse_int().unwrap_or(0);let kind=args()[1].parse_int().unwrap_or(0);
 let unit=if kind==1 {"6100FF2B"} else if kind==2 {"6100fF2b"} else {"6100ff2b"};
 let suffix=if kind==3 {"GG"} else if kind==4 {"AG"} else if kind==5 {"G"} else if kind==6 {"GGG"} else {""};
 let prefix=if kind==7 {"GG"} else {""};
 let text=prefix.concat(unit.repeat(n)).concat(suffix);
 unsafe {decode_before();};
 match probe_decode(text) {
  Ok(out)=>unsafe {decode_after(out.as_cstr(),out.len(),n,kind,true);},
  Err(e)=>unsafe {decode_after(e.as_cstr(),e.len(),n,kind,false);},
 };
 assert_eq(text.len(),prefix.len()+n*8+suffix.len())
}
'''

ARRAY_ALTERNATIVE = r'''let bytes_at_raw (b: Bytes) (i: Int): Int = 0
let array_hex (c: Int): Int =
    if c >= 48 && c <= 57 { c - 48 }
    else if c >= 97 && c <= 102 { c - 97 + 10 }
    else if c >= 65 && c <= 70 { c - 65 + 10 }
    else { 0 - 1 }

let array_scan (b: Bytes) (i: Int) (n: Int) (acc: Array[Int]): Result[Array[Int], String] =
    if i >= n { Ok(acc) }
    else {
        let hi = array_hex(bytes_at_raw(b, i));
        let lo = array_hex(bytes_at_raw(b, i + 1));
        if hi < 0 || lo < 0 { Err("Encoding.hex_decode: not a hex digit at byte ".concat(i.to_string())) }
        else { array_scan(b, i + 2, n, acc.push(hi * 16 + lo)) }
    }

let array_decode (s: String): Result[Bytes, String] = {
 let b=s.as_bytes();let n=b.len();
 if n % 2 != 0 {Err("Encoding.hex_decode: odd number of hex digits")}
 else {Result.map(array_scan(b,0,n,[]),|vals|Bytes.from_array(vals))}
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
                 ('array', str(Path(args.before).resolve())),
                 ('after', str(Path(args.after).resolve()))]
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_CC', None)
    rows = []
    cases = [(n, kind) for n in [0, 1, 512, 4096, 32768] for kind in range(8)]
    with tempfile.TemporaryDirectory(prefix='plum-hex-decode-measure-') as temp:
        work = Path(temp)
        project = work / 'project'
        project.mkdir()
        observer = work / 'observer.c'
        observer.write_text(OBSERVER)
        for label, compiler in compilers:
            content = SOURCE if label != 'array' else SOURCE.replace('Encoding.hex_decode(s)', 'array_decode(s)') + ARRAY_ALTERNATIVE
            (project / 'main.plum').write_text(content, encoding='utf-8')
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
        method='Before/after use actual unmodified public builds; array is a source-level byte-scanner prototype retaining the original output array, compiled by before. An allocation-free native observer verifies all decoded bytes or the exact diagnostic, length and NUL. Public call traffic includes Result bookkeeping; snapshot is taken inside the matched arm. Traffic/live deltas exclude input/setup; Peak is whole-process. No timing/RSS inference.',
        pattern='6100ff2b repeated n times; kind 0 lowercase, 1 uppercase, 2 mixed; 3 late GG, 4 late AG, 5 odd G, 6 odd GGG (odd length outranks invalid digits), 7 early GG',
        results=rows), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
