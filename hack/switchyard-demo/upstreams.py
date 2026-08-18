#!/usr/bin/env python3
"""Local echo "clusters" for the switchyard_route demo (stdlib only).

The judge in this demo is a *real* OpenAI-compatible endpoint (see
run-demo.sh); only the two upstream "model" servers are stubbed, so the
routing decision and the rewritten `model` field are plainly visible in the
response without needing real GPUs.

Runs two loopback HTTP servers:

- 18092: the weak (efficient) upstream. Echoes the model it received.
- 18093: the strong (capable) upstream. Echoes the model it received.

Each responds to any POST with a JSON object naming which tier served the
request and the `model` field it saw (i.e. the value switchyard_route
rewrote into the body). Usage: python3 upstreams.py  (Ctrl-C to stop)
"""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def upstream(name: str):
    """An upstream handler that echoes which tier served the request."""

    class Upstream(BaseHTTPRequestHandler):
        def do_POST(self):  # noqa: N802 (http.server API)
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length) or b"{}")
            payload = json.dumps(
                {
                    "served_by": name,
                    "model_received": body.get("model"),
                }
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, format, *args):  # noqa: A002 (matches base signature)
            print(f"[{name}] {format % args}", flush=True)

    return Upstream


def serve(port: int, handler) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer(("127.0.0.1", port), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print(f"listening on 127.0.0.1:{port}", flush=True)
    return server


if __name__ == "__main__":
    servers = [
        serve(18092, upstream("weak-upstream")),
        serve(18093, upstream("strong-upstream")),
    ]
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        for running in servers:
            running.shutdown()
