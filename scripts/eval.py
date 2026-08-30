#!/usr/bin/env python3
import argparse
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--bin", default="target/release/axe")
parser.add_argument("--model", default=os.environ.get("AXE_EVAL_MODEL", "gpt-4.1-mini"))
parser.add_argument("--base", default=os.environ.get("AXE_EVAL_BASE", "https://api.openai.com/v1"))
parser.add_argument("--runs", type=int, default=1)
parser.add_argument("--timeout", type=int, default=180)
parser.add_argument("--stop-on-fail", action="store_true")
args = parser.parse_args()
binary = str(Path(args.bin).resolve())

if not os.environ.get("OPENAI_API_KEY"):
    sys.exit("OPENAI_API_KEY is required")


def exact(root, name, content):
    return (root / name).read_text() == content


tasks = [
    {
        "name": "create-file",
        "files": {},
        "prompt": "Create hello.txt containing exactly hello followed by a newline.",
        "check": lambda root, r: exact(root, "hello.txt", "hello\n"),
    },
    {
        "name": "edit-file",
        "files": {"config.txt": "port=3000\nhost=localhost\n"},
        "prompt": "Change only the port in config.txt from 3000 to 8080.",
        "check": lambda root, r: exact(root, "config.txt", "port=8080\nhost=localhost\n"),
    },
    {
        "name": "edit-duplicate-needle",
        "files": {"dup.txt": "port = 3000\nhost = a\n\nport = 3000\nhost = b\n"},
        "prompt": "In dup.txt, change only the second 'port = 3000' line to 'port = 8080'. "
        "The first one must stay unchanged.",
        "check": lambda root, r: exact(
            root, "dup.txt", "port = 3000\nhost = a\n\nport = 8080\nhost = b\n"
        ),
    },
    {
        "name": "parallel-writes",
        "files": {},
        "prompt": "Create a.txt containing alpha, b.txt containing beta, and c.txt containing "
        "gamma. Each followed by a newline.",
        "check": lambda root, r: (
            exact(root, "a.txt", "alpha\n")
            and exact(root, "b.txt", "beta\n")
            and exact(root, "c.txt", "gamma\n")
        ),
    },
    {
        "name": "multi-file-edit",
        "files": {"a.ini": "version = 1\nname = x\n", "b.ini": "version = 1\nname = y\n"},
        "prompt": "Bump the version to 2 in both a.ini and b.ini. Change nothing else.",
        "check": lambda root, r: (
            exact(root, "a.ini", "version = 2\nname = x\n")
            and exact(root, "b.ini", "version = 2\nname = y\n")
        ),
    },
    {
        "name": "write-subdir",
        "files": {},
        "prompt": "Create logs/app.log containing exactly ready followed by a newline.",
        "check": lambda root, r: exact(root, "logs/app.log", "ready\n"),
    },
    {
        "name": "fix-test",
        "files": {
            "arithmetic.py": "def add(a, b):\n    return a - b\n",
            "test_arithmetic.py": "from arithmetic import add\n\nassert add(2, 3) == 5\n",
        },
        "prompt": "Run the test and fix the implementation. Do not change the test.",
        "check": lambda root, r: subprocess.run(
            [sys.executable, "test_arithmetic.py"], cwd=root, capture_output=True
        ).returncode
        == 0
        and exact(root, "test_arithmetic.py", "from arithmetic import add\n\nassert add(2, 3) == 5\n"),
    },
    {
        "name": "recover-tool-error",
        "files": {"note.txt": "old\n"},
        "prompt": "Read missing.txt. If it does not exist, update note.txt to contain exactly "
        "recovered followed by a newline.",
        "check": lambda root, r: exact(root, "note.txt", "recovered\n"),
    },
    {
        "name": "bash-timeout",
        "files": {},
        "prompt": "Run `sleep 30` with the bash tool and a 1 second timeout. It will be killed. "
        "Afterwards create done.txt containing exactly timed-out followed by a newline.",
        "check": lambda root, r: (root / "done.txt").exists()
        and (root / "done.txt").read_text().strip() == "timed-out",
    },
    {
        "name": "read-offset",
        "files": {"big.log": "".join(f"line {i}\n" for i in range(1, 2001))},
        "prompt": "big.log has 2000 numbered lines. Use read with an offset to find the text of "
        "the last line, then create last.txt containing exactly that text followed by a newline.",
        "check": lambda root, r: exact(root, "last.txt", "line 2000\n"),
    },
]

stats = {t["name"]: {"passes": 0, "time": 0.0} for t in tasks}
failed = []
stop = False
for run in range(args.runs):
    if stop:
        break
    for task in tasks:
        with tempfile.TemporaryDirectory(prefix="axe-eval-") as tmp:
            root = Path(tmp)
            for name, content in task["files"].items():
                (root / name).write_text(content)
            start = time.monotonic()
            try:
                result = subprocess.run(
                    [
                        binary,
                        "--base",
                        args.base,
                        "--model",
                        args.model,
                        "-C",
                        str(root),
                        task["prompt"],
                    ],
                    text=True,
                    capture_output=True,
                    timeout=args.timeout,
                )
            except subprocess.TimeoutExpired:
                result = None
            elapsed = time.monotonic() - start
            passed = result is not None and result.returncode == 0 and task["check"](root, result)
            stats[task["name"]]["time"] += elapsed
            if passed:
                stats[task["name"]]["passes"] += 1
            else:
                failed.append(f"{task['name']} run {run + 1}")
                stop = args.stop_on_fail
            label = f"{task['name']} run {run + 1} ({elapsed:.1f}s)"
            print(f"{'PASS' if passed else 'FAIL'} {label}", flush=True)
            if not passed:
                if result is None:
                    print(f"timeout after {args.timeout}s")
                else:
                    print(result.stdout)
                    print(result.stderr, file=sys.stderr)
                if stop:
                    break

total = sum(s["passes"] for s in stats.values())
runs_done = max(args.runs - (1 if stop else 0), 1) if stop else args.runs
print()
for t in tasks:
    s = stats[t["name"]]
    n = runs_done if stop else args.runs
    print(f"{t['name']:24} {s['passes']}/{n}  avg {s['time'] / n:.1f}s")
print(f"\n{total}/{len(tasks) * args.runs} passed")
if failed:
    sys.exit(f"{len(failed)} evals failed: {', '.join(failed)}")
