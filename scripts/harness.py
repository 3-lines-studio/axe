#!/usr/bin/env python3
"""Black-box end-to-end harness for axe.

Drives the real binary the way a user would: a hermetic HOME and
XDG_CONFIG_HOME, a scripted mock OpenAI-compatible endpoint, one-shot CLI
runs, and a PTY session for the interactive TUI. Stdlib only.

Usage:
    python3 scripts/harness.py [--bin path/to/axe] [--filter name] [--list]

Every case gets a fresh temp HOME. Failures keep their temp dir and print
its path. The mock server answers /chat/completions from a scenario: the
Nth request gets scenario[N], the rest repeat the last entry. Each entry is
one of:

    {"status": 429, "body": "..."}                 error response
    {"stream": [chunk, ...]}                       SSE (when stream:true)
    {"body": {...}}                                plain JSON response

A chunk is a raw OpenAI SSE payload dict. Helpers: content_chunk, tool_chunk,
usage_chunk. The /models endpoint returns two fake models.
"""
import fcntl
import http.server
import json
import os
import pty
import re
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from contextlib import contextmanager
from pathlib import Path

ANSI_RE = re.compile(
    r"\x1b\[[0-9;:<=>?]*[ -/]*[@-~]"
    r"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"
    r"|\x1b[=>]"
    r"|\x1b"
)


def strip_ansi(b):
    if isinstance(b, bytes):
        b = b.decode("utf-8", "replace")
    return ANSI_RE.sub("", b)


def screen_text(data, cols, rows):
    """Replay the cursor-addressed TUI output onto a grid.

    The PTY stream only contains the cells ratatui changed, so searching it
    for a status line misses characters that happened to be unchanged from
    the previous frame. Search the replayed screen instead.
    """
    if isinstance(data, bytes):
        data = data.decode("utf-8", "replace")
    grid = [[" "] * cols for _ in range(rows)]
    row = col = 0
    saved = (0, 0)
    i = 0
    while i < len(data):
        ch = data[i]
        if ch == "\x1b":
            if data.startswith("\x1b[", i):
                j = i + 2
                while j < len(data) and not ("@" <= data[j] <= "~"):
                    j += 1
                if j >= len(data):
                    break
                seq, final = data[i + 2:j], data[j]
                body = seq[1:] if seq.startswith("?") else seq
                nums = [int(p) if p.isdigit() else 0 for p in body.split(";") if p]
                n = nums[0] if nums else 0
                if final in "Hf":
                    row = (nums[0] if nums else 1) - 1
                    col = (nums[1] if len(nums) > 1 else 1) - 1
                elif final == "A":
                    row -= max(n, 1)
                elif final == "B":
                    row += max(n, 1)
                elif final == "C":
                    col += max(n, 1)
                elif final == "D":
                    col -= max(n, 1)
                elif final == "G":
                    col = max(n, 1) - 1
                elif final == "d":
                    row = max(n, 1) - 1
                elif final == "J" and n in (2, 3):
                    grid = [[" "] * cols for _ in range(rows)]
                elif final == "J" and n == 0:
                    if 0 <= row < rows:
                        for x in range(max(col, 0), cols):
                            grid[row][x] = " "
                    for y in range(row + 1, rows):
                        grid[y] = [" "] * cols
                elif final == "K":
                    if 0 <= row < rows:
                        lo, hi = (0, cols) if n == 2 else (max(col, 0), cols) if n == 0 else (0, max(col + 1, 0))
                        for x in range(lo, min(hi, cols)):
                            grid[row][x] = " "
                elif final == "s":
                    saved = (row, col)
                elif final == "u":
                    row, col = saved
                i = j + 1
                continue
            if data.startswith("\x1b]", i):
                j = i + 2
                while j < len(data) and data[j] not in "\x07\x1b":
                    j += 1
                i = j + 1 if j < len(data) and data[j] == "\x07" else j + 2
                continue
            i += 2
            continue
        if ch == "\r":
            col = 0
        elif ch == "\n":
            row = min(row + 1, rows - 1)
        elif ch == "\b":
            col = max(col - 1, 0)
        elif ch == "\t":
            col = min(((col // 8) + 1) * 8, cols - 1)
        elif ch >= " ":
            import unicodedata

            width = 0 if unicodedata.combining(ch) else 2 if unicodedata.east_asian_width(ch) in "WF" else 1
            if 0 <= row < rows and 0 <= col < cols:
                grid[row][col] = ch
                if width == 2 and col + 1 < cols:
                    grid[row][col + 1] = ""
            col = min(col + width, cols - 1)
        i += 1
    row = max(0, min(row, rows - 1))
    col = max(0, min(col, cols - 1))
    if grid[row][col] == " ":
        grid[row][col] = ""
    return "\n".join("".join(line).rstrip() for line in grid)


def check(cond, msg):
    if not cond:
        raise AssertionError(msg)


# --- mock OpenAI-compatible endpoint -------------------------------------

def content_chunk(text, finish=None):
    return {"choices": [{"delta": {"content": text}, "finish_reason": finish}]}


def tool_chunk(name, args, cid="call_1", finish="tool_calls"):
    return {
        "choices": [
            {
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": cid,
                            "type": "function",
                            "function": {"name": name, "arguments": json.dumps(args)},
                        }
                    ]
                },
                "finish_reason": finish,
            }
        ]
    }


