#!/usr/bin/env python3
import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--bin", default="target/release/axe")
parser.add_argument("--model", default=os.environ.get("AXE_EVAL_MODEL", "gpt-4.1-mini"))
parser.add_argument("--base", default=os.environ.get("AXE_EVAL_BASE", "https://api.openai.com/v1"))
parser.add_argument("--runs", type=int, default=1)
args = parser.parse_args()
binary = str(Path(args.bin).resolve())

if not os.environ.get("OPENAI_API_KEY"):
    sys.exit("OPENAI_API_KEY is required")

tasks = [
    {
        "name": "create-file",
        "files": {},
        "prompt": "Create hello.txt containing exactly hello followed by a newline.",
        "check": lambda root: (root / "hello.txt").read_text() == "hello\n",
    },
    {
        "name": "edit-file",
        "files": {"config.txt": "port=3000\nhost=localhost\n"},
        "prompt": "Change only the port in config.txt from 3000 to 8080.",
        "check": lambda root: (root / "config.txt").read_text() == "port=8080\nhost=localhost\n",
    },
    {
        "name": "fix-test",
        "files": {
            "arithmetic.py": "def add(a, b):\n    return a - b\n",
            "test_arithmetic.py": "from arithmetic import add\n\nassert add(2, 3) == 5\n",
        },
        "prompt": "Run the test and fix the implementation. Do not change the test.",
        "check": lambda root: subprocess.run(
            [sys.executable, "test_arithmetic.py"], cwd=root, capture_output=True
        ).returncode == 0
        and (root / "test_arithmetic.py").read_text()
        == "from arithmetic import add\n\nassert add(2, 3) == 5\n",
    },
    {
        "name": "recover-tool-error",
        "files": {"note.txt": "old\n"},
        "prompt": "Read missing.txt. If it does not exist, update note.txt to contain exactly recovered followed by a newline.",
        "check": lambda root: (root / "note.txt").read_text() == "recovered\n",
    },
]

failed = []
for run in range(args.runs):
    for task in tasks:
        with tempfile.TemporaryDirectory(prefix="axe-eval-") as tmp:
            root = Path(tmp)
            for name, content in task["files"].items():
                (root / name).write_text(content)
            result = subprocess.run(
                [binary, "--base", args.base, "--model", args.model, "-C", str(root), task["prompt"]],
                text=True,
                capture_output=True,
                timeout=180,
            )
            passed = result.returncode == 0 and task["check"](root)
            label = f"{task['name']} run {run + 1}"
            print(f"{'PASS' if passed else 'FAIL'} {label}")
            if not passed:
                failed.append(label)
                print(result.stdout)
                print(result.stderr, file=sys.stderr)

if failed:
    sys.exit(f"{len(failed)} evals failed: {', '.join(failed)}")
