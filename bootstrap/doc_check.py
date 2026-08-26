import re, subprocess, sys, os, tempfile, shutil
doc = open(sys.argv[1]).read()
plum = sys.argv[2]
blocks = re.findall(r'```(\w*)\n(.*?)```', doc, re.S)
pairs = []
for i, (lang, body) in enumerate(blocks):
    if lang != 'plum':
        continue
    nxt = blocks[i+1] if i+1 < len(blocks) else ('', '')
    pairs.append((body, nxt[0], nxt[1]))
work = tempfile.mkdtemp()
fails = 0
for n, (src, nlang, nbody) in enumerate(pairs, 1):
    d = os.path.join(work, "p%d" % n); os.makedirs(d)
    open(os.path.join(d, "main.plum"), "w").write(src)
    expect_err = nlang == '' and nbody.startswith('error:')
    r = subprocess.run([plum, "build", d, "-o", d + "/out"], capture_output=True, text=True)
    if expect_err:
        out = r.stdout + r.stderr
        want = nbody.strip().splitlines()[0]
        if r.returncode == 0 and "error" not in out:
            print("FAIL %d: expected a compile error, it built" % n); fails += 1
        elif want.replace("error: ", "") not in out:
            print("FAIL %d: wrong error\n   want %s\n   got  %s" % (n, want, out.strip().splitlines()[0] if out.strip() else "")); fails += 1
        else:
            print("ok   %d  (rejected: %s)" % (n, want))
        continue
    if r.returncode != 0:
        print("FAIL %d: did not build\n   %s" % (n, (r.stdout + r.stderr).strip().splitlines()[0] if (r.stdout+r.stderr).strip() else "")); fails += 1
        continue
    run = subprocess.run([d + "/out"], capture_output=True, text=True)
    got = run.stdout.strip()
    if nlang == '' and nbody.strip() and not nbody.startswith('error:'):
        want = nbody.strip()
        if got != want:
            print("FAIL %d: output differs\n   want %r\n   got  %r" % (n, want, got)); fails += 1
        else:
            print("ok   %d  -> %s" % (n, got.replace("\n", " | ")))
    else:
        print("ok   %d  (built; no output claimed)" % n)
shutil.rmtree(work)
print("\n%d snippets, %d failures" % (len(pairs), fails))
sys.exit(1 if fails else 0)
