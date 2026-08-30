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

    def seed_session_msgs(self, sid, title, msgs):
        d = self.session_root() / "sessions"
        d.mkdir(parents=True, exist_ok=True)
        out = "".join(json.dumps({"type": "message", "message": m}) + "\n" for m in msgs)
        (d / (sid + ".jsonl")).write_text(out)
        if title:
            (d / (sid + ".title")).write_text(title)

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
            return strip_ansi(self.buf)

    def expect(self, text, timeout=20):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if text in self.output():
                return
            if self.proc.poll() is not None:
                time.sleep(0.1)
                if text in self.output():
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
        sessions = list((h.session_root() / "sessions").glob("*.jsonl"))
        check(len(sessions) == 1, "expected 1 archived session, got %d" % len(sessions))
        check(not (h.session_root() / "session.jsonl").exists(), "live session not archived")
        text = sessions[0].read_text()
        check("check the tool" in text, "user message missing: %s" % text)
        check("echo hello" in text, "tool call missing: %s" % text)
        check("Done" in text, "answer missing: %s" % text)
        check(len(srv.requests) == 2, "expected 2 requests, got %d" % len(srv.requests))


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
            t.expect("rewound · 2 messages remaining")
            t.type("/quit\r")
            check(t.wait_exit() == 0, "exit code %s" % t.proc.poll())
        finally:
            t.close()
        sessions = list((h.session_root() / "sessions").glob("*.jsonl"))
        check(len(sessions) == 1, "expected 1 archived session, got %d" % len(sessions))
        text = sessions[0].read_text()
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
