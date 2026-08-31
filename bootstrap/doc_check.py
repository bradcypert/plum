# Extracts the ```plum blocks from a markdown document, builds each as a
# real project, runs it, and compares against what the document claims.
#
# A RUN of consecutive ```plum blocks is ONE project. A block whose
# first line is `// some/path.plum` becomes that file; a block without
# one becomes `main.plum`. That is how the README already writes its
# module examples -- two blocks, each headed by its own path -- and it
# is the only way to check them, since a module example is a project by
# definition and cannot be a single file.
#
# The block after the run says what to expect: a plain block starting
# with `error:` means it must NOT build, with that message; any other
# plain block is the exact expected output; anything else means "must
# build" and nothing more.
import re, subprocess, sys, os, tempfile, shutil

doc = open(sys.argv[1]).read()
plum = sys.argv[2]
label = os.path.basename(sys.argv[1])
blocks = re.findall(r'```(\w*)\n(.*?)```', doc, re.S)

FILE_HEADER = re.compile(r'^//\s*([\w][\w./-]*\.plum)\s*$')

projects = []
i = 0
while i < len(blocks):
    if blocks[i][0] != 'plum':
        i += 1
        continue
    files = {}
    order = []
    while i < len(blocks) and blocks[i][0] == 'plum':
        body = blocks[i][1]
        first = body.strip().split('\n')[0].strip() if body.strip() else ''
        m = FILE_HEADER.match(first)
        path = m.group(1) if m else 'main.plum'
        if path not in files:
            order.append(path)
            files[path] = body
        else:
            files[path] += body
        i += 1
    nxt = blocks[i] if i < len(blocks) else ('', '')
    projects.append((files, order, nxt[0], nxt[1]))

work = tempfile.mkdtemp()
fails = 0
for n, (files, order, nlang, nbody) in enumerate(projects, 1):
    d = os.path.join(work, "p%d" % n)
    for path, src in files.items():
        full = os.path.join(d, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        open(full, "w").write(src)
    where = order[0] if len(order) == 1 else "%s (+%d more)" % (order[0], len(order) - 1)
    expect_err = nlang == '' and nbody.startswith('error:')
    r = subprocess.run([plum, "build", d, "-o", d + "/out"], capture_output=True, text=True)
    if expect_err:
        out = r.stdout + r.stderr
        want = nbody.strip().splitlines()[0]
        if r.returncode == 0 and "error" not in out:
            print("FAIL %d: expected a compile error, it built  [%s]" % (n, where)); fails += 1
        elif want.replace("error: ", "") not in out:
            print("FAIL %d: wrong error  [%s]\n   want %s\n   got  %s" % (n, where, want, out.strip().splitlines()[0] if out.strip() else "")); fails += 1
        else:
            print("ok   %d  (rejected: %s)" % (n, want))
        continue
    if r.returncode != 0:
        print("FAIL %d: did not build  [%s]\n   %s" % (n, where, (r.stdout + r.stderr).strip().splitlines()[0] if (r.stdout+r.stderr).strip() else "")); fails += 1
        continue
    run = subprocess.run([d + "/out"], capture_output=True, text=True)
    got = run.stdout.strip()
    if nlang == '' and nbody.strip() and not nbody.startswith('error:'):
        want = nbody.strip()
        if got != want:
            print("FAIL %d: output differs  [%s]\n   want %r\n   got  %r" % (n, where, want, got)); fails += 1
        else:
            print("ok   %d  -> %s" % (n, got.replace("\n", " | ")))
    else:
        print("ok   %d  (built; no output claimed)" % n)
shutil.rmtree(work)
print("%s: %d snippets, %d failures\n" % (label, len(projects), fails))
sys.exit(1 if fails else 0)
