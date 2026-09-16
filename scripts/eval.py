#!/usr/bin/env python3
import argparse
import os
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--bin", default="target/release/axe")
parser.add_argument("--pi-bin", default="pi")
parser.add_argument("--agent", choices=("axe", "pi", "both"), default="both")
parser.add_argument("--config", default="~/.config/axe/config")
parser.add_argument("--model")
parser.add_argument("--base")
parser.add_argument("--runs", type=int, default=1)
parser.add_argument("--timeout", type=int, default=180)
parser.add_argument("--stop-on-fail", action="store_true")
args = parser.parse_args()
axe_binary = str(Path(args.bin).resolve())
config_path = Path(args.config).expanduser()
config = tomllib.loads(config_path.read_text()) if config_path.exists() else {}
api_key = os.environ.get("OPENAI_API_KEY") or config.get("api_key")
model = args.model or config.get("model")
base = args.base or config.get("base")

if not api_key:
    sys.exit("an API key is required in OPENAI_API_KEY or the Axe config")
if not model:
    sys.exit("a model is required with --model or in the Axe config")


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
        "name": "edit-nbsp-line",
        "files": {"data.txt": "hello\u00a0world\nsecond line\n"},
        "prompt": "In data.txt, replace the first line with exactly 'goodbye world' as plain "
        "ASCII text. Leave the second line unchanged.",
        "check": lambda root, r: exact(root, "data.txt", "goodbye world\nsecond line\n"),
    },
    {
        "name": "edit-large-block",
        "files": {"gendata.txt": "".join(f"item {i}\n" for i in range(1, 61))},
        "prompt": "In gendata.txt, replace the contiguous block from 'item 20' through "
        "'item 40' inclusive with a single line containing exactly collapsed. "
        "Change nothing else.",
        "check": lambda root, r: exact(
            root,
            "gendata.txt",
            "".join(f"item {i}\n" for i in range(1, 20))
            + "collapsed\n"
            + "".join(f"item {i}\n" for i in range(41, 61)),
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

agents = ("axe", "pi") if args.agent == "both" else (args.agent,)
stats = {
    agent: {task["name"]: {"passes": 0, "time": 0.0} for task in tasks}
    for agent in agents
}
failed = []
stop = False


def command(agent, root, prompt):
    if agent == "axe":
        cmd = [axe_binary]
        if base:
            cmd += ["--base", base]
        cmd += ["--model", model, "-C", str(root), prompt]
        return cmd
    return [
        args.pi_bin,
        "--provider",
        "deepseek",
        "--model",
        model,
        "--print",
        "--no-session",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
        "--offline",
        "--tools",
        "read,write,edit,bash",
        "--",
        prompt,
    ]


for run in range(args.runs):
    if stop:
        break
    for task_index, task in enumerate(tasks):
        if stop:
            break
        agent_order = agents
        if len(agents) == 2 and (run + task_index) % 2:
            agent_order = tuple(reversed(agents))
        for agent in agent_order:
            with tempfile.TemporaryDirectory(prefix=f"{agent}-eval-") as tmp:
                root = Path(tmp)
                for name, content in task["files"].items():
                    path = root / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(content)
                env = os.environ.copy()
                env["OPENAI_API_KEY"] = api_key
                env["DEEPSEEK_API_KEY"] = api_key
                env["XDG_CONFIG_HOME"] = str(root / ".config")
                env["PI_CODING_AGENT_DIR"] = str(root / ".pi")
                start = time.monotonic()
                try:
                    result = subprocess.run(
                        command(agent, root, task["prompt"]),
                        cwd=root,
                        env=env,
                        text=True,
                        capture_output=True,
                        timeout=args.timeout,
                    )
                except subprocess.TimeoutExpired:
                    result = None
                elapsed = time.monotonic() - start
                passed = result is not None and result.returncode == 0 and task["check"](root, result)
                stats[agent][task["name"]]["time"] += elapsed
                if passed:
                    stats[agent][task["name"]]["passes"] += 1
                else:
                    failed.append(f"{agent} {task['name']} run {run + 1}")
                    stop = args.stop_on_fail
                label = f"{agent:4} {task['name']} run {run + 1} ({elapsed:.1f}s)"
                print(f"{'PASS' if passed else 'FAIL'} {label}", flush=True)
                if not passed:
                    if result is None:
                        print(f"timeout after {args.timeout}s")
                    else:
                        print(result.stdout)
                        print(result.stderr, file=sys.stderr)
                if stop:
                    break

print()
for task in tasks:
    row = [f"{task['name']:24}"]
    for agent in agents:
        result = stats[agent][task["name"]]
        row.append(
            f"{agent} {result['passes']}/{args.runs} avg {result['time'] / args.runs:.1f}s"
        )
    print("  ".join(row))
print()
for agent in agents:
    passes = sum(result["passes"] for result in stats[agent].values())
    elapsed = sum(result["time"] for result in stats[agent].values())
    print(f"{agent}: {passes}/{len(tasks) * args.runs} passed, {elapsed:.1f}s total")
if failed:
    sys.exit(f"{len(failed)} evals failed: {', '.join(failed)}")
