#!/usr/bin/env python3
import argparse
import json
import os
import re
import shutil
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
parser.add_argument("--timeout", type=int, default=240)
parser.add_argument("--filter", default="")
parser.add_argument("--stop-on-fail", action="store_true")
parser.add_argument("--keep", action="store_true", help="keep the temp dir of every failure")
parser.add_argument("--out", help="write the results as JSON to this path")
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
    path = root / name
    expected = content if isinstance(content, bytes) else content.encode()
    return path.exists() and path.read_bytes() == expected


def numbered(start, stop, template="item {}\n"):
    return "".join(template.format(i) for i in range(start, stop))


def tree(root):
    return sorted(
        (str(p.relative_to(root)), p.read_bytes())
        for p in root.rglob("*")
        if p.is_file()
    )


def service_files(version):
    return {
        f"svc{i:02}/config.ini": f"version = {version}\nname = svc{i:02}\n"
        for i in range(1, 21)
    }


def usage_files(symbol):
    return {
        "main.py": f"from lib import {symbol}\n\nprint({symbol}(2, 3))\n",
        **{
            f"lib{i}.py": f"def {symbol}(a, b):\n    return a + b + {i}\n"
            for i in range(1, 4)
        },
        "notes.txt": "do not touch\n",
    }


def refactor_files(call):
    return {
        "api.py": "def fetch(url):\n    return url\n",
        **{
            f"mod{i:02}.py": (
                "from api import fetch\n"
                "\n"
                "def run():\n"
                f"    return fetch('https://example.com/{i}', {call})\n"
            )
            for i in range(20)
        },
        "check.py": "import importlib\n\n"
        + "".join(
            f"assert importlib.import_module('mod{i:02}').run() == 'https://example.com/{i}'\n"
            for i in range(20)
        ),
    }


def deep_needle():
    lines = [f"row {i}\n" for i in range(1, 20001)]
    lines[17341] = "value = 1\n"
    return "".join(lines)


def tail(text, lines=40):
    return "\n".join(text.splitlines()[-lines:])


def workdirs():
    parent = Path(tempfile.mkdtemp(prefix="agent-eval-"))
    root = parent / "work"
    root.mkdir()
    return parent, root