def usage_chunk(inp, out):
    return {"usage": {"prompt_tokens": inp, "completion_tokens": out}}


ANSWER_SCENARIO = [
    {"stream": [content_chunk("# Done\n\nThe **plan** worked.\n", "stop"), usage_chunk(20, 8)]}
]

TOOL_SCENARIO = [
    {
        "stream": [
            content_chunk("Let me check. "),
            tool_chunk("bash", {"command": "echo hello"}),
            usage_chunk(10, 5),
        ]
    },
    {"stream": [content_chunk("# Done\n\nThe **plan** worked.\n", "stop"), usage_chunk(20, 8)]},
]

WRITE_FILE_SCENARIO = [
    {"stream": [tool_chunk("bash", {"command": "printf hi > out.txt"}), usage_chunk(10, 5)]},
    {"stream": [content_chunk("file written.", "stop"), usage_chunk(20, 8)]},
]

WRITE_TOOL_SCENARIO = [
    {"stream": [tool_chunk("write", {"path": "out.txt", "content": "hi"}), usage_chunk(10, 5)]},
    {"stream": [content_chunk("file written.", "stop"), usage_chunk(20, 8)]},
]

REWIND_SCENARIO = [
    {"stream": [content_chunk("FIRST ANSWER", "stop"), usage_chunk(5, 3)]},
    {"stream": [content_chunk("SECOND ANSWER", "stop"), usage_chunk(5, 3)]},
]


