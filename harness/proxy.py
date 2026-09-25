#!/usr/bin/env python3
"""A logging reverse proxy, standing between Sonarr/Radarr and braid-server.

Point a download client at this host instead of braid-server directly, and
every request it makes and every response it gets back is written to
/harness/recordings before being forwarded on unchanged. Nothing here alters
the request or the response: the whole point is to capture what a real
client sends and what our server actually answers, not a version of either
adjusted for the recording.

Stdlib only, on purpose. This never ships and never runs in CI, so pulling in
a real HTTP library would just be one more thing to keep working in an image
nobody builds against a pinned version.
"""

import http.server
import http.client
import json
import os
import re
import socketserver
import threading
import time

UPSTREAM_HOST = os.environ.get("UPSTREAM_HOST", "braid")
UPSTREAM_PORT = int(os.environ.get("UPSTREAM_PORT", "8080"))
RECORDINGS_DIR = os.environ.get("RECORDINGS_DIR", "/harness/recordings")
LISTEN_PORT = int(os.environ.get("LISTEN_PORT", "8080"))

os.makedirs(RECORDINGS_DIR, exist_ok=True)

_counter_lock = threading.Lock()
_counter = 0


def _next_seq():
    global _counter
    with _counter_lock:
        _counter += 1
        return _counter


def _slug(path):
    slug = re.sub(r"[^A-Za-z0-9]+", "-", path).strip("-")
    return slug[:60] or "root"


def _decode_body(headers, body):
    """Text where the content type says it is text, base64 otherwise.

    A .torrent upload is binary and would not survive being treated as
    UTF-8; a form body or a JSON body is exactly the thing a fixture needs to
    read at a glance, so it is kept as plain text rather than encoded away.
    """
    content_type = headers.get("Content-Type", "")
    if not body:
        return {"encoding": "none", "text": ""}
    is_text = any(
        marker in content_type
        for marker in ("urlencoded", "json", "text", "xml")
    ) or content_type == ""
    if is_text:
        try:
            return {"encoding": "text", "text": body.decode("utf-8")}
        except UnicodeDecodeError:
            pass
    import base64

    return {"encoding": "base64", "text": base64.b64encode(body).decode("ascii")}


class ProxyHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        # The recording file is the log that matters here; the default
        # stderr line per request just doubles it in the compose output.
        pass

    def _handle(self):
        seq = _next_seq()
        started = time.time()
        length = int(self.headers.get("Content-Length", 0))
        request_body = self.rfile.read(length) if length else b""

        request_headers = {k: v for k, v in self.headers.items()}

        conn = http.client.HTTPConnection(UPSTREAM_HOST, UPSTREAM_PORT, timeout=30)
        try:
            forward_headers = dict(request_headers)
            # http.client sets its own Host and Content-Length; carrying the
            # client's copies forward would risk them disagreeing.
            forward_headers.pop("Host", None)
            forward_headers.pop("Content-Length", None)
            conn.request(
                self.command,
                self.path,
                body=request_body if request_body else None,
                headers=forward_headers,
            )
            response = conn.getresponse()
            response_body = response.read()
            response_headers = {k: v for k, v in response.getheaders()}
            status = response.status
        except OSError as exc:
            status = 502
            response_headers = {}
            response_body = str(exc).encode("utf-8")
        finally:
            conn.close()

        record = {
            "seq": seq,
            "elapsed_ms": round((time.time() - started) * 1000, 1),
            "request": {
                "method": self.command,
                "path": self.path,
                "headers": request_headers,
                "body": _decode_body(self.headers, request_body),
            },
            "response": {
                "status": status,
                "headers": response_headers,
                "body": _decode_body(response_headers, response_body),
            },
        }
        out_name = f"{seq:05d}-{self.command}-{_slug(self.path.split('?')[0])}.json"
        with open(os.path.join(RECORDINGS_DIR, out_name), "w", encoding="utf-8") as fh:
            json.dump(record, fh, indent=2, sort_keys=True)
            fh.write("\n")

        self.send_response(status)
        for key, value in response_headers.items():
            if key.lower() in ("content-length", "transfer-encoding", "connection"):
                continue
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(response_body)))
        self.end_headers()
        if response_body:
            self.wfile.write(response_body)

    def do_GET(self):
        self._handle()

    def do_POST(self):
        self._handle()

    def do_PUT(self):
        self._handle()

    def do_DELETE(self):
        self._handle()

    def do_PATCH(self):
        self._handle()

    def do_HEAD(self):
        self._handle()


class ThreadingHTTPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


if __name__ == "__main__":
    server = ThreadingHTTPServer(("0.0.0.0", LISTEN_PORT), ProxyHandler)
    print(
        f"recording proxy listening on :{LISTEN_PORT}, "
        f"forwarding to {UPSTREAM_HOST}:{UPSTREAM_PORT}, "
        f"writing to {RECORDINGS_DIR}",
        flush=True,
    )
    server.serve_forever()
