#!/usr/bin/env python3
"""Local mocks for switchyard_route: judge :18091, weak :18092, strong :18093."""

from __future__ import annotations

import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

JUDGE_PORT, WEAK_PORT, STRONG_PORT = 18091, 18092, 18093

# Markers that flip the mock judge to LIM-2 / p_solve=0 (demo hard prompts).
_HARD_MARKERS = (
    "reverse-engineer",
    "undocumented",
    "blurry",
    "whiteboard",
    "acme-vision",
    "no harness",
    "golden file",
)


def _read_json(handler: BaseHTTPRequestHandler) -> dict[str, Any]:
    length = int(handler.headers.get("Content-Length", "0"))
    raw = handler.rfile.read(length) if length else b"{}"
    try:
        value = json.loads(raw or b"{}")
    except json.JSONDecodeError:
        return {}
    return value if isinstance(value, dict) else {}


def _write_json(handler: BaseHTTPRequestHandler, payload: dict[str, Any]) -> None:
    body = json.dumps(payload).encode()
    handler.send_response(200)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


def _latest_user_text(body: dict[str, Any]) -> str:
    messages = body.get("messages")
    if not isinstance(messages, list):
        return ""
    for message in reversed(messages):
        if isinstance(message, dict) and message.get("role") == "user":
            content = message.get("content")
            if isinstance(content, str):
                return content
    return ""


def _verdict(prompt: str) -> dict[str, Any]:
    lowered = prompt.lower()
    if any(marker in lowered for marker in _HARD_MARKERS):
        return {
            "crux": "undocumented reference or missing harness",
            "primary_rule": "LIM-2",
            "capability_boundary": "unsupported",
            "p_solve": 0.0,
        }
    return {
        "crux": "bounded factual task",
        "primary_rule": "SUP-1",
        "capability_boundary": "supported",
        "p_solve": 0.95,
    }


def _chat_completion(model: str, content: str) -> dict[str, Any]:
    return {
        "id": "chat-mock",
        "object": "chat.completion",
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }
        ],
    }


def judge_handler() -> type[BaseHTTPRequestHandler]:
    class Judge(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802
            if not self.path.startswith("/v1/chat/completions"):
                self.send_error(404)
                return
            body = _read_json(self)
            prompt = _latest_user_text(body)
            match = re.search(r"(?is)user:\s*(.+)$", prompt)
            if match:
                prompt = match.group(1).strip()
            verdict = _verdict(prompt)
            print(
                f"[judge] p_solve={verdict['p_solve']} rule={verdict['primary_rule']} "
                f"preview={prompt[:60]!r}",
                flush=True,
            )
            _write_json(
                self,
                _chat_completion(
                    body.get("model") or "mock-switchyard-judge",
                    json.dumps(verdict, separators=(",", ":")),
                ),
            )

        def log_message(self, format: str, *args: object) -> None:  # noqa: A002
            return

    return Judge


def upstream(name: str) -> type[BaseHTTPRequestHandler]:
    class Upstream(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802
            body = _read_json(self)
            _write_json(
                self,
                _chat_completion(body.get("model") or name, f"served_by={name}"),
            )

        def log_message(self, format: str, *args: object) -> None:  # noqa: A002
            print(f"[{name}] {format % args}", flush=True)

    return Upstream


def serve(port: int, handler: type[BaseHTTPRequestHandler]) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer(("127.0.0.1", port), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print(f"listening on 127.0.0.1:{port}", flush=True)
    return server


if __name__ == "__main__":
    servers = [
        serve(JUDGE_PORT, judge_handler()),
        serve(WEAK_PORT, upstream("weak-upstream")),
        serve(STRONG_PORT, upstream("strong-upstream")),
    ]
    print("mocks ready", flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        for running in servers:
            running.shutdown()
