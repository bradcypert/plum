#!/usr/bin/env python3
"""THROWAWAY actual-Plum map comparison. Results are not production acceptance."""
import argparse, hashlib, json, os, pathlib, platform, statistics, subprocess
ROOT = pathlib.Path(__file__).resolve().parent
ap = argparse.ArgumentParser()
ap.add_argument('--compiler', default=os.environ.get('PLUM', './sh'))
ap.add_argument('--out', default='/tmp/plum-map-prototype-281')
ap.add_argument('--quick', action='store_true')
ap.add_argument('--sanitize', action='store_true')
ap.add_argument('--timing-only', action='store_true')
a = ap.parse_args()
out = pathlib.Path(a.out); out.mkdir(parents=True, exist_ok=True)
compiler = str(pathlib.Path(a.compiler).resolve())
(out/'metadata.json').write_text(json.dumps({
    'compiler': compiler, 'compiler_sha256': hashlib.sha256(pathlib.Path(compiler).read_bytes()).hexdigest(),
    'revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
    'platform': platform.platform(), 'quick': a.quick, 'sanitize': a.sanitize, 'timing_only': a.timing_only,
    'source_sha256': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in ROOT.iterdir() if p.suffix in ('.plum', '.py', '.c')},
}, indent=2)+'\n')
results=[]
for variant in ['baseline', 'patricia', 'bitmap16', 'bitmap32', 'rbtree']:
 for key, value, collision in [('Int', 'Int', False), ('String', 'String', False), ('Int', 'String', True), ('String', 'Int', True)]:
    project = out / f'{variant}-{key}-{value}-{collision}'; project.mkdir(exist_ok=True)
    template = 'bitmap' if variant.startswith('bitmap') else variant
    source = (ROOT / f'{template}.plum').read_text()
    if variant != 'baseline': source += '\n' + (ROOT/'adapter.plum').read_text()
    elif collision:
        prelude = (ROOT.parent/'std/prelude.plum').read_text()
        flat = 'struct FlatEntry[K, V] { key: K, value: V }\nstruct FlatMap[K, V] { buckets: Array[Array[MapEntry[K, V]]], size: Int }\n'
        flat += prelude[prelude.index('let MAP_INITIAL_BUCKETS'):prelude.index('/// How many entries.')]
        flat = flat.replace('Map.', 'FlatMap.').replace('Map[', 'FlatMap[').replace('Map {', 'FlatMap {')
        flat = flat.replace('MapEntry', 'FlatEntry').replace('FlatFlatMap', 'FlatMap').replace('value_hash(k)', '(k.hash() & 3)')
        source = flat + source.replace('Map.', 'FlatMap.').replace('Map[', 'FlatMap[')
    source = source.replace('KEY', key).replace('VALUE', value)
    source = source.replace('MASK', '15' if variant == 'bitmap16' else '31').replace('BITS', '4' if variant == 'bitmap16' else '5')
    source += '\nlet key_hash (k: '+key+'): Int = k.hash()' + (' & 3' if collision else '') + '\n'
    source += '\nlet make_key (i: Int): '+key+' = '+('i' if key == 'Int' else 'i.to_string()')+'\n'
    source += '\nlet make_value (i: Int): '+value+' = '+('i' if value == 'Int' else 'i.to_string()')+'\n'
    source += (ROOT / 'driver.plum').read_text()
    (project / 'main.plum').write_text(source)
    for tracked in ([False] if a.sanitize or a.timing_only else [False, True]):
        binary = project / ('tracked' if tracked else 'plain')
        env = os.environ.copy()
        if a.sanitize:
            wrapper = out / 'sanitize-cc'
            wrapper.write_text('#!/bin/sh\nexec clang -fsanitize=address -fno-omit-frame-pointer \"$@\"\n')
            wrapper.chmod(0o755)
            env['PLUM_CC'] = str(wrapper)
            env['ASAN_OPTIONS'] = 'detect_leaks=1'
        if tracked:
            wrapper = out / 'tracking-cc'
            wrapper.write_text('#!/bin/sh\nexec clang -DTRACK "$@" -Wl,--wrap=malloc -Wl,--wrap=free\n')
            wrapper.chmod(0o755)
            env['PLUM_CC'] = str(wrapper)
        cmd=[compiler,'build',str(project),'--release','--native-deps','none','--link-c',str(ROOT/'metrics.c'),'-o',str(binary)]
        result=subprocess.run(cmd,env=env,text=True,capture_output=True,timeout=120)
        if result.returncode: raise RuntimeError(result.stdout + result.stderr)
        for n in ([1000] if a.quick else [1,8,32,1000,10000]):
         for mode in [0,1,2]:
            if (mode == 2 or collision) and n > 1000: continue
            samples=[]
            for _ in range(1 if tracked or a.quick else 3):
                runenv=dict(env,PROTO_N=str(n),PROTO_MODE=str(mode))
                r=subprocess.run([str(binary)],env=runenv,text=True,capture_output=True,timeout=30)
                if r.returncode: raise RuntimeError(r.stdout+r.stderr)
                rows=[list(map(int,line.split()[1:])) for line in r.stderr.splitlines() if line.startswith('METRIC ')]
                if len(rows)!=7: raise RuntimeError(r.stderr)
                samples.append(rows)
            row={'variant':variant,'key':key,'value':value,'collision':collision,'n':n,'mode':mode,'tracked':tracked,
                 'build_ns':statistics.median(s[1][1]-s[0][1] for s in samples),
                 'lookup_ns':statistics.median(s[2][1]-s[1][1] for s in samples),
                 'build_allocs':samples[0][1][2]-samples[0][0][2],
                 'build_bytes':samples[0][1][3]-samples[0][0][3],
                 'live_after_build':samples[0][1][4], 'peak':samples[0][6][5],
                 'live_after_scope':samples[0][6][4], 'live_empty_after_snapshot_drop':samples[0][5][4], 'live_deleted_with_snapshots':samples[0][4][4], 'mixed_ns':statistics.median(s[3][1]-s[2][1] for s in samples), 'delete_ns':statistics.median(s[4][1]-s[3][1] for s in samples), 'total_bytes':samples[0][6][3]-samples[0][0][3]}
            assert not tracked or row['live_after_scope'] == 0, row
            results.append(row)
        print(variant,key,value,collision,'tracked' if tracked else 'plain',flush=True)
        (out/'results.json').write_text(json.dumps(results,indent=2)+'\n')
print(out/'results.json')
