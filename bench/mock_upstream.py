#!/usr/bin/env python3
"""Minimal mock Solana RPC upstream. Responds instantly with a fixed result."""
import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json

BODY = json.dumps({"jsonrpc": "2.0", "id": 1, "result": 123456789}).encode()

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get('content-length', 0))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header('content-type', 'application/json')
        self.send_header('content-length', str(len(BODY)))
        self.end_headers()
        self.wfile.write(BODY)
    def log_message(self, *a, **kw):
        pass

ThreadingHTTPServer(('127.0.0.1', 18000), H).serve_forever()
