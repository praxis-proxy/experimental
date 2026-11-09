#!/usr/bin/env python3
"""Deterministic Switchyard judge and OpenAI-shaped upstream fixtures."""

from __future__ import annotations

import json
import threading
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

COUNTS: Counter[str] = Counter()
COUNTS_LOCK = threading.Lock()


def read_body(handler: BaseHTTPRequestHandler) -> dict[str, object]:
    length = int(handler.headers.get("Content-Length", "0"))
    raw = handler.rfile.read(length) if length else b"{}"
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        return {}
    return value if isinstance(value, dict) else {}


def write_json(handler: BaseHTTPRequestHandler, payload: dict[str, object]) -> None:
    encoded = json.dumps(payload, separators=(",", ":")).encode()
    handler.send_response(200)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(encoded)))
    handler.end_headers()
    handler.wfile.write(encoded)


def completion(model: str, text: str) -> dict[str, object]:
    return {
        "id": "demo-completion",
        "object": "chat.completion",
        "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        # Five tokens cost exactly $0.05 at the bootstrap's $10,000 / MTok rate.
        "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5},
    }


def judge() -> type[BaseHTTPRequestHandler]:
    class Judge(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802
            read_body(self)
            verdict = {"crux": "deterministic demo", "primary_rule": "SUP-1", "capability_boundary": "supported", "p_solve": 0.95}
            write_json(self, completion("mock-switchyard-judge", json.dumps(verdict, separators=(",", ":"))))

        def log_message(self, _format: str, *_args: object) -> None:
            return

    return Judge


def upstream(name: str) -> type[BaseHTTPRequestHandler]:
    class Upstream(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802
            body = read_body(self)
            with COUNTS_LOCK:
                COUNTS[name] += 1
            model = body.get("model") if isinstance(body.get("model"), str) else name
            write_json(self, completion(model, f"served_by={name}"))

        def log_message(self, _format: str, *_args: object) -> None:
            return

    return Upstream


def control() -> type[BaseHTTPRequestHandler]:
    class Control(BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802
            if self.path != "/counts":
                self.send_error(404)
                return
            with COUNTS_LOCK:
                payload = {name: COUNTS[name] for name in ("weak", "strong", "free")}
            write_json(self, payload)

        def log_message(self, _format: str, *_args: object) -> None:
            return

    return Control


def serve(port: int, handler: type[BaseHTTPRequestHandler]) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer(("0.0.0.0", port), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


if __name__ == "__main__":
    servers = [serve(18091, judge()), serve(18092, upstream("weak")), serve(18093, upstream("strong")), serve(18094, upstream("free")), serve(9090, control())]
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        for server in servers:
            server.shutdown()