tasks = [
    {
        "name": "create-file",
        "files": {},
        "prompt": "Create hello.txt containing exactly hello followed by a newline.",
        "check": lambda root: exact(root, "hello.txt", "hello\n"),
    },
    {
        "name": "edit-file",
        "files": {"config.txt": "port=3000\nhost=localhost\n"},
        "prompt": "Change only the port in config.txt from 3000 to 8080.",
        "check": lambda root: exact(root, "config.txt", "port=8080\nhost=localhost\n"),
    },
    {
        "name": "edit-duplicate-needle",
        "files": {"dup.txt": "port = 3000\nhost = a\n\nport = 3000\nhost = b\n"},
        "prompt": "In dup.txt, change only the second 'port = 3000' line to 'port = 8080'. "
        "The first one must stay unchanged.",
        "check": lambda root: exact(
            root, "dup.txt", "port = 3000\nhost = a\n\nport = 8080\nhost = b\n"
        ),
    },
    {
        "name": "parallel-writes",
        "files": {},
        "prompt": "Create a.txt containing alpha, b.txt containing beta, and c.txt containing "
        "gamma. Each followed by a newline.",
        "check": lambda root: (
            exact(root, "a.txt", "alpha\n")
            and exact(root, "b.txt", "beta\n")
            and exact(root, "c.txt", "gamma\n")
        ),
    },
    {
        "name": "multi-file-edit",
        "files": {"a.ini": "version = 1\nname = x\n", "b.ini": "version = 1\nname = y\n"},
        "prompt": "Bump the version to 2 in both a.ini and b.ini. Change nothing else.",
        "check": lambda root: (
            exact(root, "a.ini", "version = 2\nname = x\n")
            and exact(root, "b.ini", "version = 2\nname = y\n")
        ),
    },
    {
        "name": "edit-nbsp-line",
        "files": {"data.txt": "hello\u00a0world\nsecond line\n"},
        "prompt": "In data.txt, replace the first line with exactly 'goodbye world' as plain "
        "ASCII text. Leave the second line unchanged.",
        "check": lambda root: exact(root, "data.txt", "goodbye world\nsecond line\n"),
    },
    {
        "name": "edit-large-block",
        "files": {"gendata.txt": numbered(1, 61)},
        "prompt": "In gendata.txt, replace the contiguous block from 'item 20' through "
        "'item 40' inclusive with a single line containing exactly collapsed. "
        "Change nothing else.",
        "check": lambda root: exact(
            root, "gendata.txt", numbered(1, 20) + "collapsed\n" + numbered(41, 61)
        ),
    },
    {
        "name": "write-subdir",
        "files": {},
        "prompt": "Create logs/app.log containing exactly ready followed by a newline.",
        "check": lambda root: exact(root, "logs/app.log", "ready\n"),
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
        ).returncode
        == 0
        and exact(root, "test_arithmetic.py", "from arithmetic import add\n\nassert add(2, 3) == 5\n"),
    },
    {
        "name": "recover-tool-error",
        "files": {"note.txt": "old\n"},
        "prompt": "Read missing.txt. If it does not exist, update note.txt to contain exactly "
        "recovered followed by a newline.",
        "check": lambda root: exact(root, "note.txt", "recovered\n"),
    },
    {
        "name": "bash-timeout",
        "files": {},
        "prompt": "Run `sleep 30` with the bash tool and a 1 second timeout. It will be killed. "
        "Afterwards create done.txt containing exactly timed-out followed by a newline.",
        "check": lambda root: exact(root, "done.txt", "timed-out\n"),
    },
    {
        "name": "read-offset",
        "files": {"big.log": numbered(1, 2001, "line {}\n")},
        "prompt": "big.log has 2000 numbered lines. Use read with an offset to find the text of "
        "the last line, then create last.txt containing exactly that text followed by a newline.",
        "check": lambda root: exact(root, "last.txt", "line 2000\n"),
    },
    {
        "name": "many-file-edit",
        "files": service_files(1),
        "prompt": "Every svc*/config.ini file has version = 1. Set the version to 2 in all "
        "twenty of them. Change nothing else.",
        "check": lambda root: all(
            exact(root, name, content) for name, content in service_files(2).items()
        ),
    },
    {
        "name": "needle-deep",
        "files": {"deep.txt": deep_needle()},
        "prompt": "deep.txt has 20000 lines. Change the single line that reads 'value = 1' to "
        "'value = 2'. Every other line, including the total count, must stay identical.",
        "check": lambda root: exact(
            root, "deep.txt", deep_needle().replace("value = 1\n", "value = 2\n")
        ),
    },
    {
        "name": "bash-big-output",
        "files": {},
        "prompt": "Use the bash tool to run `seq 1 200000`. Then create last.txt containing "
        "exactly the final number of that output followed by a newline.",
        "check": lambda root: exact(root, "last.txt", "200000\n"),
    },
    {
        "name": "two-bugs",
        "files": {
            "stats.py": (
                "def mean(values):\n"
                "    return sum(values) / len(values) + 1\n"
                "\n"
                "def spread(values):\n"
                "    return max(values) - min(values) * 2\n"
            ),
            "test_stats.py": (
                "from stats import mean, spread\n"
                "\n"
                "assert mean([2, 4, 6]) == 4\n"
                "assert spread([3, 10]) == 7\n"
            ),
        },
        "prompt": "The test fails twice. Run it, fix stats.py so it passes, and leave "
        "test_stats.py untouched.",
        "check": lambda root: subprocess.run(
            [sys.executable, "test_stats.py"], cwd=root, capture_output=True
        ).returncode
        == 0
        and exact(
            root,
            "test_stats.py",
            "from stats import mean, spread\n\nassert mean([2, 4, 6]) == 4\nassert spread([3, 10]) == 7\n",
        )
        and "len(values) + 1" not in (root / "stats.py").read_text()
        and "min(values) * 2" not in (root / "stats.py").read_text(),
    },
    {
        "name": "rename-symbol",
        "files": usage_files("compute"),
        "prompt": "Rename the function compute to calculate in every file that defines or "
        "imports it. Leave notes.txt alone.",
        "check": lambda root: all(
            exact(root, name, content) for name, content in usage_files("calculate").items()
        ),
    },
    {
        "name": "crlf-edit",
        "files": {"win.txt": "port = 3000\r\nhost = localhost\r\n"},
        "prompt": "In win.txt change only 3000 to 8080. Keep the file's existing line endings "
        "exactly as they are.",
        "check": lambda root: exact(root, "win.txt", "port = 8080\r\nhost = localhost\r\n"),
    },
    {
        "name": "trailing-blank-lines",
        "files": {"tail.txt": "first\nsecond\n\n\n"},
        "prompt": "In tail.txt change only the first line to 'FIRST'. The rest of the file must "
        "stay byte for byte identical.",
        "check": lambda root: exact(root, "tail.txt", "FIRST\nsecond\n\n\n"),
    },
    {
        "name": "ten-file-sum",
        "files": {f"num{i}.txt": f"{i * 7}\n" for i in range(1, 11)},
        "prompt": "Every num*.txt file holds one number. Create total.txt containing exactly "
        "the sum of those ten numbers followed by a newline.",
        "check": lambda root: exact(root, "total.txt", f"{sum(i * 7 for i in range(1, 11))}\n"),
    },
    {
        "name": "deep-nested-create",
        "files": {},
        "prompt": "Create a/b/c/d/e/note.txt containing exactly nested followed by a newline.",
        "check": lambda root: exact(root, "a/b/c/d/e/note.txt", "nested\n"),
    },
    {
        "name": "error-chain",
        "files": {"note.txt": "old\n"},
        "prompt": "Try to read missing1.txt, then try to read missing2.txt, then update note.txt "
        "to contain exactly recovered followed by a newline. Both reads fail on purpose.",
        "check": lambda root: exact(root, "note.txt", "recovered\n"),
    },
    {
        "name": "read-only",
        "files": {"alpha.txt": "a\n", "beta.txt": "b\n"},
        "prompt": "List the files in the current directory and report which ones exist. Do not "
        "create, change, or delete anything.",
        "check": lambda root: tree(root)
        == [("alpha.txt", b"a\n"), ("beta.txt", b"b\n")],
    },
    {
        "name": "edit-nth-needle",
        "files": {
            "many.txt": "".join(
                f"port = 3000\nhost = h{i}\n\n" for i in range(1, 6)
            )
        },
        "prompt": "many.txt has five identical 'port = 3000' lines. Change only the fourth one "
        "to 'port = 8080'. The other four must stay unchanged.",
        "check": lambda root: exact(
            root,
            "many.txt",
            "port = 3000\nhost = h1\n\n"
            "port = 3000\nhost = h2\n\n"
            "port = 3000\nhost = h3\n\n"
            "port = 8080\nhost = h4\n\n"
            "port = 3000\nhost = h5\n\n",
        ),
    },
    {
        "name": "whitespace-drift",
        "files": {"tabs.py": "def f():\n\tvalue = 1\n\treturn value\n"},
        "prompt": "In tabs.py replace the block\n\ndef f():\n    value = 1\n    return value\n\n"
        "with the same block but value = 2. The real file does not use the indentation I typed, "
        "so keep its own indentation.",
        "check": lambda root: exact(
            root, "tabs.py", "def f():\n\tvalue = 2\n\treturn value\n"
        ),
    },
    {
        "name": "path-with-spaces",
        "files": {},
        "prompt": "Create the file 'my docs/notes file.txt' containing exactly ok followed by a "
        "newline.",
        "check": lambda root: exact(root, "my docs/notes file.txt", "ok\n"),
    },
    {
        "name": "empty-file-edit",
        "files": {"empty.txt": ""},
        "prompt": "empty.txt is empty. Append the single line hello to it.",
        "check": lambda root: exact(root, "empty.txt", "hello\n"),
    },
    {
        "name": "latin1-bytes",
        "files": {"latin1.txt": "caf\xe9 = 1\nkeep = 2\n".encode("latin-1")},
        "prompt": "latin1.txt holds latin-1 bytes, not UTF-8. Change 2 to 3 on the second line "
        "and leave every other byte identical.",
        "check": lambda root: exact(
            root, "latin1.txt", "caf\xe9 = 1\nkeep = 3\n".encode("latin-1")
        ),
    },
    {
        "name": "single-line-huge",
        "files": {"long.txt": "data=" + "a" * 200000 + "&port=3000&host=localhost\n"},
        "prompt": "long.txt is one huge line. Change only port=3000 to port=8080 and leave the "
        "rest of the line byte for byte identical.",
        "check": lambda root: exact(
            root, "long.txt", "data=" + "a" * 200000 + "&port=8080&host=localhost\n"
        ),
    },
    {
        "name": "symlink-preserved",
        "files": {"real.txt": "value = 1\n"},
        "setup": lambda root: (root / "link.txt").symlink_to("real.txt"),
        "prompt": "link.txt is a symlink to real.txt. Change the value to 2 through link.txt and "
        "keep link.txt a symlink.",
        "check": lambda root: exact(root, "real.txt", "value = 2\n")
        and (root / "link.txt").is_symlink(),
    },
    {
        "name": "stderr-only",
        "files": {"noisy.sh": "#!/bin/sh\necho 'hidden code 42' >&2\n"},
        "prompt": "Run `sh noisy.sh`. It prints only to stderr. Create out.txt containing exactly "
        "the message it printed followed by a newline.",
        "check": lambda root: exact(root, "out.txt", "hidden code 42\n"),
    },
    {
        "name": "hundred-files",
        "files": {f"many/f{i:03}.txt": f"{i}\n" for i in range(100)},
        "prompt": "many/ holds 100 files named f000.txt through f099.txt. Replace each file's "
        "content with its own number followed by ' ok' and a newline, so f007.txt contains "
        "exactly '7 ok' plus a newline. Change nothing else.",
        "check": lambda root: all(
            exact(root, f"many/f{i:03}.txt", f"{i} ok\n") for i in range(100)
        ),
    },
    {
        "name": "wide-refactor",
        "files": refactor_files(30),
        "prompt": "api.py defines fetch(url). Change it to fetch(url, timeout) with no default, "
        "and update all twenty mod*.py call sites to pass 30 as the timeout. check.py verifies the "
        "result: run it and keep it unchanged.",
        "check": lambda root: subprocess.run(
            [sys.executable, "check.py"], cwd=root, capture_output=True
        ).returncode
        == 0
        and exact(
            root,
            "api.py",
            "def fetch(url, timeout):\n    return url\n",
        )
        and exact(root, "check.py", refactor_files(30)["check.py"]),
    },
    {
        "name": "six-steps",
        "files": {},
        "prompt": "Follow these steps in order: 1) create data/a.txt with the lines one, two, "
        "three, 2) append a line four, 3) delete the line two, 4) write the number of lines left "
        "in a.txt to data/count.txt, 5) copy a.txt to data/b.txt, 6) delete data/a.txt. Nothing "
        "else may remain.",
        "check": lambda root: tree(root)
        == [("data/b.txt", b"one\nthree\nfour\n"), ("data/count.txt", b"3\n")],
    },
    {
        "name": "long-steps",
        "files": {},
        "prompt": "Do these steps in order: 1) create task.txt containing 'step one' plus a "
        "newline, 2) append a line 'step two' to it, 3) count its lines and write that number "
        "alone plus a newline to count.txt, 4) delete task.txt. Nothing else may remain.",
        "check": lambda root: tree(root) == [("count.txt", b"2\n")],
    },
]

