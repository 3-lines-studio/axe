#!/usr/bin/env python3
"""Mock OpenAI-compatible endpoint for testing the axe TUI without an API key.

First request: streams content then a bash tool call (echo hello).
Second request: streams a rich markdown answer (heading, table, code).

Usage:
    python3 scripts/mock-server.py            # listens on 127.0.0.1:8787
    OPENAI_API_KEY=test ./target/release/axe -base http://127.0.0.1:8787
"""
import http.server
import json
import threading
import time

PORT = 8787
calls = 0

RICH = """# Done

The **plan** worked.

| Step | Status |
|------|--------|
| one  | done   |
| two  | done   |

- [x] tested
- [ ] ship it

```rust
fn main() { println!("hello"); }
```
"""


def sse(chunks):
    return "".join(f"data: {json.dumps(c)}\n\n" for c in chunks).encode()


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        global calls
        n = int(self.headers.get("Content-Length", 0))
        req = json.loads(self.rfile.read(n))
        calls += 1
        if calls == 1:
            chunks = [
                {"choices": [{"delta": {"content": "Let me check. "}, "finish_reason": None}]},
                {"choices": [{"delta": {"content": "Running a quick command..."}, "finish_reason": None}]},
                {
                    "choices": [
                        {
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "c1",
                                        "type": "function",
                                        "function": {
                                            "name": "bash",
                                            "arguments": json.dumps({"command": "echo hello"}),
                                        },
                                    }
                                ]
                            },
                            "finish_reason": "tool_calls",
                        }
                    ],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5},
                },
            ]
        else:
            chunks = [
                {"choices": [{"delta": {"content": RICH}, "finish_reason": "stop"}],
                 "usage": {"prompt_tokens": 20, "completion_tokens": 8}},
            ]
        if req.get("stream"):
            data = sse(chunks)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
        else:
            # Non-streaming: return the final rich answer as plain JSON.
            data = json.dumps(
                {"choices": [{"message": {"role": "assistant", "content": RICH}}],
                 "usage": {"prompt_tokens": 20, "completion_tokens": 8}}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if req.get("stream"):
            for chunk in data.split(b"\n\n"):
                if chunk:
                    self.wfile.write(chunk + b"\n\n")
                    self.wfile.flush()
                    time.sleep(0.2)
        else:
            self.wfile.write(data)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"mock OpenAI endpoint on http://127.0.0.1:{PORT}", flush=True)
    srv.serve_forever()
