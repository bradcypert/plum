#!/usr/bin/env python3
"""Bound formatter allocation growth; optionally compare source-built compilers."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("compiler", type=Path)
parser.add_argument("--before", type=Path)
parser.add_argument("--out", type=Path)
args = parser.parse_args()
compiler = args.compiler.resolve()
before = args.before.resolve() if args.before else None
GROWTH_LIMIT = 2.6


def statements(count, formatted):
    expected = "let dense (): Int = {\n" + "    let x = 1;\n" * count + "    x\n}\n"
    source = expected if formatted else "let dense (): Int = { " + "let x = 1; " * count + "x }\n"
    return source, expected


def definitions(count):
    source = " ".join(f"let f{i} (): Int = {{ let x = 1; x }}" for i in range(count)) + "\n"
    expected = "\n\n".join(f"let f{i} (): Int = {{\n    let x = 1;\n    x\n}}" for i in range(count)) + "\n"
    return source, expected


def measure(binary, path, expected):
    result = subprocess.run([str(binary), "fmt", str(path)], capture_output=True,
                            env={**os.environ, "PLUM_RT_STATS": "1"}, timeout=30)
    assert result.returncode == 0, result.stdout + result.stderr
    assert result.stdout == expected.encode(), "scaling fixture formatted incorrectly"
    stats = re.search(rb"alloc n=(\d+) bytes=(\d+)", result.stderr)
    assert stats, "formatter did not report allocation volume"
    return {"allocations": int(stats[1]), "allocated_bytes": int(stats[2])}


cases = [
    ("dense_statements", [1000, 2000, 4000], lambda n: statements(n, False), 96_000_000),
    ("formatted_statements", [1000, 2000, 4000], lambda n: statements(n, True), 96_000_000),
    ("many_definitions", [250, 500, 1000], definitions, 96_000_000),
]
report = {
    "metric": "Cumulative requested Plum-cell allocation bytes, not peak/live memory or native buffers",
    "method": "One independent CLI process per input and compiler; identical expected output required",
    "platform": {"system": platform.system(), "machine": platform.machine()},
    "after_compiler_sha256": hashlib.sha256(compiler.read_bytes()).hexdigest(),
    "growth_limit": GROWTH_LIMIT,
    "cases": {},
}
if before:
    report["before_compiler_sha256"] = hashlib.sha256(before.read_bytes()).hexdigest()

with tempfile.TemporaryDirectory(prefix="plum-format-scaling-") as temp:
    path = Path(temp) / "main.plum"
    for name, sizes, fixture, budget in cases:
        rows = []
        for count in sizes:
            source, expected = fixture(count)
            path.write_bytes(source.encode())
            row = {"count": count, "input_bytes": len(source.encode()),
                   "output_bytes": len(expected.encode()),
                   "input_sha256": hashlib.sha256(source.encode()).hexdigest(),
                   "output_sha256": hashlib.sha256(expected.encode()).hexdigest()}
            if before:
                row["before"] = measure(before, path, expected)
            row["after"] = measure(compiler, path, expected)
            rows.append(row)
            print(f"formatter-scaling: {name} {count}: {row['after']['allocated_bytes']} bytes", flush=True)
        allocated = [row["after"]["allocated_bytes"] for row in rows]
        # Permit setup costs and platform variation, but reject the roughly
        # fourfold growth caused by repeatedly copying a growing prefix.
        for first, second in zip(allocated, allocated[1:]):
            assert second <= first * GROWTH_LIMIT, f"{name}: allocation grew {second / first:.2f}x when input doubled"
        assert allocated[-1] <= budget, f"{name}: {allocated[-1]} allocated bytes exceeds {budget}"
        report["cases"][name] = {"largest_input_budget": budget, "samples": rows}

if args.out:
    args.out.write_text(json.dumps(report, indent=2) + "\n")
print("formatter-scaling: allocation growth and volume stay within budgets")
