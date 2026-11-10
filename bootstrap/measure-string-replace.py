#!/usr/bin/env python3
"""Measure actual String.replace results, including identity and escaping chains."""
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
void replace_before(void){int64_t t=plum_heap_snapshot();for(int i=0;i<6;i++)before[i]=plum_heap_snapshot_field(t,i);}
void replace_after(const unsigned char *p,int64_t length,int64_t n,int64_t kind){
 const unsigned char *patterns[]={
  (const unsigned char *)"abc\xc3\xa9\0\xf0\x9f\x99\x82",
  (const unsigned char *)"abc\xc3\xa9\0\xf0\x9f\x99\x82",
  (const unsigned char *)"abc\xc3\xa9\0\xf0\x9f\x99\x82",
  (const unsigned char *)"a[\xc3\xa9]\0\xf0\x9f\x99\x82",
  (const unsigned char *)"a\xc3\xa9\0x",(const unsigned char *)"",
  (const unsigned char *)"aX\0b",(const unsigned char *)"",
  (const unsigned char *)"abc/def/",(const unsigned char *)"plain\xc3\xa9"};
 const int widths[]={10,10,10,10,5,0,4,0,8,7};
 int64_t expected=kind==7?(5*n)/2+(5*n)%2:widths[kind]*n;
 if(length!=expected || p[length])abort();
 for(int64_t i=0;i<length;i++){
  unsigned char want=kind==7?(i<(5*n)/2?'X':'a'):patterns[kind][i%widths[kind]];
  if(p[i]!=want)abort();
 }
 int64_t t=plum_heap_snapshot(),v[6];for(int i=0;i<6;i++)v[i]=plum_heap_snapshot_field(t,i);
 printf("{\"allocations\":%lld,\"allocated_bytes\":%lld,\"additional_live_bytes\":%lld,\"peak_bytes\":%lld,\"output_bytes\":%lld}\n",(long long)(v[0]-before[0]),(long long)(v[1]-before[1]),(long long)(v[3]-before[3]),(long long)v[5],(long long)length);
}
'''
SOURCE = r'''let probe(s:String)(pattern:String)(target:String):String=s.replace(pattern,target)
extern "C" {fn replace_before()->Unit;fn replace_after(s:CStr,length:Int,n:Int,kind:Int)->Unit;}
let main():Unit={
 let n=args()[0].parse_int().unwrap_or(0);let kind=args()[1].parse_int().unwrap_or(0);
 let unit=if kind<3 {"abcé\x00🙂"} else if kind<5 {"aé\x00🙂"} else if kind==5 {"a"} else if kind==6 {"a\x00b"} else if kind==7 {"aaaaa"} else if kind==8 {"abc/def/"} else {"plainé"};
 let pattern=if kind==0 {"!"} else if kind==1 {"é"} else if kind==2 {""} else if kind==3 {"é"} else if kind==4 {"🙂"} else if kind==5 {"a"} else if kind==6 {"\x00"} else if kind==7 {"aa"} else {"\\"};
 let target=if kind==0 {"XX"} else if kind==1 {"é"} else if kind==2 {"X"} else if kind==3 {"[é]"} else if kind==4 {"x"} else if kind==5 {""} else if kind==6 {"X\x00"} else if kind==7 {"X"} else {"/"};
 let text=unit.repeat(n);
 unsafe {replace_before();};
 let out=if kind==9 {probe(probe(probe(text,"&","&amp;"),"<","&lt;"),">","&gt;")} else {probe(text,pattern,target)};
 unsafe {replace_after(out.as_cstr(),out.len(),n,kind);};
 assert_eq(text.len(),unit.len()*n)
}
'''
PROTOTYPE = r'''let prototype(s:String)(pattern:String)(target:String):String=
 if s.len()==0 {""}
 else if pattern=="" || pattern==target || !s.contains(pattern) {s}
 else {s.replace(pattern,target)}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before')
    parser.add_argument('after')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    before, after = str(Path(args.before).resolve()), str(Path(args.after).resolve())
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_CC', None)
    rows = []
    with tempfile.TemporaryDirectory(prefix='plum-replace-measure-') as temp:
        work = Path(temp)
        project = work / 'project'
        project.mkdir()
        observer = work / 'observer.c'
        observer.write_text(OBSERVER)
        for label, compiler in [('before', before), ('prototype', before), ('after', after)]:
            source = SOURCE if label != 'prototype' else SOURCE.replace('=s.replace(pattern,target)', '=prototype(s,pattern,target)') + PROTOTYPE
            (project / 'main.plum').write_text(source)
            for mode, flags in [('debug', []), ('release', ['--release'])]:
                binary = work / 'app.exe'
                built = subprocess.run([compiler, 'build', project, '--memory-stats', *flags,
                                        '--link-c', observer, '-o', binary], cwd=repo, env=env,
                                       capture_output=True, text=True, timeout=300)
                if built.returncode:
                    raise RuntimeError(built.stdout + built.stderr)
                for n in [0, 1, 512, 4096, 32768]:
                    for kind in range(10):
                        result = subprocess.run([str(binary), str(n), str(kind)], env=env,
                                                check=True, capture_output=True, text=True, timeout=60)
                        row = dict(compiler=label, mode=mode, n=n, kind=kind, **json.loads(result.stdout))
                        rows.append(row)
                        print(row, flush=True)
    args.out.write_text(json.dumps(dict(
        method='Ordinary compiler builds with an allocation-free native observer verifying every output byte, length and NUL. Prototype is a source-level identity wrapper built by before; it retains the old changed-result constructor and does not add production size/failure guards. Input remains live; traffic/live deltas exclude setup; peak is whole-process. No timing/RSS inference.',
        cases={'0':'no match, Unicode/NUL input','1':'identical pattern/target','2':'empty pattern','3':'UTF-8 expansion','4':'UTF-8 shrink','5':'delete all','6':'NUL match and insertion','7':'nonoverlap aa in a repeated input','8':'no-match path normalization','9':'three-stage no-match HTML escaping'},
        results=rows), indent=2) + '\n')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
