#!/usr/bin/env python3
"""Minimal HTTP echo server for exercising ActionType::Webhook end-to-end.

POST /echo -> 200, JSON body: {"received": <posted JSON>, "auth": <Authorization header or null>}
GET  /health -> 200 "ok" (readiness probe for the caller script)

Usage: mock_webhook_server.py <port>
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass  # keep test output quiet

    def do_GET(self):
        if self.path == "/health":
            self._send(200, b"ok")
        else:
            self._send(404, b"not found")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b""
        try:
            received = json.loads(raw) if raw else None
        except json.JSONDecodeError:
            received = raw.decode("utf-8", "replace")
        body = json.dumps({
            "received": received,
            "auth": self.headers.get("Authorization"),
        }).encode("utf-8")
        self._send(200, body)

    def _send(self, code, body: bytes):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    port = int(sys.argv[1])
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