if args.filter:
    tasks = [task for task in tasks if re.search(args.filter, task["name"])]
    if not tasks:
        sys.exit(f"no task matches {args.filter!r}")

agents = ("axe", "pi") if args.agent == "both" else (args.agent,)
AXE_TOTALS = re.compile(
    r"run done elapsed_ms=\d+ turns=(\d+) input_tokens=(\d+) cached_input_tokens=(\d+) "
    r"output_tokens=(\d+)"
)
AXE_START = re.compile(r"run start messages=\d+ system_bytes=(\d+) tools=(\d+)")
stats = {
    agent: {
        task["name"]: {
            "passes": 0,
            "seconds": [],
            "fresh": 0,
            "cached": 0,
            "output": 0,
            "turns": 0,
            "system": 0,
            "tools": 0,
        }
        for task in tasks
    }
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
        "--mode",
        "json",
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


def axe_usage(stderr):
    empty = {"fresh": 0, "cached": 0, "output": 0, "turns": 0, "system": 0, "tools": 0}
    found = AXE_TOTALS.findall(stderr)
    if not found:
        return empty
    turns, sent, cached, received = (int(value) for value in found[-1])
    start = AXE_START.findall(stderr)
    system, tools = (int(value) for value in start[-1]) if start else (0, 0)
    return {
        "fresh": sent - cached,
        "cached": cached,
        "output": received,
        "turns": turns,
        "system": system,
        "tools": tools,
    }


def pi_usage(stdout):
    usage = {"fresh": 0, "cached": 0, "output": 0, "turns": 0, "system": 0, "tools": 0}
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "message_end":
            message = event["message"]
            if message["role"] == "system":
                usage["system"] = len(json.dumps(message.get("sections")))
                usage["tools"] = len(message.get("toolsAdded") or [])
            continue
        if event.get("type") != "turn_end":
            continue
        turn = event["message"].get("usage") or {}
        usage["turns"] += 1
        usage["fresh"] += turn.get("input") or 0
        usage["cached"] += turn.get("cacheRead") or 0
        usage["output"] += turn.get("output") or 0
    return usage


def usage(agent, result):
    if result is None:
        return {"fresh": 0, "cached": 0, "output": 0, "turns": 0, "system": 0, "tools": 0}
    if agent == "axe":
        return axe_usage(result.stderr)
    return pi_usage(result.stdout)


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
            parent, root = workdirs()
            for name, content in task["files"].items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                if isinstance(content, bytes):
                    path.write_bytes(content)
                else:
                    path.write_text(content, newline="")
            if "setup" in task:
                task["setup"](root)
            env = os.environ.copy()
            env["OPENAI_API_KEY"] = api_key
            env["DEEPSEEK_API_KEY"] = api_key
            env["AXE_TRACE"] = "1"
            env["HOME"] = str(parent / "home")
            env["XDG_CONFIG_HOME"] = str(parent / "config")
            env["PI_CODING_AGENT_DIR"] = str(parent / "pi")
            for name in ("home", "config", "pi"):
                (parent / name).mkdir(exist_ok=True)
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
            passed = (
                result is not None
                and result.returncode == 0
                and task["check"](root)
            )
            spent = usage(agent, result)
            row = stats[agent][task["name"]]
            row["seconds"].append(elapsed)
            for key in ("fresh", "cached", "output", "turns"):
                row[key] += spent[key]
            for key in ("system", "tools"):
                row[key] = max(row[key], spent[key])
            if passed:
                row["passes"] += 1
            else:
                failed.append(f"{agent} {task['name']} run {run + 1}")
                stop = args.stop_on_fail
            label = (
                f"{agent:4} {task['name']:22} run {run + 1} "
                f"({elapsed:5.1f}s "
                f"{spent['fresh']:6d} fresh {spent['cached']:6d} cached "
                f"{spent['output']:5d} out {spent['turns']:2d} turns)"
            )
            print(f"{'PASS' if passed else 'FAIL'} {label}", flush=True)
            if passed or not args.keep:
                shutil.rmtree(parent)
            if not passed:
                if args.keep:
                    print(f"kept {root}")
                if result is None:
                    print(f"timeout after {args.timeout}s")
                else:
                    print(tail(result.stdout))
                    print(tail(result.stderr), file=sys.stderr)
            if stop:
                break

print()
columns = ("pass", "s", "in", "cache", "out", "turns")
widths = {"pass": 7, "s": 8, "in": 8, "cache": 8, "out": 8, "turns": 7}


def cell(agent, values):
    return "".join(
        f"{values[column]:>{widths[column] + len(agent) + 1}}" for column in columns
    )


header = f"{'task':24}"
for agent in agents:
    header += "".join(
        f"{agent + ' ' + column:>{widths[column] + len(agent) + 1}}" for column in columns
    )
print(header)
for task in tasks:
    line = f"{task['name']:24}"
    for agent in agents:
        row = stats[agent][task["name"]]
        line += cell(
            agent,
            {
                "pass": f"{row['passes']}/{args.runs}",
                "s": f"{sum(row['seconds']) / args.runs:.1f}",
                "in": row["fresh"] // args.runs,
                "cache": row["cached"] // args.runs,
                "out": row["output"] // args.runs,
                "turns": f"{row['turns'] / args.runs:.1f}",
            }
        )
    print(line)
print()
for agent in agents:
    rows = stats[agent].values()
    passes = sum(row["passes"] for row in rows)
    seconds = sum(sum(row["seconds"]) for row in rows)
    fresh = sum(row["fresh"] for row in rows) // args.runs
    cached = sum(row["cached"] for row in rows) // args.runs
    output = sum(row["output"] for row in rows) // args.runs
    turns = sum(row["turns"] for row in rows) / args.runs
    system = max(row["system"] for row in rows)
    tools = max(row["tools"] for row in rows)
    print(
        f"{agent}: {passes}/{len(tasks) * args.runs} passed, {seconds:.1f}s total, "
        f"{fresh} in, {cached} cache, {output} out, {turns:.0f} turns, "
        f"{system} system bytes, {tools} tools"
    )
if args.out:
    report = {
        "model": model,
        "base": base,
        "runs": args.runs,
        "tasks": [task["name"] for task in tasks],
        "agents": {
            agent: {
                "passes": sum(row["passes"] for row in stats[agent].values()),
                "seconds": round(
                    sum(sum(row["seconds"]) for row in stats[agent].values()), 1
                ),
                "tasks": stats[agent],
            }
            for agent in agents
        },
    }
    Path(args.out).write_text(json.dumps(report, indent=2) + "\n")

if failed:
    sys.exit(f"{len(failed)} evals failed: {', '.join(failed)}")
