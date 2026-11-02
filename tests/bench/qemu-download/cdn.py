#!/usr/bin/env python3
"""A stand-in for boot.arcboxcdn.com/qemu that can be slow, cut and wrong.

Serves every file in DIR at /qemu/v<version>/<name>, honours `Range:
bytes=N-`, and logs one line per request to stdout:

    GET /qemu/v0.0.0-dev.0169f2b/qemu-darwin-arm64-v0.0.0-dev.0169f2b.tar.gz range=bytes=2400000- status=206 sent=3530315

Usage: cdn.py DIR PORT [--rate BYTES_PER_S] [--cut-first BYTES] [--status CODE]

--rate       sends at most this many bytes per second (to watch progress).
--cut-first  stops the first full (non-Range) response after this many
             bytes and closes the connection, as a dropped link does.
--status     answers every request with this status and no body.
"""

import argparse
import http.server
import os
import sys
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument("dir")
parser.add_argument("port", type=int)
parser.add_argument("--rate", type=int, default=0)
parser.add_argument("--cut-first", type=int, default=0)
parser.add_argument("--status", type=int, default=0)
args = parser.parse_args()

cut_pending = threading.Event()
if args.cut_first:
    cut_pending.set()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def log(self, range_header, status, sent):
        print(f"GET {self.path} range={range_header or '-'} status={status} sent={sent}", flush=True)

    def do_GET(self):
        range_header = self.headers.get("Range")
        if args.status:
            self.send_response(args.status)
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            self.log(range_header, args.status, 0)
            return
        path = os.path.join(args.dir, os.path.basename(self.path))
        if not self.path.startswith("/qemu/v") or not os.path.isfile(path):
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            self.log(range_header, 404, 0)
            return
        with open(path, "rb") as f:
            body = f.read()
        length = len(body)
        start = 0
        if range_header and range_header.startswith("bytes=") and range_header.endswith("-"):
            start = int(range_header[len("bytes="):-1])
        if start >= length and range_header:
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{length}")
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            self.log(range_header, 416, 0)
            return
        status = 206 if range_header else 200
        self.send_response(status)
        if range_header:
            self.send_header("Content-Range", f"bytes {start}-{length - 1}/{length}")
        self.send_header("Content-Length", str(length - start))
        self.send_header("Connection", "close")
        self.end_headers()
        limit = length - start
        if not range_header and cut_pending.is_set():
            cut_pending.clear()
            limit = min(limit, args.cut_first)
        sent = 0
        chunk = 64 * 1024
        try:
            while sent < limit:
                piece = body[start + sent:start + min(sent + chunk, limit)]
                self.wfile.write(piece)
                self.wfile.flush()
                sent += len(piece)
                if args.rate:
                    time.sleep(len(piece) / args.rate)
        except (BrokenPipeError, ConnectionResetError):
            # The client went away mid-transfer, as the `restart` scenario's
            # daemon does on SIGTERM; the log line below records what was sent.
            pass
        self.log(range_header, status, sent)
        self.close_connection = True


server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
print(f"serving {args.dir} on 127.0.0.1:{args.port}", flush=True)
try:
    server.serve_forever()
except KeyboardInterrupt:
    sys.exit(0)
