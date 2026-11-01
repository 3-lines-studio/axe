#!/usr/bin/env python3
import argparse
import json
import os
import statistics
import subprocess
import socket
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("bins", nargs="+", default=["target/release/axe"])
parser.add_argument("--runs", type=int, default=30)
parser.add_argument(
    "--scenario",
    choices=(
        "final",
        "stream",
        "write",
        "bash",
        "sleep",
        "history",
        "read-large",
        "read-large-offset",
        "edit-large",
        "edit-large-end",
        "all",
    ),
    default="all"
)
args = parser.parse_args()

lock = threading.Lock()
request_bytes = {}
connections = {}
large_file = Path(tempfile.gettempdir()) / f"axe-bench-large-{os.getpid()}.txt"
large_file.write_bytes(b"x\n" * 5_000_000)


def chunk(data):
    return f"data: {json.dumps(data, separators=(',', ':'))}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def do_POST(self):
        size = int(self.headers["Content-Length"])
        body = self.rfile.read(size)
        request = json.loads(body)
        run_id = self.headers.get("Authorization", "").removeprefix("Bearer ")
        with lock:
            request_bytes.setdefault(run_id, []).append(size)
            connections.setdefault(run_id, set()).add(self.client_address[1])
        tool_results = sum(message["role"] == "tool" for message in request["messages"])
        has_tool_result = tool_results > 0
        user_content = next(
            message.get("content", "")
            for message in reversed(request["messages"])
            if message["role"] == "user"
        )
        final_only = user_content == "Reply with done."
        if user_content == "Stream.":
            events = [
                {"choices": [{"delta": {"content": "x"}, "finish_reason": None}]}
                for _ in range(1000)
            ]
            events.append(
                {
                    "choices": [{"delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 1000},
                }
            )
        elif (
            has_tool_result and user_content != "Build history."
        ) or user_content == "Build history." and tool_results >= 100 or final_only:
            events = [
                {"choices": [{"delta": {"content": "done"}, "finish_reason": None}]},
                {
                    "choices": [{"delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 1},
                },
            ]
        else:
            if user_content == "Build history.":
                tool = {"name": "read", "arguments": '{"path":"missing.txt"}'}
            elif user_content == "Read large.":
                arguments = json.dumps({"path": str(large_file), "offset": 1, "limit": 1})
                tool = {"name": "read", "arguments": arguments}
            elif user_content == "Read large offset.":
                arguments = json.dumps(
                    {"path": str(large_file), "offset": 4_999_999, "limit": 1}
                )
                tool = {"name": "read", "arguments": arguments}
            elif user_content in ("Edit large.", "Edit large end."):
                arguments = json.dumps(
                    {"path": "large.txt", "edits": [{"oldText": "old\n", "newText": "new\n"}]}
                )
                tool = {"name": "edit", "arguments": arguments}
            elif user_content == "Run true.":
                tool = {"name": "bash", "arguments": '{"command":"true"}'}
            elif user_content == "Run sleep.":
                tool = {"name": "bash", "arguments": '{"command":"sleep 1"}'}
            else:
                tool = {"name": "write", "arguments": '{"path":"out.txt","content":"ok\\n"}'}
            events = [
                {
                    "choices": [
                        {
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_1",
                                        "function": tool,
                                    }
                                ]
                            },
                            "finish_reason": None,
                        }
                    ]
                },
                {
                    "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 1},
                },
            ]
        payload = b"".join(chunk(event) for event in events) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "keep-alive")
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()

    def log_message(self, format, *values):
        pass


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f"http://127.0.0.1:{server.server_port}"


def run(binary, index, scenario):
    run_id = f"{os.getpid()}-{index}-{time.monotonic_ns()}"
    prompts = {
        "final": "Reply with done.",
        "stream": "Stream.",
        "write": "Create out.txt containing ok.",
        "bash": "Run true.",
        "sleep": "Run sleep.",
        "history": "Build history.",
        "read-large": "Read large.",
        "read-large-offset": "Read large offset.",
        "edit-large": "Edit large.",
        "edit-large-end": "Edit large end.",
    }
    prompt = prompts[scenario]
    with tempfile.TemporaryDirectory(prefix="axe-bench-") as tmp:
        if scenario == "edit-large":
            Path(tmp, "large.txt").write_bytes(b"old\n" + b"x\n" * 5_000_000)
        if scenario == "edit-large-end":
            Path(tmp, "large.txt").write_bytes(b"x\n" * 5_000_000 + b"old\n")
        env = os.environ.copy()
        env["OPENAI_API_KEY"] = run_id
        env["XDG_CONFIG_HOME"] = str(Path(tmp) / ".config")
        started = time.perf_counter_ns()
        process = subprocess.Popen(
            [binary, "--base", base, "--model", "bench", "-C", tmp, prompt],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        _, status, usage = os.wait4(process.pid, 0)
        code = os.waitstatus_to_exitcode(status)
        process.returncode = code
        elapsed = (time.perf_counter_ns() - started) / 1_000_000
        cpu = (usage.ru_utime + usage.ru_stime) * 1000
        output = Path(tmp, "out.txt")
        edited = Path(tmp, "large.txt")
        valid = scenario != "write" or output.read_text() == "ok\n"
        valid = valid and (scenario != "edit-large" or edited.read_bytes().startswith(b"new\n"))
        valid = valid and (scenario != "edit-large-end" or edited.read_bytes().endswith(b"new\n"))
        if code != 0 or not valid:
            raise RuntimeError(f"{binary} failed {scenario} run {index + 1}")
    with lock:
        sizes = request_bytes.pop(run_id)
        count = len(connections.pop(run_id))
    return elapsed, cpu, usage.ru_maxrss, sum(sizes), count


scenarios = (
    ("final", "stream", "write", "bash", "sleep")
    if args.scenario == "all"
    else (args.scenario,)
)
for binary_arg in args.bins:
    binary = str(Path(binary_arg).resolve())
    print(Path(binary).name)
    for scenario in scenarios:
        run(binary, -1, scenario)
        samples = [run(binary, index, scenario) for index in range(args.runs)]
        elapsed = sorted(sample[0] for sample in samples)
        cpu = sorted(sample[1] for sample in samples)
        rss = sorted(sample[2] for sample in samples)
        sizes = [sample[3] for sample in samples]
        connection_counts = [sample[4] for sample in samples]
        p95 = elapsed[max(0, int(len(elapsed) * 0.95) - 1)]
        print(f"  {scenario}")
        print(f"    wall median {statistics.median(elapsed):.2f} ms  p95 {p95:.2f} ms")
        print(f"    CPU median {statistics.median(cpu):.2f} ms")
        print(f"    peak RSS median {statistics.median(rss) / 1024:.2f} MiB")
        print(f"    request bytes median {statistics.median(sizes):.0f}")
        print(f"    connections median {statistics.median(connection_counts):.0f}")
    print(f"  binary {Path(binary).stat().st_size / 1024:.1f} KiB")

server.shutdown()
large_file.unlink()