class MockServer:
    def __init__(self, scenario, chunk_delay=0.01):
        self.scenario = scenario
        self.chunk_delay = chunk_delay
        self.requests = []
        self.lock = threading.Lock()
        self.httpd = None
        self.thread = None

    def start(self):
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()
        return self

    @property
    def base_url(self):
        return "http://127.0.0.1:%d" % self.httpd.server_address[1]

    def stop(self):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=5)

    def _handler(self):
        srv = self

        class H(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def log_message(self, *a):
                pass

            def do_GET(self):
                if self.path == "/models":
                    body = json.dumps({"data": [{"id": "model-a"}, {"id": "model-b"}]}).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                else:
                    self.send_response(404)
                    self.end_headers()

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                raw = self.rfile.read(length) if length else b""
                with srv.lock:
                    idx = len(srv.requests)
                    srv.requests.append(
                        {
                            "method": "POST",
                            "path": self.path,
                            "headers": dict(self.headers),
                            "raw": raw,
                            "json": None,
                        }
                    )
                try:
                    srv.requests[idx]["json"] = json.loads(raw or b"{}")
                except ValueError:
                    pass
                spec = srv.scenario[min(idx, len(srv.scenario) - 1)] if srv.scenario else {}
                status = spec.get("status", 200)
                if status != 200:
                    payload = spec.get("body", '{"error":{"message":"mock error"}}')
                    if isinstance(payload, str):
                        payload = payload.encode()
                    self.send_response(status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                req = srv.requests[idx]["json"] or {}
                if req.get("stream") and "stream" in spec:
                    chunks = [b"data: " + json.dumps(c).encode() + b"\n\n" for c in spec["stream"]]
                    chunks.append(b"data: [DONE]\n\n")
                    payload = b"".join(chunks)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    for c in chunks:
                        self.wfile.write(c)
                        self.wfile.flush()
                        if srv.chunk_delay:
                            time.sleep(srv.chunk_delay)
                else:
                    body = spec.get("body")
                    if isinstance(body, str):
                        body = json.loads(body)
                    if body is None:
                        body = {
                            "choices": [
                                {"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}
                            ],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
                        }
                    payload = json.dumps(body).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)

        return H


# --- hermetic environment -------------------------------------------------

class Harness:
    def __init__(self, root):
        self.root = Path(root)
        self.home = self.root / "home"
        self.config = self.root / "config"
        self.work = self.root / "work"
        for d in (self.home, self.config, self.work, self.axe_root()):
            d.mkdir(parents=True, exist_ok=True)

    def axe_root(self):
        return self.config / "axe"

    def session_root(self):
        value = 0xcbf29ce484222325
        for byte in os.fsencode(self.work.resolve()):
            value ^= byte
            value = value * 0x100000001b3 & 0xffffffffffffffff
        return self.axe_root() / "projects" / f"{value:016x}"

    def write_config(self, text):
        (self.axe_root() / "config").write_text(text)

    def seed_session(self, sid, title, content):
        self.seed_session_msgs(sid, title, [{"Role": "user", "Content": content}])

    def state_entries(self, sid=None):
        root = self.session_root()
        entries = []
        path = root / "state.json"
        if path.exists():
            entries.extend(json.loads(path.read_text()).get("entries", []))
        journal = root / "journal.jsonl"
        if journal.exists():
            for line in journal.read_text().splitlines():
                if line.strip():
                    entries.extend(json.loads(line).get("entries", []))
        if sid is not None:
            entries = [entry for entry in entries if entry.get("session") == sid]
        return entries

    def session_state(self, sid=None):
        root = self.session_root()
        states = {}
        path = root / "state.json"
        if path.exists():
            for item in json.loads(path.read_text()).get("states", []):
                states[item["session"]] = item.get("state", {})
        journal = root / "journal.jsonl"
        if journal.exists():
            for line in journal.read_text().splitlines():
                if line.strip():
                    for item in json.loads(line).get("states", []):
                        states[item["session"]] = item.get("state", {})
        if sid is not None:
            return states.get(sid, {})
        return states

    def seed_session_msgs(self, sid, title, msgs):
        root = self.session_root()
        root.mkdir(parents=True, exist_ok=True)
        entries = [
            entry
            for entry in self.state_entries()
            if entry.get("session") != sid
        ]
        for msg in msgs:
            entries.append(
                {
                    "id": len(entries),
                    "at": len(entries),
                    "session": sid,
                    "entry": {"type": "message", "message": msg},
                }
            )
        for index, entry in enumerate(entries):
            entry["id"] = index
        state = {
            "version": 1,
            "seq": 1,
            "next_id": len(entries),
            "next_task": 0,
            "entries": entries,
            "tasks": [],
        }
        (root / "state.json").write_text(json.dumps(state))
        (root / "journal.jsonl").write_text("")
        (root / "session.current").write_text(sid)

    def env(self, api_key="sk-test", extra=None):
        e = dict(os.environ)
        e.update(
            {
                "HOME": str(self.home),
                "XDG_CONFIG_HOME": str(self.config),
                "TERM": "xterm-256color",
                "LANG": "C.UTF-8",
                "LC_ALL": "C.UTF-8",
            }
        )
        e.pop("TMUX", None)
        e.pop("OPENAI_API_KEY", None)
        e.pop("DEEPSEEK_API_KEY", None)
        e.pop("DEEPSEEK_BASE_URL", None)
        if api_key is not None:
            e["OPENAI_API_KEY"] = api_key
        if extra:
            e.update(extra)
        return e

    @contextmanager
    def mock(self, scenario, **kw):
        srv = MockServer(scenario, **kw).start()
        try:
            yield srv
        finally:
            srv.stop()

    def oneshot(self, axe, args, stdin=None, base=None, api_key="sk-test", extra_env=None):
        if base:
            args = ["-base", base] + args
        r = subprocess.run(
            [axe] + args,
            input=stdin,
            capture_output=True,
            env=self.env(api_key=api_key, extra=extra_env),
            timeout=120,
            cwd=str(self.work),
        )
        r.stdout = r.stdout.decode("utf-8", "replace")
        r.stderr = r.stderr.decode("utf-8", "replace")
        return r

    def tui(self, axe, args=None, base=None, api_key="sk-test", extra_env=None, cols=100, rows=30):
        if base:
            args = ["-base", base] + (args or [])
        return Tui(axe, self.env(api_key=api_key, extra=extra_env), args or [], cwd=str(self.work), cols=cols, rows=rows)


# --- PTY driver for the TUI ----------------------------------------------

KEYS = {
    "enter": b"\r",
    "esc": b"\x1b",
    "tab": b"\t",
    "backspace": b"\x7f",
    "up": b"\x1b[A",
    "down": b"\x1b[B",
    "ctrl_c": b"\x03",
    "shift_enter": b"\x1b[13;2u",
    "eof": b"\x04",
}


class Tui:
    def __init__(self, axe, env, args, cwd, cols=100, rows=30):
        self.cols = cols
        self.rows = rows
        self.master, slave = pty.openpty()
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.proc = subprocess.Popen(
            [axe] + args, stdin=slave, stdout=slave, stderr=slave, env=env, cwd=cwd
        )
        os.close(slave)
        self.buf = b""
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._pump, daemon=True)
        self._thread.start()

    def _pump(self):
        # The TUI repaints constantly; if the master is not drained the PTY
        # output buffer fills and the TUI blocks on write, never reading keys.
        while not self._stop.is_set():
            r, _, _ = select.select([self.master], [], [], 0.05)
            if not r:
                continue
            try:
                data = os.read(self.master, 65536)
            except OSError:
                return
            if not data:
                return
            with self._lock:
                self.buf += data

    def send(self, text):
        try:
            os.write(self.master, text.encode() if isinstance(text, str) else text)
        except OSError:
            pass

    def type(self, text, delay=0.01):
        for ch in text:
            self.send(ch)
            time.sleep(delay)

    def key(self, name):
        self.send(KEYS[name])

    def output(self):
        with self._lock:
            return screen_text(self.buf, self.cols, self.rows)

    def expect(self, text, timeout=20):
        deadline = time.time() + timeout
        expected = "".join(text.split())
        while time.time() < deadline:
            output = self.output()
            if text in output or expected in "".join(output.split()):
                return
            if self.proc.poll() is not None:
                time.sleep(0.1)
                output = self.output()
                if text in output or expected in "".join(output.split()):
                    return
                raise AssertionError(
                    "tui exited (code %s) before %r appeared\n--- output ---\n%s"
                    % (self.proc.poll(), text, self.output()[-4000:])
                )
            time.sleep(0.05)
        raise AssertionError(
            "timeout waiting for %r\n--- output ---\n%s" % (text, self.output()[-4000:])
        )

    def wait_exit(self, timeout=20):
        deadline = time.time() + timeout
        while time.time() < deadline:
            code = self.proc.poll()
            if code is not None:
                time.sleep(0.1)
                return code
            time.sleep(0.05)
        self.proc.kill()
        raise AssertionError("tui did not exit\n--- output ---\n%s" % self.output()[-4000:])

    def close(self):
        self._stop.set()
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait(timeout=5)
        self._thread.join(timeout=2)
        try:
            os.close(self.master)
        except OSError:
            pass


# --- test cases ----------------------------------------------------------

CASES = []


def case(fn):
    CASES.append(fn)
    return fn




def invalid_transcript(msgs):
    """Describe the first provider-invalid tool exchange, or None."""
    pending = set()
    for m in msgs:
        role = m.get("role")
        if role == "assistant":
            if pending:
                return "tool_calls %s left unanswered" % sorted(pending)
            pending = {c["id"] for c in m.get("tool_calls") or []}
        elif role == "tool":
            pending.discard(m.get("tool_call_id"))
        elif role == "user" and pending:
            return "user message with tool_calls %s unanswered" % sorted(pending)
    if pending:
        return "transcript ends with tool_calls %s unanswered" % sorted(pending)
    return None


def body_msg(server, i, role):
    return [m for m in server.requests[i]["json"]["messages"] if m["role"] == role]


@case
def oneshot_prints_answer(h, axe):
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["hello"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check("Done" in p.stdout, "stdout: %s" % p.stdout[-500:])
        check("tokens:" in p.stderr, "stderr: %s" % p.stderr[-500:])
        check(len(srv.requests) == 1, "expected 1 request, got %d" % len(srv.requests))
        check(body_msg(srv, 0, "user")[-1]["content"] == "hello", "prompt not sent")
        check(
            srv.requests[0]["headers"].get("Authorization") == "Bearer sk-test",
            "auth header wrong: %s" % srv.requests[0]["headers"],
        )
        body = srv.requests[0]["json"]
        check(body.get("reasoning_effort") == "max", "effort not max: %s" % body)
        check(body.get("thinking") == {"type": "enabled"}, "thinking not enabled: %s" % body)


@case
def oneshot_stdin(h, axe):
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, [], stdin=b"hello from stdin", base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check("Done" in p.stdout, "stdout: %s" % p.stdout[-500:])
        check(
            body_msg(srv, 0, "user")[-1]["content"] == "hello from stdin",
            "stdin prompt not sent",
        )





@case
def oneshot_tool_loop(h, axe):
    with h.mock(TOOL_SCENARIO) as srv:
        p = h.oneshot(axe, ["use tools"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check("Done" in p.stdout, "stdout: %s" % p.stdout[-500:])
        check(len(srv.requests) == 2, "expected 2 requests, got %d" % len(srv.requests))
        tool_msgs = body_msg(srv, 1, "tool")
        check(
            any("hello" in m.get("content", "") for m in tool_msgs),
            "tool result not fed back: %s" % tool_msgs,
        )
        check("echo hello" in p.stderr, "tool trace missing: %s" % p.stderr[-500:])


@case
def oneshot_bash_workdir(h, axe):
    with h.mock(WRITE_FILE_SCENARIO) as srv:
        p = h.oneshot(axe, ["-C", str(h.work), "write a file"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check((h.work / "out.txt").read_text() == "hi", "bash ran in wrong dir")


@case
def oneshot_write_workdir(h, axe):
    with h.mock(WRITE_TOOL_SCENARIO) as srv:
        p = h.oneshot(axe, ["-C", str(h.work), "write a file"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check((h.work / "out.txt").read_text() == "hi", "write ran in wrong dir")


def edit_result(srv):
    return body_msg(srv, 1, "tool")[-1]["content"]


@case
def oneshot_edit_patch(h, axe):
    (h.work / "f.txt").write_text("a\nb\nc\n")
    scenario = [
        {
            "stream": [
                tool_chunk("edit", {"path": "f.txt", "edits": [{"oldText": "b\n", "newText": "B\n"}]}),
                usage_chunk(10, 5),
            ]
        },
        {"stream": [content_chunk("done", "stop"), usage_chunk(20, 8)]},
    ]
    with h.mock(scenario) as srv:
        p = h.oneshot(axe, ["edit it"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        result = edit_result(srv)
    check((h.work / "f.txt").read_text() == "a\nB\nc\n", (h.work / "f.txt").read_text())
    check("Successfully replaced 1 block(s)" in result, result)
    check("--- f.txt" in result and "+++ f.txt" in result, result)
    check("-b" in result and "+B" in result, result)


@case
def oneshot_edit_fuzzy(h, axe):
    (h.work / "f.txt").write_text("let x = \uff08hi\uff09;\n")
    scenario = [
        {
            "stream": [
                tool_chunk("edit", {"path": "f.txt", "edits": [{"oldText": "(hi)", "newText": "(bye)"}]}),
                usage_chunk(10, 5),
            ]
        },
        {"stream": [content_chunk("done", "stop"), usage_chunk(20, 8)]},
    ]
    with h.mock(scenario) as srv:
        p = h.oneshot(axe, ["edit it"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
    check(
        (h.work / "f.txt").read_text() == "let x = (bye);\n",
        repr((h.work / "f.txt").read_text()),
    )


@case
def oneshot_edit_quirk_string(h, axe):
    (h.work / "f.txt").write_text("a\nb\nc\n")
    edits = json.dumps([{"oldText": "b\n", "newText": "B\n"}])
    scenario = [
        {
            "stream": [
                tool_chunk("edit", {"path": "f.txt", "edits": edits}),
                usage_chunk(10, 5),
            ]
        },
        {"stream": [content_chunk("done", "stop"), usage_chunk(20, 8)]},
    ]
    with h.mock(scenario) as srv:
        p = h.oneshot(axe, ["edit it"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
    check((h.work / "f.txt").read_text() == "a\nB\nc\n", repr((h.work / "f.txt").read_text()))


@case
def oneshot_edit_patch_truncated(h, axe):
    old = "".join("old line %d\n" % i for i in range(300))
    new = "".join("new line %d\n" % i for i in range(300))
    (h.work / "f.txt").write_text(old)
    scenario = [
        {
            "stream": [
                tool_chunk("edit", {"path": "f.txt", "edits": [{"oldText": old, "newText": new}]}),
                usage_chunk(10, 5),
            ]
        },
        {"stream": [content_chunk("done", "stop"), usage_chunk(20, 8)]},
    ]
    with h.mock(scenario) as srv:
        p = h.oneshot(axe, ["edit it"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        result = edit_result(srv)
    check((h.work / "f.txt").read_text() == new, "large edit not applied")
    check("lines omitted" in result, result[:300] + " ... " + result[-300:])
    check(result.count("\n") <= 84, "patch not bounded: %d lines" % result.count("\n"))


@case
def oneshot_flags_and_config(h, axe):
    h.write_config('base = "http://127.0.0.1:9"\nmodel = "cfg-model"\n')
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["-model", "flag-model", "-system", "You are Taco.", "hi"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        req = srv.requests[0]["json"]
        check(req["model"] == "flag-model", "flag model lost: %s" % req["model"])
        sys_msgs = [m for m in req["messages"] if m["role"] == "system"]
        check(any("Taco" in m.get("content", "") for m in sys_msgs), "system flag lost")
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["hi"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check(srv.requests[0]["json"]["model"] == "cfg-model", "config model ignored")


@case
def oneshot_config_api_key(h, axe):
    h.write_config('api_key = "cfg-key"\n')
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["hi"], base=srv.base_url, api_key=None)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check(
            srv.requests[0]["headers"].get("Authorization") == "Bearer cfg-key",
            "config key not used",
        )


@case
def oneshot_env_beats_config(h, axe):
    h.write_config('api_key = "cfg-key"\n')
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["hi"], base=srv.base_url, api_key="env-key")
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check(
            srv.requests[0]["headers"].get("Authorization") == "Bearer env-key",
            "env key should win over config",
        )


@case
def oneshot_retry(h, axe):
    scenario = [
        {"status": 429, "body": '{"error":{"message":"rate limited"}}'},
        {"status": 500, "body": "boom"},
        ANSWER_SCENARIO[0],
    ]
    with h.mock(scenario) as srv:
        p = h.oneshot(axe, ["hi"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check(len(srv.requests) == 3, "expected 3 requests, got %d" % len(srv.requests))
        statuses = [r["headers"].get("Content-Length") for r in srv.requests]
        check(statuses, "requests recorded without bodies")


@case
def oneshot_retry_exhausted(h, axe):
    scenario = [{"status": 429, "body": '{"error":{"message":"rate limited"}}'}] * 3
    with h.mock(scenario) as srv:
        p = h.oneshot(axe, ["hi"], base=srv.base_url)
        check(p.returncode == 1, "expected exit 1, got %s" % p.returncode)
        check("429" in p.stderr and "rate limited" in p.stderr, "stderr: %s" % p.stderr[-500:])
        check(len(srv.requests) == 3, "expected 3 attempts, got %d" % len(srv.requests))


@case
def oneshot_provider_down(h, axe):
    p = h.oneshot(axe, ["hi"], base="http://127.0.0.1:1")
    check(p.returncode == 1, "expected exit 1, got %s" % p.returncode)
    check("error:" in p.stderr, "stderr: %s" % p.stderr[-500:])


@case
def oneshot_bad_flag(h, axe):
    p = h.oneshot(axe, ["-nope"])
    check(p.returncode == 2, "expected exit 2, got %s" % p.returncode)
    check("flag provided but not defined" in p.stderr, "stderr: %s" % p.stderr[-500:])


@case
def oneshot_end_of_flags(h, axe):
    # `--` lets a prompt that starts with a dash through.
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["--", "-fix the bug"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check(
            body_msg(srv, 0, "user")[-1]["content"] == "-fix the bug",
            "dash prompt not sent: %s" % body_msg(srv, 0, "user"),
        )

    # Flags before `--` still take effect.
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["-model", "dash-model", "--", "hi"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check(srv.requests[0]["json"]["model"] == "dash-model", "flag before -- lost")
        check(body_msg(srv, 0, "user")[-1]["content"] == "hi", "prompt after -- lost")

    # A lone dash is a prompt, not a flag.
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["-"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check(body_msg(srv, 0, "user")[-1]["content"] == "-", "bare dash not sent")


@case
def oneshot_help(h, axe):
    p = h.oneshot(axe, ["--help"])
    check(p.returncode == 0, "expected exit 0, got %s" % p.returncode)
    check("Usage: axe" in p.stderr, "stderr: %s" % p.stderr[-500:])


@case
def oneshot_version(h, axe):
    p = h.oneshot(axe, ["--version"])
    check(p.returncode == 0, "expected exit 0, got %s" % p.returncode)
    check(p.stdout == "axe 0.1.0\n", "stdout: %s" % p.stdout[-500:])



@case
def tui_help_screen(h, axe):
    with h.mock(ANSWER_SCENARIO) as srv:
        t = h.tui(axe, base=srv.base_url)
        try:
            t.expect("Run /help")
            t.type("/help\r")
            t.expect("/resume")
            t.key("esc")
            t.type("/quit\r")
            check(t.wait_exit() == 0, "exit code %s" % t.proc.poll())
        finally:
            t.close()


@case
def oneshot_reads_image(h, axe):
    """The model pulls an image into its own context by reading the file: the
    tool result carries the image, so no user turn is needed."""
    shot = h.work / "shot.png"
    shot.write_bytes(bytes.fromhex("89504e470d0a1a0a") + b"\x00" * 32)
    scenario = [
        {"stream": [tool_chunk("read", {"path": str(shot)}), usage_chunk(10, 2)]},
        {"stream": [content_chunk("It is a PNG.", "stop"), usage_chunk(30, 4)]},
    ]
    with h.mock(scenario) as srv:
        p = h.oneshot(axe, ["what is in shot.png?"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check("It is a PNG." in p.stdout, p.stdout)
    tool = body_msg(srv, 1, "tool")[-1]
    content = tool["content"]
    check(isinstance(content, list), "expected image blocks, got %r" % (content,))
    check(content[1]["type"] == "image_url", content)
    check(
        content[1]["image_url"]["url"].startswith("data:image/png;base64,"),
        content[1],
    )


@case
def tui_image_attach(h, axe):
    """A local image attaches through /image and reaches the provider as a
    content block array rather than a plain string."""
    shot = h.work / "shot.png"
    shot.write_bytes(bytes.fromhex("89504e470d0a1a0a") + b"\x00" * 64)
    with h.mock(ANSWER_SCENARIO) as srv:
        t = h.tui(axe, base=srv.base_url)
        try:
            t.expect("Run /help")
            t.type("/image %s\r" % shot)
            t.expect("attached")
            t.type("describe this\r")
            t.expect("Done")
            t.type("/quit\r")
            check(t.wait_exit() == 0, "exit code %s" % t.proc.poll())
        finally:
            t.close()
    content = body_msg(srv, 0, "user")[-1]["content"]
    check(isinstance(content, list), "expected a block array, got %r" % (content,))
    check(content[0] == {"type": "text", "text": "describe this"}, content)
    check(content[1]["type"] == "image_url", content)
    check(
        content[1]["image_url"]["url"].startswith("data:image/png;base64,"),
        content[1],
    )


@case
def tui_full_turn(h, axe):
    with h.mock(TOOL_SCENARIO) as srv:
        t = h.tui(axe, base=srv.base_url)
        try:
            t.expect("Run /help")
            t.type("check the tool\r")
            t.expect("Ran echo hello")
            t.expect("Done")
            t.type("/quit\r")
            check(t.wait_exit() == 0, "exit code %s" % t.proc.poll())
        finally:
            t.close()
        text = json.dumps(h.state_entries())
        check("check the tool" in text, "user message missing: %s" % text)
        check("echo hello" in text, "tool call missing: %s" % text)
        check("Done" in text, "answer missing: %s" % text)
        check(len(srv.requests) == 2, "expected 2 requests, got %d" % len(srv.requests))


@case
def usage_persisted(h, axe):
    with h.mock(TOOL_SCENARIO) as srv:
        t = h.tui(axe, base=srv.base_url)
        try:
            t.expect("Run /help")
            t.type("check the tool\r")
            t.expect("Ran echo hello")
            t.expect("Done")
            t.type("/quit\r")
            check(t.wait_exit() == 0, "exit code %s" % t.proc.poll())
        finally:
            t.close()
    states = [state for state in h.session_state().values() if state.get("input")]
    check(len(states) == 1, "tui: expected 1 usage state, got %d" % len(states))
    usage = states[0]
    check(usage["input"] == 30, "tui: billed input: %s" % usage)
    check(usage["output"] == 13, "tui: billed output: %s" % usage)
    check(usage["context_input"] == 20, "tui: context_input: %s" % usage)
    check(usage["context_output"] == 8, "tui: context_output: %s" % usage)

    h.seed_session("111", "old", "prior work")
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["--resume", "111", "continue"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
    usage = h.session_state("111")
    check(usage.get("input") == 20, "oneshot: billed input: %s" % usage)
    check(usage.get("context_input") == 20, "oneshot: context_input: %s" % usage)
    check(usage.get("context_output") == 8, "oneshot: context_output: %s" % usage)


@case
def resume_repairs_unanswered_tool_calls(h, axe):
    # A crash mid-batch leaves an assistant message whose tool_calls have no
    # results. Providers reject that, so resume must drop the unfinished
    # exchange before sending.
    h.seed_session_msgs(
        "111",
        "crashy",
        [
            {"Role": "user", "Content": "do work"},
            {
                "Role": "assistant",
                "Content": "",
                "ToolCalls": [{"ID": "c1", "Name": "bash", "Arguments": '{"command":"echo a"}'}],
            },
            {"Role": "tool", "Content": "a\n", "ToolCallID": "c1"},
            {
                "Role": "assistant",
                "Content": "",
                "ToolCalls": [{"ID": "c2", "Name": "bash", "Arguments": '{"command":"echo b"}'}],
            },
        ],
    )
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["--resume", "111", "continue"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
    sent = srv.requests[0]["json"]["messages"]
    check(invalid_transcript(sent) is None, "oneshot: %s" % invalid_transcript(sent))
    calls = [c["id"] for m in sent for c in m.get("tool_calls") or []]
    check("c2" not in calls, "dangling call not dropped: %s" % calls)
    check("c1" in calls, "complete call dropped: %s" % calls)
    check(sent[-1]["content"] == "continue", "prompt missing: %s" % sent[-1])

    # Resuming the now-extended session stays valid: the repair drops only
    # the unfinished exchange, never the turns around it.
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["--resume", "111", "again"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
    sent = srv.requests[0]["json"]["messages"]
    check(invalid_transcript(sent) is None, "second resume: %s" % invalid_transcript(sent))
    check(any(m.get("content") == "continue" for m in sent), "earlier turn lost: %s" % sent)

    # The TUI resume path repairs too.
    with h.mock(ANSWER_SCENARIO) as srv:
        t = h.tui(axe, ["--resume", "111"], base=srv.base_url)
        try:
            t.expect("Run /help")
            t.type("one more\r")
            t.expect("Done")
            t.type("/quit\r")
            check(t.wait_exit() == 0, "exit code %s" % t.proc.poll())
        finally:
            t.close()
    sent = srv.requests[0]["json"]["messages"]
    check(invalid_transcript(sent) is None, "tui: %s" % invalid_transcript(sent))


@case
def tui_ctrl_c_quit(h, axe):
    with h.mock(ANSWER_SCENARIO) as srv:
        t = h.tui(axe, base=srv.base_url)
        try:
            t.expect("Run /help")
            t.key("ctrl_c")
            t.expect("press ctrl+c again to exit")
            t.key("ctrl_c")
            check(t.wait_exit() == 0, "exit code %s" % t.proc.poll())
        finally:
            t.close()


@case
def oneshot_resume_last(h, axe):
    h.seed_session("111", "old", "prior work")
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["--resume", "last", "continue it"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        check("Done" in p.stdout, "stdout: %s" % p.stdout[-500:])
        users = body_msg(srv, 0, "user")
        check(any("prior work" in m.get("content", "") for m in users), "history not sent: %s" % users)
        check(users[-1]["content"] == "continue it", "prompt not appended: %s" % users)
        text = json.dumps(h.state_entries("111"))
        check("continue it" in text, "resumed prompt not saved: %s" % text)


@case
def oneshot_bare_resume_errors(h, axe):
    p = h.oneshot(axe, ["-r", "hello"])
    check(p.returncode == 2, "expected exit 2, got %s" % p.returncode)
    check("needs last or a session id" in p.stderr, "stderr: %s" % p.stderr[-500:])


@case
def oneshot_resume_persists_on_failure(h, axe):
    h.seed_session("111", "old", "prior work")
    p = h.oneshot(axe, ["--resume", "111", "keep this"], base="http://127.0.0.1:1")
    check(p.returncode == 1, "expected exit 1, got %s" % p.returncode)
    text = json.dumps(h.state_entries("111"))
    check("keep this" in text, "failed resume not saved: %s" % text)
    check("prior work" in text, "history dropped: %s" % text)


@case
def oneshot_resume_last_flushes_live(h, axe):
    h.seed_session_msgs(
        "111",
        "old",
        [
            {"Role": "user", "Content": "prior work"},
            {"Role": "user", "Content": "live extra"},
        ],
    )
    with h.mock(ANSWER_SCENARIO) as srv:
        p = h.oneshot(axe, ["--resume", "last", "continue it"], base=srv.base_url)
        check(p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr[-500:]))
        users = body_msg(srv, 0, "user")
        check(any("live extra" in m.get("content", "") for m in users), "live not flushed: %s" % users)
        check(users[-1]["content"] == "continue it", "prompt not appended: %s" % users)
        text = json.dumps(h.state_entries("111"))
        check("live extra" in text and "continue it" in text, "archive missing live: %s" % text)


@case
def tui_resume_last(h, axe):
    with h.mock(TOOL_SCENARIO) as srv:
        t = h.tui(axe, base=srv.base_url)
        try:
            t.expect("Run /help")
            t.type("check the tool\r")
            t.expect("Ran echo hello")
            t.expect("Done")
            t.type("/quit\r")
            check(t.wait_exit() == 0, "first session exit")
        finally:
            t.close()
        t2 = h.tui(axe, ["--resume", "last"], base=srv.base_url)
        try:
            t2.expect("Run /help")
            t2.expect("check the tool")
            t2.expect("Ran echo hello")
            t2.type("/quit\r")
            check(t2.wait_exit() == 0, "resumed session exit")
        finally:
            t2.close()



@case
def tui_rewind(h, axe):
    with h.mock(REWIND_SCENARIO) as srv:
        t = h.tui(axe, base=srv.base_url)
        try:
            t.expect("Run /help")
            t.type("first question\r")
            t.expect("FIRST ANSWER")
            t.type("second question\r")
            t.expect("SECOND ANSWER")
            # Double-esc opens the rewind screen, preselected on the most
            # recent message; up moves to "second question", enter rewinds.
            t.key("esc")
            time.sleep(0.05)
            t.key("esc")
            t.expect("Rewind 4")
            t.expect("second question")
            t.key("up")
            t.key("enter")
            t.expect("2 messages remaining")
            t.type("/quit\r")
            check(t.wait_exit() == 0, "exit code %s" % t.proc.poll())
        finally:
            t.close()
        text = json.dumps(h.state_entries())
        check("first question" in text, "kept user message missing: %s" % text)
        check("FIRST ANSWER" in text, "kept answer missing: %s" % text)
        check("second question" not in text, "rewound turn not truncated: %s" % text)



def main(argv):
    axe = "target/release/axe"
    filt = None
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--bin":
            i += 1
            if i >= len(argv):
                usage()
            axe = argv[i]
        elif a == "--filter":
            i += 1
            if i >= len(argv):
                usage()
            filt = argv[i]
        elif a == "--list":
            for fn in CASES:
                print(fn.__name__)
            return 0
        elif a in ("-h", "--help"):
            usage()
        else:
            usage()
        i += 1
    axe = os.path.abspath(axe)
    if not os.path.exists(axe):
        print("binary not found: %s (build first: make harness or cargo build --release)" % axe)
        return 2

    failed = []
    for fn in CASES:
        if filt and filt not in fn.__name__:
            continue
        tmp = tempfile.mkdtemp(prefix="axe-harness-")
        h = Harness(tmp)
        t0 = time.time()
        try:
            fn(h, axe)
            shutil.rmtree(tmp, ignore_errors=True)
            print("PASS %-28s %5.1fs" % (fn.__name__, time.time() - t0))
        except Exception as e:
            failed.append(fn.__name__)
            print("FAIL %-28s %s" % (fn.__name__, e))
            print("     artifacts kept in: %s" % tmp)
    print()
    if failed:
        print("%d/%d cases failed: %s" % (len(failed), len(CASES), ", ".join(failed)))
        return 1
    print("all %d cases passed" % len(CASES))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
