#!/usr/bin/env python3
"""Exercise metadata consumers, query failures, and layout-independent inventories."""
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile

from compiler_metadata import ROOT, OUTPUT, check_generated, load, render
from plum_queries import ast, completion_names, parse_ast, query

builtins = runpy.run_path(str(ROOT / 'bootstrap/check-builtins'))
compiler = Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve()
data = load()
check_generated(data)
checks = 0


def expect_error(fn):
    global checks
    try:
        fn()
    except (OSError, ValueError):
        checks += 1
    else:
        raise AssertionError('invalid input was accepted')


with tempfile.TemporaryDirectory(prefix='plum-tooling-check-') as scratch:
    work = Path(scratch)

    def run(command, ok=True, **kwargs):
        global checks
        result = subprocess.run(list(map(str, command)), capture_output=True, text=True, timeout=120, **kwargs)
        assert (result.returncode == 0) == ok, (command, result.returncode, result.stdout, result.stderr)
        checks += 1
        return result

    # JSON layout is irrelevant; invalid or ambiguous metadata is rejected.
    metadata = work / 'metadata.json'
    for indent in [None, 2, '\t']:
        metadata.write_text(json.dumps(data, indent=indent), encoding='utf-8')
        assert load(metadata) == data
        checks += 1
    for version in ['0.0.0', '1.2.3-alpha.1+build.42']:
        metadata.write_text(json.dumps(dict(data, version=version)))
        assert load(metadata)['version'] == version
        checks += 1
    for patch in [
        {'schema_version': True}, {'schema_version': 2}, {'version': ''}, {'version': 'bad version'},
        {'version': '1.2.3-.'}, {'version': '1.2.3-01'}, {'version': '01.2.3'},
        {'stdlib_modules': []}, {'stdlib_modules': ['Os', 'Os']}, {'stdlib_modules': ['bad-name']},
        {'stdlib_modules': 'Os'}, {'stdlib_modules': [1]}, {'unexpected': 1},
    ]:
        metadata.write_text(json.dumps(dict(data, **patch)), encoding='utf-8')
        expect_error(lambda: load(metadata))
    for invalid in ['{}', '[]', '{', '{"version":"1","version":"2"}']:
        metadata.write_text(invalid, encoding='utf-8')
        expect_error(lambda: load(metadata))
    expect_error(lambda: load(work / 'missing.json'))

    # Every generated variant must already be a formatter fixed point.
    generated = work / 'generated.plum'
    variants = [data['stdlib_modules'], ['Os']] + [['A' * 30, 'B' * 30, 'C' * n] for n in [23, 24, 25, 26]]
    for modules in variants:
        text = render(dict(data, stdlib_modules=modules))
        generated.write_text(text, encoding='utf-8')
        assert query(compiler, 'fmt', generated) == text
        check_generated(dict(data, stdlib_modules=modules), generated)
        checks += 1
    generated.write_text('// stale declarations\n', encoding='utf-8')
    expect_error(lambda: check_generated(data, generated))

    # Parse the same declarations with every token on a separate line, tabs,
    # and CRLF. Comments and strings that resemble code must remain inert.
    source = '''
// method_name == "fake_method"
/// name: "fake_entry"
pub let builtin_methods (): Array[BuiltinMethod] = [BuiltinMethod { name: "len", doc: "name=" }]
pub let builtin_functions (): Array[BuiltinFn] = [BuiltinFn { name: "args" }]
let builtin_sig (name: String): Bool = name == "args"
let infer (method_name: String): Bool = method_name == "len"
let prose (): String = "method_name == \\"not_code\\""
let main (): Unit = ()
'''
    fixture = work / 'inventory.plum'
    fixture.write_text(source, encoding='utf-8')
    tokens = query(compiler, 'relex', fixture, '--tokens').splitlines()
    for layout in [source, '\n'.join(tokens), '\r\n'.join(tokens), '\t'.join(tokens)]:
        fixture.write_text(layout, encoding='utf-8')
        assert builtins['inventories'](ast(compiler, fixture)) == {'len'}
        checks += 1
    for original, replacement in [('name: "len"', 'name: "other"'), ('name: "args"', 'name: "other"')]:
        fixture.write_text(source.replace(original, replacement), encoding='utf-8')
        expect_error(lambda: builtins['inventories'](ast(compiler, fixture)))
    fixture.write_text(source.replace('[BuiltinFn { name: "args" }]', '[]'))
    expect_error(lambda: builtins['inventories'](ast(compiler, fixture)))
    fixture.write_text(source + '\nlet extra (method_name: String): Bool =\n\n// a new case\nmethod_name == "missing"\n')
    expect_error(lambda: builtins['inventories'](ast(compiler, fixture)))
    for invalid in ['', '()', '((', '(()))', '((let x "unterminated))']:
        expect_error(lambda: parse_ast(invalid))
    for invalid in ['', '{}', '[]', '[{}]', '[{"name":1}]', '[{"name":""}]']:
        expect_error(lambda: completion_names(invalid))
    fixture.write_text('let main (): Unit = { xs\n. len\n(); println(".fake()"); }\n')
    assert builtins['dot_calls'](ast(compiler, fixture)) == {'len'}
    checks += 1

    # Run real consumers in a tiny checkout so missing/corrupt/stale inputs
    # exercise their exit statuses without changing production source files.
    repo = work / 'repo'
    (repo / 'bootstrap').mkdir(parents=True)
    for name in ['compiler_metadata.py', 'plum_queries.py', 'gen-compiler-metadata', 'check-version', 'check-doc-names', 'check-builtins']:
        shutil.copy2(ROOT / 'bootstrap' / name, repo / 'bootstrap' / name)
    out = repo / OUTPUT.relative_to(ROOT)
    out.parent.mkdir(parents=True)
    out.write_text(render(data), encoding='utf-8')
    meta = repo / 'bootstrap/compiler-metadata.json'
    meta.write_text(json.dumps(data), encoding='utf-8')
    (repo / 'bootstrap/build-site').write_text('')
    (repo / 'README.md').write_text('The `Array.len` function.\n')
    stub = work / 'compiler'

    def fake(code):
        stub.write_text('#!' + sys.executable + '\nimport sys\n' + code + '\n', encoding='utf-8')
        stub.chmod(0o755)

    env = dict(os.environ, PLUM=str(work / 'no-compiler'))
    run([repo / 'bootstrap/gen-compiler-metadata', '--check'], env=env)
    run([repo / 'bootstrap/check-version', '--source-only', 'v' + data['version']], env=env)
    run([repo / 'bootstrap/check-version', '--source-only', 'v999.0.0'], ok=False)
    env['PLUM'] = str(stub)
    fake('print(' + repr('plum ' + data['version']) + ')')
    run([repo / 'bootstrap/check-version', 'v' + data['version']], env=env)
    fake('print(' + repr('plum ' + data['version']) + '); sys.exit(1)')
    run([repo / 'bootstrap/check-version', 'v' + data['version']], env=env, ok=False)
    for response in ['', 'plum 999.0.0', 'plum ' + data['version'] + ' extra']:
        fake('print(' + repr(response) + ')')
        run([repo / 'bootstrap/check-version', 'v' + data['version']], env=env, ok=False)

    names = [{'name': m + '.example'} for m in data['stdlib_modules']] + [{'name': 'Array.len'}]
    fake('print(' + repr(json.dumps(names, indent=2)) + ')')
    run([repo / 'bootstrap/check-doc-names', stub])
    baseline_ast = query(compiler, 'ast', ROOT / 'bootstrap/self_host/typecheck/infer.plum')
    fake('print(' + repr(baseline_ast) + ') if sys.argv[1] == "ast" else print(' + repr(json.dumps(names[1:])) + ')')
    run([repo / 'bootstrap/check-doc-names', stub], ok=False)
    run([repo / 'bootstrap/check-builtins', stub], ok=False)
    for response in ['', '{}', '[]', '[{}]']:
        fake('print(' + repr(baseline_ast) + ') if sys.argv[1] == "ast" else print(' + repr(response) + ')')
        run([repo / 'bootstrap/check-doc-names', stub], ok=False)
        run([repo / 'bootstrap/check-builtins', stub], ok=False)
    fake('print(' + repr(baseline_ast) + ') if sys.argv[1] == "ast" else sys.exit(1)')
    run([repo / 'bootstrap/check-builtins', stub], ok=False)
    run([repo / 'bootstrap/check-doc-names', stub], ok=False)

    for contents in ['{}', '{', json.dumps(dict(data, stdlib_modules=[]))]:
        meta.write_text(contents)
        run([repo / 'bootstrap/gen-compiler-metadata', '--check'], ok=False)
        run([repo / 'bootstrap/check-version', '--source-only', 'v' + data['version']], ok=False)
        run([repo / 'bootstrap/check-doc-names', stub], ok=False)
    meta.unlink()
    run([repo / 'bootstrap/check-version', '--source-only', 'v' + data['version']], ok=False)
    meta.write_text(json.dumps(data))
    out.write_text('// stale\n')
    run([repo / 'bootstrap/check-version', '--source-only', 'v' + data['version']], ok=False)
    run([repo / 'bootstrap/check-doc-names', stub], ok=False)

print(f'tooling-metadata: {checks} metadata, layout, query and failure checks passed')
