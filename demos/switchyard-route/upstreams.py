#!/usr/bin/env python3
"""Local mocks for switchyard_route: judge, primary tiers, and failure target."""

from __future__ import annotations

import json
import os
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

JUDGE_PORT, WEAK_PORT, STRONG_PORT, FAILURE_PORT = 18091, 18092, 18093, 18094
_judge_down = threading.Event()
_upstream_down = {
    "weak-upstream": threading.Event(),
    "strong-upstream": threading.Event(),
    "failure-upstream": threading.Event(),
}
_upstream_delay_seconds = dict.fromkeys(_upstream_down, 0.0)
_upstream_response_padding_bytes = dict.fromkeys(_upstream_down, 0)
_fragmented_stream = threading.Event()
_fragmented_stream_release = threading.Event()
_judge_delay_seconds = 0.0
_failure_authorization = f"Bearer {os.environ.get('SWITCHYARD_FAILURE_API_KEY', 'demo-failure-key')}"

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


def _write_json(
    handler: BaseHTTPRequestHandler, payload: dict[str, Any], status: int = 200
) -> None:
    body = json.dumps(payload).encode()
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


def _write_chat_stream(
    handler: BaseHTTPRequestHandler, model: str, content: str, fragmented: bool = False
) -> None:
    chunks = [
        _chat_completion_chunk(model, {"role": "assistant", "content": content}, None),
        _chat_completion_chunk(model, {}, "stop"),
    ]
    events = [f"data: {json.dumps(chunk, separators=(',', ':'))}\n\n" for chunk in chunks]
    events.append("data: [DONE]\n\n")
    encoded = [event.encode() for event in events]
    handler.send_response(200)
    handler.send_header("Content-Type", "text/event-stream")
    handler.send_header("Cache-Control", "no-cache")
    handler.send_header("Content-Length", str(sum(len(event) for event in encoded)))
    handler.end_headers()
    if not fragmented:
        handler.wfile.write(b"".join(encoded))
        return
    handler.wfile.write(encoded[0])
    handler.wfile.flush()
    _fragmented_stream_release.wait(timeout=5)
    for event in encoded[1:]:
        handler.wfile.write(event)
        handler.wfile.flush()
        time.sleep(0.02)


def _chat_completion_chunk(model: str, delta: dict[str, str], finish_reason: str | None) -> dict[str, Any]:
    return {
        "id": "chat-stream-mock",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    }


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


def _request_path(handler: BaseHTTPRequestHandler) -> str:
    return handler.path.split("?", 1)[0]


def judge_handler() -> type[BaseHTTPRequestHandler]:
    class Judge(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802
            global _judge_delay_seconds
            path = _request_path(self)
            if path == "/control/down":
                _read_json(self)
                _judge_down.set()
                print("[judge] down", flush=True)
                _write_json(self, {"judge": "down"})
                return
            if path == "/control/up":
                _read_json(self)
                _judge_down.clear()
                print("[judge] up", flush=True)
                _write_json(self, {"judge": "up"})
                return
            if path == "/control/judge/delay":
                milliseconds = _read_json(self).get("milliseconds")
                if (
                    not isinstance(milliseconds, int)
                    or not 0 <= milliseconds <= 10_000
                ):
                    self.send_error(
                        400, "milliseconds must be an integer from 0 to 10000"
                    )
                    return
                _judge_delay_seconds = milliseconds / 1000
                print(f"[judge] delay={milliseconds}ms", flush=True)
                _write_json(self, {"judge": {"delay_ms": milliseconds}})
                return
            if path == "/control/fragmented-stream/on":
                _read_json(self)
                _fragmented_stream.set()
                _fragmented_stream_release.clear()
                _write_json(self, {"fragmented_stream": "on"})
                return
            if path == "/control/fragmented-stream/release":
                _read_json(self)
                _fragmented_stream_release.set()
                _write_json(self, {"fragmented_stream": "released"})
                return
            if path == "/control/fragmented-stream/off":
                _read_json(self)
                _fragmented_stream.clear()
                _fragmented_stream_release.set()
                _write_json(self, {"fragmented_stream": "off"})
                return
            for name, event in _upstream_down.items():
                if path == f"/control/{name}/down":
                    event.set()
                    _write_json(self, {name: "down"})
                    return
                if path == f"/control/{name}/up":
                    event.clear()
                    _write_json(self, {name: "up"})
                    return
                if path == f"/control/{name}/delay":
                    milliseconds = _read_json(self).get("milliseconds")
                    if (
                        not isinstance(milliseconds, int)
                        or not 0 <= milliseconds <= 10_000
                    ):
                        self.send_error(
                            400, "milliseconds must be an integer from 0 to 10000"
                        )
                        return
                    _upstream_delay_seconds[name] = milliseconds / 1000
                    print(f"[{name}] delay={milliseconds}ms", flush=True)
                    _write_json(self, {name: {"delay_ms": milliseconds}})
                    return
                if path == f"/control/{name}/response-padding":
                    padding_bytes = _read_json(self).get("bytes")
                    if (
                        not isinstance(padding_bytes, int)
                        or not 0 <= padding_bytes <= 12_000_000
                    ):
                        self.send_error(400, "bytes must be an integer from 0 to 12000000")
                        return
                    _upstream_response_padding_bytes[name] = padding_bytes
                    print(f"[{name}] response_padding={padding_bytes}bytes", flush=True)
                    _write_json(self, {name: {"response_padding_bytes": padding_bytes}})
                    return
            if not path.startswith("/v1/chat/completions"):
                self.send_error(404)
                return
            body = _read_json(self)
            if _judge_down.is_set():
                print("[judge] refusing (down)", flush=True)
                _write_json(self, {"error": "judge down"}, status=503)
                return
            delay_seconds = _judge_delay_seconds
            if delay_seconds:
                milliseconds = int(delay_seconds * 1000)
                print(f"[judge] delaying {milliseconds}ms", flush=True)
                time.sleep(delay_seconds)
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
            if _request_path(self) != "/v1/chat/completions":
                self.send_error(404)
                return
            body = _read_json(self)
            if name == "failure-upstream":
                if self.headers.get("Content-Type") != "application/json":
                    self.send_error(415, "failure request must be JSON")
                    return
                if self.headers.get("Authorization") != _failure_authorization:
                    self.send_error(401, "failure credential missing or invalid")
                    return
                if body.get("model") != "mock-failure":
                    self.send_error(400, "failure model was not applied")
                    return
            if _upstream_down[name].is_set():
                _write_json(self, {"error": f"{name} down"}, status=503)
                return
            delay_seconds = _upstream_delay_seconds[name]
            if delay_seconds:
                milliseconds = int(delay_seconds * 1000)
                print(f"[{name}] delaying {milliseconds}ms", flush=True)
                time.sleep(delay_seconds)
            content = f"served_by={name}" + "x" * _upstream_response_padding_bytes[name]
            if body.get("stream") is True:
                _write_chat_stream(
                    self,
                    body.get("model") or name,
                    content,
                    fragmented=_fragmented_stream.is_set(),
                )
                return
            _write_json(
                self,
                _chat_completion(body.get("model") or name, content),
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
        serve(FAILURE_PORT, upstream("failure-upstream")),
    ]
    print("mocks ready", flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        for running in servers:
            running.shutdown()
